//! Worktree change notification (docs/phase-5-implementation-plan.md §6) —
//! a HOST-ONLY capability. Unlike the review store's `watch` (which has a
//! digest-poll fallback for a WSL repo with no host connection, since
//! `dv/reviews` is small enough to poll cheaply once a second), there is no
//! sane fallback for "watch an entire, possibly huge, worktree": polling it
//! the same way would mean re-`ls -la`-ing every tracked directory once a
//! second, which is exactly the kind of per-command `wsl.exe` overhead this
//! whole phase exists to eliminate. So [`watch_worktree`] is `None` for
//! every location except "Wsl, with host routing enabled" — and when it
//! does return a watcher, the supervised subscription only ever delivers
//! events while a live, `watch`-capable `dv-host` connection exists
//! (silence in between, resubscribe when one comes back). Callers must
//! treat both `None` and a silent watcher as "no live worktree watching
//! right now" (staleness/file-list refresh keeps its pre-S4 behavior: it
//! only ever updates when something else already reloads it), never as an
//! error.

use std::sync::Arc;

use crate::location::RepoLocation;

use super::manager;
use super::supervisor::{self, RemoteWatchSupervisor, SupervisorConfig};

/// Keeps a supervised worktree watch alive; dropping it stops event
/// delivery and tears the subscription down on the supervisor's thread
/// (best-effort, never blocking the dropper — see
/// [`crate::remote::supervisor::RemoteWatchSupervisor`]).
pub struct WorktreeWatcher {
    _supervisor: RemoteWatchSupervisor,
}

/// Subscribe to worktree changes for `location`: `on_change` (invoked on a
/// background thread — the host client's watch-dispatch thread for real
/// events, the supervisor's thread for synthetic re-arm events) fires
/// whenever anything under the repo root changes, `.git` excluded
/// (`crates/host/src/watch.rs` filters both `<root>/.git` and, for a
/// linked worktree, the real gitdir it points to — plan §6).
///
/// Returns quickly on every path: subscription setup (which can involve a
/// host spawn or a slow RPC) happens on the supervisor's thread, never the
/// caller's. If the host connection dies mid-session, the supervisor
/// resubscribes on the respawned host and fires one synthetic `on_change`
/// so the consumer revalidates whatever it missed during the outage
/// (backlog: "Remote watchers — resubscribe-on-respawn"). `None` only when
/// this location can never have a worktree watch at all: a `Local` repo
/// (WSL-only capability — see the module doc) or host routing disabled
/// for the process (`DV_NO_HOST=1`, or a headless CLI run that never
/// called `enable_hosts`). Callers (`crates/app/src/workspace.rs`) must
/// treat `None` as "nothing to watch this session," not a failure.
pub fn watch_worktree(
    location: RepoLocation,
    on_change: Box<dyn Fn() + Send + Sync>,
) -> Option<WorktreeWatcher> {
    let RepoLocation::Wsl { distro, path } = &location else {
        return None;
    };
    if !manager::hosts_enabled() {
        return None;
    }
    let distro = distro.clone();
    let supervisor = supervisor::spawn(SupervisorConfig {
        label: location.display_name(),
        root: path.clone(),
        kind: "worktree",
        on_change: Arc::from(on_change),
        // `distro_running`-gated for the same reason as the store watcher's
        // source (capstone P2-D): a per-tick `client_for` retry can spawn a
        // host, and spawning boots a stopped distro — a background watcher
        // must idle (cheap throttled probe only) until the user starts the
        // distro again, then recover on its own.
        client_source: Box::new(move || {
            if !manager::distro_running(&distro) {
                return None;
            }
            manager::client_for(&distro)
                .filter(|client| client.is_alive() && client.has_cap("watch"))
        }),
        // No fallback exists for a whole worktree (module doc) — the
        // supervisor just idles between host-reacquisition attempts.
        fallback_tick: None,
        // No synthetic on the FIRST subscribe: the sole consumer computed
        // its diff immediately before arming this, and an unconditional
        // extra revalidate per repo-open would be waste. Re-arms after an
        // outage always fire one.
        synthetic_on_first_arm: false,
        poll_interval: supervisor::DEFAULT_POLL_INTERVAL,
    });
    Some(WorktreeWatcher {
        _supervisor: supervisor,
    })
}
