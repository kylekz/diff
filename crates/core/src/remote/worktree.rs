//! Worktree change notification (docs/phase-5-implementation-plan.md §6) —
//! a HOST-ONLY capability. Unlike the review store's `watch` (which has a
//! digest-poll fallback for a WSL repo with no host connection, since
//! `dv/reviews` is small enough to poll cheaply once a second), there is no
//! sane fallback for "watch an entire, possibly huge, worktree": polling it
//! the same way would mean re-`ls -la`-ing every tracked directory once a
//! second, which is exactly the kind of per-command `wsl.exe` overhead this
//! whole phase exists to eliminate. So [`watch_worktree`] is simply `None`
//! for every location except "Wsl, with a live host connection that
//! advertises the `watch` cap" — callers must treat `None` as "no live
//! worktree watching this session" (staleness/file-list refresh keeps its
//! pre-S4 behavior: it only ever updates when something else already
//! reloads it), never as an error.

use std::sync::Arc;

use crate::location::RepoLocation;

use super::client::HostClient;
use super::manager;

/// Keeps a worktree watch alive; dropping it unsubscribes (best-effort —
/// see [`HostClient::watch_unsubscribe`]'s doc for why errors here are
/// deliberately swallowed).
pub struct WorktreeWatcher {
    client: Arc<HostClient>,
    watch_id: u64,
}

impl Drop for WorktreeWatcher {
    fn drop(&mut self) {
        self.client.unregister_watch_callback(self.watch_id);
        let _ = self.client.watch_unsubscribe(self.watch_id);
    }
}

/// Subscribe to worktree changes for `location`: `on_change` (invoked on
/// the host client's dedicated watch-dispatch thread — see
/// [`HostClient::register_watch_callback`]) fires whenever anything under
/// the repo root changes, `.git` excluded (`crates/host/src/watch.rs`
/// filters both `<root>/.git` and, for a linked worktree, the real gitdir
/// it points to — plan §6). `None` when no live, `watch`-capable host
/// connection exists for this location: a `Local` repo (this is a WSL-only
/// capability — see the module doc), hosts disabled/`DV_NO_HOST=1`, the
/// distro cooling down or dead, or a host binary too old to list `"watch"`
/// among its `caps`. Callers (`crates/app/src/workspace.rs`) must treat
/// `None` as "nothing to watch this session," not a failure.
pub fn watch_worktree(
    location: RepoLocation,
    on_change: Box<dyn Fn() + Send + Sync>,
) -> Option<WorktreeWatcher> {
    let RepoLocation::Wsl { distro, path } = &location else {
        return None;
    };
    let client = manager::client_for(distro)?;
    if !client.has_cap("watch") {
        return None;
    }
    match client.watch_subscribe(path, "worktree") {
        Ok(watch_id) => {
            client.register_watch_callback(watch_id, move |_params| on_change());
            Some(WorktreeWatcher { client, watch_id })
        }
        Err(err) => {
            eprintln!(
                "worktree watch unavailable for {} (continuing without it): {err:#}",
                location.display_name()
            );
            None
        }
    }
}
