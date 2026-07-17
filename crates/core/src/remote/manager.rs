//! Per-distro `dv-host` connection registry (docs/phase-5-implementation-plan.md
//! §3-4). [`enable_hosts`] is called exactly once, only from the GUI
//! startup path (`crates/app/src/main.rs`) — the headless CLI dispatch
//! (`dv review`/`dv comment`/`dv pr <list|view|create|fetch>`) never calls
//! it, so those stay on today's `wsl.exe`-per-command behavior byte for
//! byte. Until then, and whenever `DV_NO_HOST=1` is set, [`client_for`]
//! always returns `None` and every caller falls back to `Route::Spawn`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::client::{self, HostClient};
use super::install;

/// Default per-distro cool-down between a dead/failed host and the next
/// respawn attempt — avoids hammering `wsl.exe` in a spawn-fail loop.
/// Overridable via `DV_HOST_COOLDOWN_MS` (read fresh on every call, never
/// cached) so the WSL integration test can shrink it instead of sleeping
/// 30 real seconds (plan §8 S2: "make cool-down configurable via env or a
/// test hook").
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);

static ENABLED: AtomicBool = AtomicBool::new(false);
static FALLBACK_COUNT: AtomicU64 = AtomicU64::new(0);

/// One distro's connection history.
enum HostEntry {
    Alive(Arc<HostClient>),
    /// The reader thread saw the connection go away (EOF / crash).
    Dead {
        since: Instant,
    },
    /// `HostClient::spawn_wsl` itself failed (WSL absent, distro deleted,
    /// no `dv-host` at `DV_HOST_PATH`, proto mismatch, ...).
    Failed {
        since: Instant,
        /// The installed binary's content hash at the point a
        /// reinstall-then-respawn STILL proto-mismatched — i.e. the skew is
        /// between this dv build and its own sidecar, not fixable by
        /// re-streaming. Later rounds compare against it to skip the ~9MB
        /// reinstall while the sidecar is unchanged (S3 review P3); a new
        /// sidecar (dv upgraded) hashes differently and resets the cycle.
        skew_hash: Option<String>,
    },
}

/// `distro -> Arc<Mutex<Option<HostEntry>>>` (`None` = never attempted, no
/// history yet). The OUTER map mutex is held only long enough to
/// get-or-insert the per-distro slot; the actual spawn attempt (which can
/// take several seconds on a cold distro boot) runs with only the
/// PER-DISTRO mutex held. That's the whole no-thundering-herd design:
/// concurrent `client_for` calls for DIFFERENT distros never block each
/// other (different slots), and concurrent calls for the SAME distro
/// serialize — the second caller blocks on the first's spawn/reuse instead
/// of racing it to spawn a second `wsl.exe -d <distro> --exec dv-host`.
type Slot = Arc<Mutex<Option<HostEntry>>>;
type Registry = Mutex<HashMap<String, Slot>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Enable host routing for the rest of this process's lifetime. Idempotent
/// (repeat calls are a no-op); call exactly once, from the GUI startup path
/// only. `dv review`/`dv comment`/`dv pr <list|view|create|fetch>` must
/// never call this — see `crates/app/src/main.rs`'s headless dispatch,
/// which returns before `run_gui` (and this) is ever reached.
pub fn enable_hosts() {
    ENABLED.store(true, Ordering::SeqCst);
}

fn cooldown() -> Duration {
    std::env::var("DV_HOST_COOLDOWN_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_COOLDOWN)
}

/// `DV_NO_HOST=1` is the hard A/B lever (plan §3): force-disables host
/// routing even after [`enable_hosts`] ran, so a single env var reproduces
/// Stage-A behavior for comparison runs without a separate build.
/// `pub(crate)` for [`super::worktree::watch_worktree`]'s cheap "can this
/// process ever have a host at all" gate — no supervisor thread is worth
/// spawning when the answer is a hard no.
pub(crate) fn hosts_enabled() -> bool {
    hosts_enabled_given(
        std::env::var("DV_NO_HOST").ok().as_deref(),
        ENABLED.load(Ordering::SeqCst),
    )
}

/// Pure core of [`hosts_enabled`] — factored out so the
/// enabled/disabled/env matrix is unit-testable without touching real
/// process-global env state (which races under the parallel test harness).
fn hosts_enabled_given(no_host_env: Option<&str>, enabled_flag: bool) -> bool {
    if no_host_env == Some("1") {
        return false;
    }
    enabled_flag
}

/// `DV_HOST_PATH` read directly — the dev-loop override. As of S3,
/// [`client_for`]'s own spawn path no longer calls this: it always goes
/// through [`install::ensure_installed`], which checks the very same env
/// var itself (first, before ever touching a sidecar) and returns it
/// straight through. This function survives only as
/// [`should_count_spawn_fallback`]'s "is *anything* configured" probe for
/// [`note_spawn_fallback`]'s gate — see that function's doc for why it
/// still only recognizes the env var and not sidecar-based configuration.
fn host_path() -> Option<String> {
    host_path_given(std::env::var("DV_HOST_PATH").ok())
}

fn host_path_given(env_val: Option<String>) -> Option<String> {
    env_val.filter(|v| !v.is_empty())
}

/// The state [`decide_respawn`] reasons over — a distro-agnostic summary of
/// a [`HostEntry`], with the `Alive` case already resolved to "is the
/// client actually still alive" by the caller (so this pure function never
/// needs to touch a real `HostClient`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Alive,
    Dead(Instant),
    Failed(Instant),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RespawnDecision {
    /// Reuse the existing live client.
    Reuse,
    /// No history, or cool-down has elapsed: attempt a fresh spawn now.
    Spawn,
    /// A recent attempt failed/died and cool-down hasn't elapsed yet.
    CoolingDown,
}

/// Pure cool-down state machine (plan §8 S2: "manager cool-down state
/// machine (pure fn over Instants)") — unit-testable without spawning
/// anything or touching the real registry/clock.
pub(crate) fn decide_respawn(
    state: Option<EntryKind>,
    now: Instant,
    cooldown: Duration,
) -> RespawnDecision {
    match state {
        None => RespawnDecision::Spawn,
        Some(EntryKind::Alive) => RespawnDecision::Reuse,
        Some(EntryKind::Dead(since)) | Some(EntryKind::Failed(since)) => {
            if now.saturating_duration_since(since) >= cooldown {
                RespawnDecision::Spawn
            } else {
                RespawnDecision::CoolingDown
            }
        }
    }
}

/// A live host client for `distro` ONLY if one is already connected and
/// alive in the registry RIGHT NOW — never spawns, installs, or boots a
/// distro (unlike [`client_for`]). The badge walk uses this so app launch
/// never boots a stopped distro just to compute a badge (plan §4).
pub(crate) fn client_if_running(distro: &str) -> Option<Arc<HostClient>> {
    if !hosts_enabled() {
        return None;
    }
    let slot = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(distro)
        .cloned()?;
    // `try_lock`, not `lock`: if the slot is mid-spawn (`client_for` holds it
    // across a possibly-multi-second cold boot), treat it as "not running
    // yet" and skip rather than blocking the badge walk on that boot.
    let guard = slot.try_lock().ok()?;
    match &*guard {
        Some(HostEntry::Alive(client)) if client.is_alive() => Some(Arc::clone(client)),
        _ => None,
    }
}

/// Whether a live `dv-host` connection already exists for `distro` (never
/// spawns/boots — see [`client_if_running`]). The app's badge walk gates WSL
/// entries on this so launch boots no stopped distros.
pub fn has_running_host(distro: &str) -> bool {
    client_if_running(distro).is_some()
}

/// Distros with a background warm-up already kicked off and not yet
/// resolved (spawned OR failed) — de-dupes [`warm_up_in_background`] so a
/// burst of `GitRepo::open` calls for the same distro in quick succession
/// (several files/reviews opened before the first spawn attempt finishes)
/// doesn't pile up one `std::thread::spawn` per call. An entry is removed
/// the moment its underlying [`client_for`] attempt resolves, so a later
/// genuine retry (e.g. after a cool-down, or a second distro's repo opened
/// afterward) still fires.
fn warming_registry() -> &'static Mutex<std::collections::HashSet<String>> {
    static WARMING: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    WARMING.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Kick off [`client_for`]'s (possibly multi-second, cold-distro-boot-
/// including) install+spawn+handshake on a background thread instead of the
/// caller's — the mechanism behind "opening a WSL repo should not block on
/// host spawn" (docs/backlog.md "Cold startup is tree-sitter-bound... (b)
/// the WSL host is spawned on the cold-start critical path"). Meant to be
/// called exactly once per explicit repo-open (`GitRepo::open`, for a `Wsl`
/// location) — genuine user intent, same distinction
/// [`client_if_running`]'s own doc draws against the badge walk, which must
/// never boot a distro on its own and stays on `client_if_running`/
/// [`has_running_host`] alone (this function is never called from there).
///
/// A no-op when hosts are disabled, a client is already alive (nothing to
/// warm), or a warm-up for this distro is already in flight. Otherwise
/// spawns a thread that calls the ordinary blocking [`client_for`] purely
/// for its side effect of populating the registry — its return value is
/// discarded; `client_for`'s own `Alive`/`Dead`/`Failed` bookkeeping already
/// logs failures. Every OTHER path that eventually wants this distro's host
/// (a fresh [`crate::command::CommandBuilder::new`] for a later-opened
/// repo, or the watch supervisor's own retry loop — see
/// `crate::remote::supervisor`'s module doc) picks up whatever this
/// produces the ordinary way, with no direct coupling to this function.
pub(crate) fn warm_up_in_background(distro: &str) {
    if !hosts_enabled() || client_if_running(distro).is_some() {
        return;
    }
    {
        let mut warming = warming_registry().lock().unwrap_or_else(|e| e.into_inner());
        if !warming.insert(distro.to_string()) {
            // Already warming — the in-flight attempt's own `client_for`
            // call serializes on the per-distro slot lock (see that
            // function's doc), so a second thread here would just block
            // behind the first for no benefit.
            return;
        }
    }
    let distro = distro.to_string();
    std::thread::spawn(move || {
        client_for(&distro);
        warming_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&distro);
    });
}

/// A connected host client for `distro`, or `None` if hosts are disabled, no
/// `dv-host` binary is configured/installable (no `DV_HOST_PATH`, no
/// sidecar — see [`install::ensure_installed`]'s [`install::InstallError::NoSidecar`]),
/// the distro is cooling down after a recent failure, or a fresh spawn
/// attempt itself fails. Callers (`CommandBuilder::new`) fall back to
/// `Route::Spawn` on `None` — this function's whole contract is "never
/// worse than not having a host".
pub(crate) fn client_for(distro: &str) -> Option<Arc<HostClient>> {
    if !hosts_enabled() {
        return None;
    }

    let slot: Slot = {
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(distro.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };

    // Holding this per-distro lock across the (possibly multi-second) spawn
    // — which, as of S3, now includes `install::ensure_installed`'s own WSL
    // round trips (resolving $HOME, checking the marker, streaming the
    // binary) — is the point of the two-tier locking: a second concurrent
    // caller for the SAME distro blocks here and then reuses whatever the
    // first caller produced, instead of racing it to install/spawn a second
    // `dv-host`. `install::ensure_installed` MUST route every subprocess
    // through `CommandBuilder::new_spawn_only` rather than `::new` for
    // exactly this reason — `::new` would re-enter this function for the
    // same distro and deadlock on the very lock held right here.
    //
    // FIXED (S3 review P2, was a KNOWN LIMIT, closed by S5 robustness): the
    // install round trips run under this lock, so an unbounded wedged
    // `wsl.exe` (stuck distro boot) would otherwise block every future
    // `CommandBuilder::new` for THIS distro indefinitely (other distros are
    // unaffected either way; the outer registry lock is never held here).
    // As of S5, every bootstrap command in `install.rs` runs through
    // `CommandBuilder::run_timeout`/`run_text_timeout`/`run_with_stdin_timeout`
    // (`INSTALL_COMMAND_TIMEOUT`, 60s) instead of the unbounded `run`/
    // `run_text`/`run_with_stdin` — a genuine wedge now surfaces as an
    // `InstallError::Io` here, which the match below turns into `Failed` +
    // cool-down like any other spawn failure, instead of a lock pile-up.
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();

    // Resolve `Alive` to a real liveness check up front; a client whose
    // reader thread already saw EOF is downgraded to `Dead` right here so
    // `decide_respawn` always sees an accurate state.
    if let Some(HostEntry::Alive(client)) = &*guard
        && !client.is_alive()
    {
        *guard = Some(HostEntry::Dead { since: now });
    }

    let state = match &*guard {
        None => None,
        Some(HostEntry::Alive(_)) => Some(EntryKind::Alive),
        Some(HostEntry::Dead { since }) => Some(EntryKind::Dead(*since)),
        Some(HostEntry::Failed { since, .. }) => Some(EntryKind::Failed(*since)),
    };
    let prior_skew = match &*guard {
        Some(HostEntry::Failed { skew_hash, .. }) => skew_hash.clone(),
        _ => None,
    };

    match decide_respawn(state, now, cooldown()) {
        RespawnDecision::Reuse => match &*guard {
            Some(HostEntry::Alive(client)) => Some(Arc::clone(client)),
            _ => unreachable!("decide_respawn only returns Reuse for EntryKind::Alive"),
        },
        RespawnDecision::CoolingDown => None,
        RespawnDecision::Spawn => {
            // `ensure_installed` itself checks `DV_HOST_PATH` first (dev
            // loop: skips sidecar/hash/install entirely, same as S1/S2's
            // behavior) before ever falling to the sidecar install flow.
            // `ensure_installed_spec` rather than the `ensure_installed`
            // wrapper: the wrapper drops the content hash, which the
            // persistent-skew check below needs.
            let binary = match install::ensure_installed_spec(distro, &install::HOST_SPEC) {
                Ok(binary) => binary,
                // Unconfigured (no env var, no sidecar next to dv.exe) —
                // silent, no `Failed` entry, so a sidecar that appears
                // later (or a `DV_HOST_PATH` set later) works on the very
                // next call instead of waiting out a stale cool-down.
                Err(install::InstallError::NoSidecar { .. }) => return None,
                Err(err) => {
                    eprintln!("[dv-host manager] failed to install dv-host for {distro}: {err}");
                    *guard = Some(HostEntry::Failed {
                        since: now,
                        skew_hash: None,
                    });
                    return None;
                }
            };

            match HostClient::spawn_wsl(distro, &binary.path) {
                Ok(client) => {
                    let client = Arc::new(client);
                    *guard = Some(HostEntry::Alive(Arc::clone(&client)));
                    Some(client)
                }
                // Proto mismatch against an install-managed binary (plan
                // §2): force exactly one reinstall (delete the marker so
                // `ensure_installed` can't just see a stale-but-matching
                // hash and skip the reinstall) and respawn. A `DevOverride`
                // path never reaches this arm — there's no marker to
                // invalidate for an arbitrary dev-supplied binary, so it
                // falls straight to the generic failure arm below instead.
                Err(err)
                    if binary.source == install::HostBinarySource::Managed
                        && client::is_proto_mismatch(&err) =>
                {
                    // Persistent-skew short circuit (S3 review P3): a prior
                    // round already reinstalled EXACTLY these bytes and the
                    // handshake still mismatched — the skew is between this
                    // dv build and its own sidecar, and re-streaming ~9MB
                    // every cool-down expiry can't fix it. Stay Failed
                    // until the sidecar's content actually changes (dv
                    // upgrade), which hashes differently and resets this.
                    if !binary.hash.is_empty()
                        && prior_skew.as_deref() == Some(binary.hash.as_str())
                    {
                        eprintln!(
                            "[dv-host manager] proto mismatch for {distro} persists with an \
                             unchanged sidecar (hash {}); skipping reinstall — update dv",
                            &binary.hash[..12.min(binary.hash.len())]
                        );
                        *guard = Some(HostEntry::Failed {
                            since: now,
                            skew_hash: prior_skew,
                        });
                        return None;
                    }
                    eprintln!(
                        "[dv-host manager] proto mismatch for {distro} ({err:#}); forcing one reinstall"
                    );
                    let reinstalled = match install::force_reinstall_spec(
                        distro,
                        &install::HOST_SPEC,
                    ) {
                        Ok(binary) => binary,
                        Err(reinstall_err) => {
                            eprintln!(
                                "[dv-host manager] reinstall failed for {distro}: {reinstall_err}"
                            );
                            *guard = Some(HostEntry::Failed {
                                since: now,
                                skew_hash: None,
                            });
                            return None;
                        }
                    };
                    match HostClient::spawn_wsl(distro, &reinstalled.path) {
                        Ok(client) => {
                            let client = Arc::new(client);
                            *guard = Some(HostEntry::Alive(Arc::clone(&client)));
                            Some(client)
                        }
                        Err(respawn_err) => {
                            eprintln!(
                                "[dv-host manager] still failing for {distro} after reinstall: {respawn_err:#}"
                            );
                            // A REPEATED proto mismatch right after a
                            // verified fresh reinstall is the inherent-skew
                            // signal — remember the hash so the next round
                            // skips the pointless re-stream. Any other
                            // respawn failure keeps the ordinary retry.
                            let skew_hash = (client::is_proto_mismatch(&respawn_err))
                                .then(|| reinstalled.hash.clone());
                            *guard = Some(HostEntry::Failed {
                                since: now,
                                skew_hash,
                            });
                            None
                        }
                    }
                }
                Err(err) => {
                    eprintln!("[dv-host manager] failed to spawn host for {distro}: {err:#}");
                    *guard = Some(HostEntry::Failed {
                        since: now,
                        skew_hash: None,
                    });
                    None
                }
            }
        }
    }
}

/// Whether [`mark_dead`] should downgrade the currently-registered entry to
/// `Dead` — factored out as a pure decision over already-resolved facts
/// (plan §8 S2 review finding P2-1) so the identity/trigger rule is
/// unit-testable without a real `HostClient`/registry:
///
///   - `current` must be `Some(EntryKind::Alive)` — a `Dead`/`Failed`
///     cool-down already ticking, or no entry at all, is left alone
///     (idempotent: calling this twice for the same failure is a no-op the
///     second time).
///   - `same_instance` must be `true` — the failing client must be the
///     SAME `Arc<HostClient>` currently registered as alive
///     (`Arc::ptr_eq`, checked by the caller). Without this, a stale
///     client a long-lived `GitRepo` still holds could downgrade a FRESH,
///     healthy client that already replaced it in the registry — the
///     spawn/kill churn loop this finding fixes.
pub(crate) fn should_mark_dead(current: Option<EntryKind>, same_instance: bool) -> bool {
    matches!(current, Some(EntryKind::Alive)) && same_instance
}

/// Mark `distro`'s entry dead RIGHT NOW — called by `CommandBuilder`'s Host
/// arm (and `GitRepo::batch_request`'s) the moment a CONNECTION-level
/// failure (see [`super::client::RequestFailure::is_connection`] — never a
/// `Timeout` or an `Rpc` error the host successfully answered with) hits an
/// already-vended client, rather than waiting for some later, unrelated
/// `client_for` call to notice via `is_alive()`.
///
/// This matters because a single `CommandBuilder`/`GitRepo` caches its
/// `Route` for its whole lifetime (plan §3: decided once, in `new`) — it
/// never calls `client_for` again itself, so without this, the cool-down
/// clock wouldn't start until some OTHER, unrelated caller happened to
/// construct a fresh `CommandBuilder` for the same distro.
///
/// `failing` must be the SAME client instance the caller's failed request
/// actually ran against — see [`should_mark_dead`] for why: a caller
/// holding a stale `Arc<HostClient>` from before a respawn must never be
/// able to downgrade the fresh client that replaced it.
pub(crate) fn mark_dead(distro: &str, failing: &Arc<HostClient>) {
    let slot: Slot = {
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(distro.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    let (current, same_instance) = match &*guard {
        Some(HostEntry::Alive(registered)) => {
            (Some(EntryKind::Alive), Arc::ptr_eq(registered, failing))
        }
        Some(HostEntry::Dead { since }) => (Some(EntryKind::Dead(*since)), false),
        Some(HostEntry::Failed { since, .. }) => (Some(EntryKind::Failed(*since)), false),
        None => (None, false),
    };
    if should_mark_dead(current, same_instance) {
        *guard = Some(HostEntry::Dead {
            since: Instant::now(),
        });
    }
}

/// Pure core of [`note_spawn_fallback`]'s "is this actual route loss, or
/// just never-configured" gate (plan §8 S2 review finding P3-4) — factored
/// out so the decision is unit-testable without touching real env vars or
/// the process-global counter. Only `true` (WSL location, hosts enabled,
/// AND a host path configured) means "count and log this"; hosts enabled
/// with no `host_path` is the "every dev session until S3" unconfigured
/// state, not route loss, and must stay silent.
///
/// Known S3 gap, left as-is deliberately: `host_path` here still only ever
/// reflects `DV_HOST_PATH` (see [`host_path`]), not "a sidecar successfully
/// resolved through `install::ensure_installed`". In the now-normal S3
/// world (no `DV_HOST_PATH`, a sidecar auto-installs instead), a
/// [`HostEntry::Failed`] distro from a persistently broken install would
/// stay silent here forever instead of counting as route loss. Widening
/// this gate to "sidecar present OR env var set" is a reasonable follow-up,
/// but it's outside this slice's explicit wiring list and this function's
/// existing unit tests pin the current (env-var-only) contract — revisit
/// alongside S4/S5 if the silent-Failed-loop turns out to matter in
/// practice.
fn should_count_spawn_fallback(
    is_wsl_location: bool,
    enabled: bool,
    host_path: Option<&str>,
) -> bool {
    is_wsl_location && enabled && host_path.is_some()
}

/// Cheap production probe (plan §8 S2 gate): the WSL integration test
/// asserts this counter stays at 0 across a whole routed session with a
/// live host, and watches it increment exactly when a host request fails
/// mid-session and this call transparently fell back to `wsl.exe`.
///
/// Silently ignores the "hosts enabled but no `DV_HOST_PATH` configured"
/// state — every dev session until S3 ships an installer/sidecar, since
/// [`host_path`] is the only source for S2. Without
/// [`should_count_spawn_fallback`]'s gate, EVERY `CommandBuilder::run` call
/// for a Wsl location would bump this counter and (in debug builds)
/// `eprintln!` once per command, for the whole session, for something
/// that's expected and permanent rather than a failure. Only genuine route
/// loss — a dead/failed host entry, or a mid-call fallback after a
/// previously-live connection failed — counts and logs.
pub fn note_spawn_fallback(location: &crate::location::RepoLocation, reason: &str) {
    let is_wsl_location = matches!(location, crate::location::RepoLocation::Wsl { .. });
    if !should_count_spawn_fallback(is_wsl_location, hosts_enabled(), host_path().as_deref()) {
        return;
    }
    let count = FALLBACK_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
    if cfg!(debug_assertions) || std::env::var("DV_HOST_DEBUG").as_deref() == Ok("1") {
        eprintln!(
            "[dv-host manager] wsl.exe spawn fallback #{count} for {}: {reason}",
            location.display_name()
        );
    }
}

/// Total count of [`note_spawn_fallback`] calls so far — exposed for the
/// WSL integration test's "zero fallbacks during a healthy session" and
/// "at least one fallback after killing the host" assertions.
pub fn spawn_fallback_count() -> u64 {
    FALLBACK_COUNT.load(Ordering::SeqCst)
}

/// The last chunk of a host's stderr (a crash/panic message, typically),
/// suitable to append to a connection-loss log. Empty when the ring is
/// blank; otherwise the trailing ~800 chars on a char boundary, prefixed.
fn stderr_tail(client: &Arc<HostClient>) -> String {
    truncate_stderr_tail(&client.stderr_snapshot())
}

/// Pure core of [`stderr_tail`] — factored out so the truncation/formatting
/// logic is unit-testable without a real `HostClient` (a plain string in,
/// string out, no process behind it). Trims surrounding whitespace first
/// (the ring commonly ends in a trailing newline from the last line the
/// host wrote, which would otherwise eat into the character budget for no
/// benefit), then keeps at most the last ~800 characters of what's left —
/// backing up to the nearest UTF-8 character boundary, since slicing a
/// `str` at a non-boundary byte offset panics (the same hazard
/// [`crate::command::truncate_lossy`] exists to guard against).
fn truncate_stderr_tail(s: &str) -> String {
    const MAX_CHARS: usize = 800;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let tail = if trimmed.len() > MAX_CHARS {
        let mut start = trimmed.len() - MAX_CHARS;
        while start < trimmed.len() && !trimmed.is_char_boundary(start) {
            start += 1;
        }
        &trimmed[start..]
    } else {
        trimmed
    };
    format!("; host stderr: {tail}")
}

/// A vended host client's request just failed at the CONNECTION level (the
/// channel is dead — see [`RequestFailure::is_connection`][super::client::RequestFailure::is_connection]).
/// Logs the fallback (with the host's stderr tail for crash context) and,
/// for a `Wsl` location, starts the distro's cool-down via [`mark_dead`].
/// Centralizes what `CommandBuilder` and `GitRepo::batch_request` both do
/// on connection loss.
pub(crate) fn note_host_connection_lost(
    location: &crate::location::RepoLocation,
    client: &Arc<HostClient>,
    reason: &str,
) {
    note_spawn_fallback(location, &format!("{reason}{}", stderr_tail(client)));
    if let crate::location::RepoLocation::Wsl { distro, .. } = location {
        mark_dead(distro, client);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- hosts_enabled_given: pure enabled/disabled/env matrix ----------

    #[test]
    fn hosts_enabled_given_dv_no_host_always_wins() {
        assert!(!hosts_enabled_given(Some("1"), true));
        assert!(!hosts_enabled_given(Some("1"), false));
    }

    #[test]
    fn hosts_enabled_given_otherwise_follows_the_flag() {
        assert!(hosts_enabled_given(None, true));
        assert!(!hosts_enabled_given(None, false));
        // Only the exact string "1" disables — anything else is not the
        // documented lever and shouldn't silently do the same thing.
        assert!(hosts_enabled_given(Some("0"), true));
        assert!(hosts_enabled_given(Some(""), true));
    }

    // --- host_path_given: empty string treated as unset -----------------

    #[test]
    fn host_path_given_treats_absent_and_empty_as_none() {
        assert_eq!(host_path_given(None), None);
        assert_eq!(host_path_given(Some(String::new())), None);
    }

    #[test]
    fn host_path_given_passes_through_a_real_value() {
        assert_eq!(
            host_path_given(Some("/opt/dv/host/dv-host".to_string())),
            Some("/opt/dv/host/dv-host".to_string())
        );
    }

    // --- decide_respawn: the cool-down state machine, pure over Instants -

    #[test]
    fn decide_respawn_no_history_spawns_immediately() {
        assert_eq!(
            decide_respawn(None, Instant::now(), Duration::from_secs(30)),
            RespawnDecision::Spawn
        );
    }

    #[test]
    fn decide_respawn_alive_reuses() {
        assert_eq!(
            decide_respawn(
                Some(EntryKind::Alive),
                Instant::now(),
                Duration::from_secs(30)
            ),
            RespawnDecision::Reuse
        );
    }

    #[test]
    fn decide_respawn_dead_within_cooldown_waits() {
        let since = Instant::now();
        let now = since + Duration::from_secs(5);
        assert_eq!(
            decide_respawn(Some(EntryKind::Dead(since)), now, Duration::from_secs(30)),
            RespawnDecision::CoolingDown
        );
    }

    #[test]
    fn decide_respawn_dead_after_cooldown_respawns() {
        let since = Instant::now();
        let now = since + Duration::from_secs(31);
        assert_eq!(
            decide_respawn(Some(EntryKind::Dead(since)), now, Duration::from_secs(30)),
            RespawnDecision::Spawn
        );
    }

    #[test]
    fn decide_respawn_failed_behaves_like_dead() {
        let since = Instant::now();
        assert_eq!(
            decide_respawn(
                Some(EntryKind::Failed(since)),
                since,
                Duration::from_secs(30)
            ),
            RespawnDecision::CoolingDown
        );
        let later = since + Duration::from_secs(30);
        assert_eq!(
            decide_respawn(
                Some(EntryKind::Failed(since)),
                later,
                Duration::from_secs(30)
            ),
            RespawnDecision::Spawn
        );
    }

    #[test]
    fn decide_respawn_cooldown_boundary_is_inclusive() {
        let since = Instant::now();
        let exactly_at_cooldown = since + Duration::from_secs(30);
        assert_eq!(
            decide_respawn(
                Some(EntryKind::Dead(since)),
                exactly_at_cooldown,
                Duration::from_secs(30)
            ),
            RespawnDecision::Spawn,
            "cool-down elapsed exactly at the boundary should allow a respawn attempt"
        );
    }

    // --- note_spawn_fallback / spawn_fallback_count: location gating -----
    //
    // These call the REAL counter (process-global, like the rest of the
    // manager's state) but only ever exercise the early-return branches
    // (Local location, or a Wsl location while hosts are disabled — which
    // is always true in this test binary since nothing here ever calls
    // the real `enable_hosts()`), so they can't perturb the count in a way
    // that affects other tests' assertions about it.

    #[test]
    fn note_spawn_fallback_ignores_local_locations() {
        let before = spawn_fallback_count();
        note_spawn_fallback(
            &crate::location::RepoLocation::Local(std::path::PathBuf::from("/x")),
            "should not count",
        );
        assert_eq!(spawn_fallback_count(), before);
    }

    #[test]
    fn note_spawn_fallback_ignores_wsl_when_hosts_disabled() {
        // `enable_hosts()` is never called anywhere in this test binary, so
        // hosts are disabled ambiently — this call must be a no-op.
        let before = spawn_fallback_count();
        note_spawn_fallback(
            &crate::location::RepoLocation::Wsl {
                distro: "Ubuntu".to_string(),
                path: "/x".to_string(),
            },
            "should not count while disabled",
        );
        assert_eq!(spawn_fallback_count(), before);
    }

    // --- should_count_spawn_fallback: unconfigured vs. actual route loss -
    // (plan §8 S2 review finding P3-4)

    #[test]
    fn should_count_spawn_fallback_requires_all_three() {
        assert!(should_count_spawn_fallback(
            true,
            true,
            Some("/opt/dv-host")
        ));
    }

    #[test]
    fn should_count_spawn_fallback_silent_when_unconfigured() {
        // Hosts enabled, WSL location, but no DV_HOST_PATH — "never
        // configured", not route loss. This is the every-dev-session-
        // until-S3 state the finding exists to keep silent.
        assert!(!should_count_spawn_fallback(true, true, None));
    }

    #[test]
    fn should_count_spawn_fallback_silent_for_local_or_disabled() {
        assert!(!should_count_spawn_fallback(
            false,
            true,
            Some("/opt/dv-host")
        ));
        assert!(!should_count_spawn_fallback(
            true,
            false,
            Some("/opt/dv-host")
        ));
    }

    // --- should_mark_dead: identity + Alive-only trigger rule ------------
    // (plan §8 S2 review finding P2-1)

    #[test]
    fn should_mark_dead_true_for_the_registered_alive_client() {
        assert!(should_mark_dead(Some(EntryKind::Alive), true));
    }

    #[test]
    fn should_mark_dead_false_for_a_stale_client_not_currently_registered() {
        // The exact spawn/kill churn loop this finding fixes: a
        // long-lived `GitRepo` still holding a stale client must not be
        // able to downgrade the FRESH client that already replaced it.
        assert!(!should_mark_dead(Some(EntryKind::Alive), false));
    }

    #[test]
    fn should_mark_dead_false_when_already_dead_or_failed() {
        // Idempotent: a cool-down already ticking (from an earlier,
        // possibly different, failure) is left alone rather than reset.
        let since = Instant::now();
        assert!(!should_mark_dead(Some(EntryKind::Dead(since)), true));
        assert!(!should_mark_dead(Some(EntryKind::Failed(since)), true));
    }

    #[test]
    fn should_mark_dead_false_with_no_entry_yet() {
        assert!(!should_mark_dead(None, true));
    }

    // --- truncate_stderr_tail: pure string truncation/formatting ---------
    // (plan §8 S5: "stderr ring surfacing")

    #[test]
    fn truncate_stderr_tail_empty_ring_is_empty_string() {
        assert_eq!(truncate_stderr_tail(""), "");
        // Whitespace-only counts as blank too — a ring that only ever saw a
        // trailing newline has no crash context worth appending.
        assert_eq!(truncate_stderr_tail("   \n\n  "), "");
    }

    #[test]
    fn truncate_stderr_tail_short_text_passes_through_prefixed() {
        assert_eq!(
            truncate_stderr_tail("thread 'main' panicked at src/main.rs:1"),
            "; host stderr: thread 'main' panicked at src/main.rs:1"
        );
    }

    #[test]
    fn truncate_stderr_tail_trims_surrounding_whitespace_first() {
        assert_eq!(
            truncate_stderr_tail("\n\n  panic: boom  \n"),
            "; host stderr: panic: boom"
        );
    }

    #[test]
    fn truncate_stderr_tail_keeps_only_the_last_800_chars() {
        let long = "a".repeat(1000);
        let result = truncate_stderr_tail(&long);
        let prefix = "; host stderr: ";
        assert!(result.starts_with(prefix));
        assert_eq!(result.len() - prefix.len(), 800);
        // It's the TAIL that's kept, not the head.
        assert!(result.ends_with(&"a".repeat(800)));
    }

    #[test]
    fn truncate_stderr_tail_backs_up_to_a_char_boundary() {
        // "中" is 3 bytes; 300 repeats is 900 bytes, so the naive
        // `len - 800` byte offset (100) lands mid-character. The result
        // must still be valid UTF-8 (i.e. not panic) and contain only whole
        // characters.
        let long = "中".repeat(300);
        let result = truncate_stderr_tail(&long);
        assert!(result.starts_with("; host stderr: "));
        let tail = result.strip_prefix("; host stderr: ").unwrap();
        assert!(tail.chars().all(|c| c == '中'));
    }

    // --- crash_then_cooldown_then_respawn: the full `client_for` path
    // against a REAL WSL host (plan §8 S5 gate: "crash-respawn cool-down
    // test") ---------------------------------------------------------------
    //
    // The pure `decide_respawn` state machine above is already exhaustively
    // unit-tested; what was missing is proof that the FULL `client_for`
    // path — spawn, observe a real crash, cool down, respawn — actually
    // honors it end to end against a real `dv-host` process. That needs
    // live WSL (`client_for` -> `install::ensure_installed` ->
    // `HostClient::spawn_wsl` has no fake seam), so this mirrors
    // `crates/core/tests/wsl_host_routing.rs`'s ignored WSL test: same
    // `DV_HOST_PATH`/`DV_HOST_COOLDOWN_MS` env setup, same
    // `enable_hosts()` call, same shortened cool-down instead of a real
    // 30s sleep.
    //
    // It lives HERE — in this module's own unit tests — rather than
    // alongside `wsl_host_routing.rs` in `crates/core/tests/`, because
    // `client_for` is `pub(crate)`: an integration test file is a separate
    // crate from `dv-core`'s own `#[cfg(test)]` code and can only see
    // `pub` items (which is exactly why that file drives everything through
    // `GitRepo`/`manager::enable_hosts`/`manager::spawn_fallback_count`
    // instead). Being IN the crate is what lets this test call `client_for`
    // directly and assert on `CoolingDown` vs. `Spawn` instead of only
    // inferring it from the fallback counter.
    //
    // Mutates real process-global state (env vars, the `manager` registry,
    // the `ENABLED` flag) exactly like that file's test — guarded by its
    // own env mutex so a manual `--ignored` run can't race another
    // env-mutating test in this binary. Never runs during the normal
    // `cargo test` gate (no `--ignored` there), so `ENABLED` being latched
    // `true` for the rest of the process by this test can never affect any
    // of this module's other (ambiently-hosts-disabled) tests.
    static CRASH_RESPAWN_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    #[ignore = "requires WSL Ubuntu with dv-host built inside it (see crates/host/tests/wsl_host.rs's module doc for the build command)"]
    fn crash_then_cooldown_then_respawn() {
        let _env_guard = CRASH_RESPAWN_ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        const DISTRO: &str = "Ubuntu";
        const COOLDOWN_MS: u64 = 800;

        // SAFETY: test-only env mutation; the mutex above serializes this
        // against any other env-mutating test in this binary that might run
        // in the same manual `--ignored` pass.
        unsafe {
            let host_path = std::env::var("DV_HOST_PATH")
                .unwrap_or_else(|_| "/home/kyle/.cache/dv-target/debug/dv-host".to_string());
            std::env::set_var("DV_HOST_PATH", host_path);
            std::env::set_var("DV_HOST_COOLDOWN_MS", COOLDOWN_MS.to_string());
        }
        enable_hosts();

        let first = client_for(DISTRO).expect("initial spawn should succeed");
        let first_pid = first.pid();

        first.kill();
        let alive_deadline = Instant::now() + Duration::from_secs(5);
        while first.is_alive() && Instant::now() < alive_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !first.is_alive(),
            "the reader thread should have observed the killed process's EOF by now"
        );

        // Immediately after the crash: `client_for` must notice the death
        // (Alive -> Dead), start the cool-down, and return `None` rather
        // than respawning right away.
        assert!(
            client_for(DISTRO).is_none(),
            "client_for must return None (cooling down) immediately after a crash"
        );

        // Sleep past the (shortened) cool-down and try again: a fresh spawn
        // attempt should succeed, with a DIFFERENT pid than the dead one —
        // proof this is a genuinely new process, not a stale handle.
        std::thread::sleep(Duration::from_millis(COOLDOWN_MS + 200));
        let second = client_for(DISTRO).expect("respawn after cool-down should succeed");
        assert_ne!(
            second.pid(),
            first_pid,
            "the respawned host must be a different process than the one that crashed"
        );

        unsafe {
            std::env::remove_var("DV_HOST_PATH");
            std::env::remove_var("DV_HOST_COOLDOWN_MS");
        }
    }
}
