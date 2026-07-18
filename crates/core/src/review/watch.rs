//! Change notification for the review store, so the GUI reflects external
//! edits (the agent CLI, another dv window) without reload. Local repos get
//! real file watching (`notify`); a WSL repo gets a supervised remote watch
//! (`crate::remote::supervisor`): real inotify through a live,
//! `watch`-capable `dv-host` connection whenever one exists, the 1s
//! digest-poll fallback whenever one doesn't (hosts disabled/`DV_NO_HOST=1`,
//! a cooling-down/dead distro, an older host binary), with automatic
//! resubscribe-on-respawn when a host connection dies mid-session.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use notify::Watcher as _;

use crate::location::RepoLocation;
use crate::remote::manager;
use crate::remote::supervisor::{self, RemoteWatchSupervisor, SupervisorConfig};

use super::io::StoreIo;

/// How often the WSL fallback polls while no host subscription is live.
/// Local watching (and the host-backed subscription) is event-driven.
const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// A fallback digest slower than this is suspected of having BOOTED the
/// distro rather than merely listing a directory on a running one
/// (`wsl.exe` against a live distro answers in well under a second; a
/// distro boot adds multiple seconds) — see the `fallback_tick` in
/// [`watch`] and `manager::note_boot_suspect`. A false positive (a
/// genuinely slow digest, e.g. a host RPC riding its timeout) costs one
/// quarantine window of paused polling, nothing more.
const BOOT_SUSPECT_THRESHOLD: Duration = Duration::from_millis(2_000);

/// Keeps the watch alive; dropping it stops callbacks (best-effort — an
/// in-flight callback may still complete).
pub enum ReviewWatcher {
    /// A local repo: a real OS file watcher (`notify`) on `.git/dv/reviews`.
    Local {
        /// Held only for its `Drop` (stops the OS watcher).
        _watcher: notify::RecommendedWatcher,
    },
    /// A WSL repo: a supervisor thread (see [`crate::remote::supervisor`])
    /// that owns the whole lifecycle off the caller's thread — host
    /// subscribe, digest-poll fallback while no host is usable, death
    /// detection via [`crate::remote::client::HostClient::on_death`], and
    /// resubscribe once [`manager`]'s cool-down lets a fresh host spawn.
    /// Every (re)arm fires one synthetic `on_change` so consumers re-check
    /// state and catch anything missed during an outage — and so `dv review
    /// wait`'s arm-before-snapshot ordering still holds now that [`watch`]
    /// returns before the subscription actually exists. Dropping the
    /// watcher never blocks: teardown (unregister + best-effort
    /// unsubscribe) runs on the supervisor's thread.
    ///
    /// This closes the S4 "permanently silent after a mid-session host
    /// death" gap (backlog: "Remote watchers — resubscribe-on-respawn +
    /// subscribe off the GUI thread"), and subsumes the old dedicated
    /// `Poll` variant — the digest poll is now the supervisor's degraded
    /// mode rather than a one-shot routing decision made at watch time.
    Remote { _supervisor: RemoteWatchSupervisor },
}

/// Start watching the reviews directory for `location`, invoking
/// `on_change` (from a background thread — `notify`'s callback thread, the
/// supervisor thread, or the host client's dedicated watch-dispatch
/// thread) whenever anything in it changes. The callback must be cheap —
/// hand off to a channel. Returns quickly on every path: the WSL arm only
/// spawns the supervisor thread; all blocking work (host spawn, subscribe
/// RPC) happens on that thread, never the caller's.
pub(super) fn watch(
    location: RepoLocation,
    on_change: Box<dyn Fn() + Send + Sync>,
) -> Result<ReviewWatcher> {
    match &location {
        RepoLocation::Local(_) => {
            let dir = local_reviews_dir(&location)?;
            // The directory must exist to be watched; creating it is what
            // the store does on first save anyway.
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            let mut watcher = notify::recommended_watcher(
                move |event: std::result::Result<notify::Event, notify::Error>| {
                    // Errors fire the callback too: ReadDirectoryChangesW's
                    // buffer-overflow error specifically means *dropped*
                    // events, so a spurious reload is the safe direction —
                    // silence would mean staleness.
                    if let Err(err) = &event {
                        eprintln!("review watcher event error (reloading anyway): {err}");
                    }
                    on_change();
                },
            )
            .context("creating file watcher")?;
            watcher
                .watch(&dir, notify::RecursiveMode::NonRecursive)
                .with_context(|| format!("watching {}", dir.display()))?;
            Ok(ReviewWatcher::Local { _watcher: watcher })
        }
        RepoLocation::Wsl { distro, path } => {
            let io = StoreIo::new(location.clone());
            let mut last: Option<String> = None;
            let distro = distro.clone();
            let supervisor = supervisor::spawn(SupervisorConfig {
                label: location.display_name(),
                root: path.clone(),
                kind: "store",
                on_change: Arc::from(on_change),
                // The manager owns spawn pacing (cool-downs, install);
                // returning `None` while hosts are disabled or the distro
                // is cooling down is what keeps the supervisor's degraded
                // loop cheap. Gated on `distro_running` (capstone P2-D):
                // `client_for` can spawn a host — which BOOTS a stopped
                // distro — and a background reacquisition retry must never
                // resurrect a distro the user deliberately shut down. The
                // probe is a cheap, throttled `wsl.exe --list --running`
                // service query that itself boots nothing.
                client_source: Box::new({
                    let distro = distro.clone();
                    move || {
                        if !manager::distro_running(&distro) {
                            return None;
                        }
                        manager::client_for(&distro)
                            .filter(|client| client.is_alive() && client.has_cap("watch"))
                    }
                }),
                // The pre-supervisor digest poll, now the degraded mode: a
                // baseline on the first call, then "did the digest move"
                // per tick. `last` persists across host sessions, so
                // re-entering the fallback after an outage also drift-checks
                // against the pre-outage state. Also `distro_running`-gated
                // (capstone P2-D): the digest shells `wsl.exe -d <distro>
                // ls`, which boots a stopped distro within one tick of a
                // `wsl --shutdown` — while the distro is down this reports
                // "no change" without touching it, and `last`'s pre-outage
                // baseline means the first post-restart digest still
                // catches anything that changed in between.
                fallback_tick: Some(Box::new(move || {
                    if !manager::distro_running(&distro) {
                        return false;
                    }
                    // Boot-suspicion self-correction (capstone P2-D): the
                    // probe gate above can never fully close the race — a
                    // `wsl --shutdown` landing in the tens of ms between
                    // the probe's answer and this digest's `wsl.exe` spawn
                    // makes THIS command boot the distro back up, and
                    // without correction that latches (the accidental boot
                    // makes every later probe honestly say "running").
                    // A digest against a running distro is sub-second; one
                    // that had to boot it first takes seconds — flag it so
                    // `distro_running` quarantines the distro long enough
                    // to idle-terminate itself. See `note_boot_suspect`.
                    let started = std::time::Instant::now();
                    let next = digest(&io);
                    if started.elapsed() >= BOOT_SUSPECT_THRESHOLD {
                        manager::note_boot_suspect(&distro);
                        return false;
                    }
                    let changed = last.as_ref().is_some_and(|prev| *prev != next);
                    last = Some(next);
                    changed
                })),
                synthetic_on_first_arm: true,
                poll_interval: POLL_INTERVAL,
            });
            Ok(ReviewWatcher::Remote {
                _supervisor: supervisor,
            })
        }
    }
}

/// A cheap change digest: the review file listing plus sizes/mtimes, one
/// `ls -la` (or read_dir) through the store's I/O layer.
fn digest(io: &StoreIo) -> String {
    io.digest_dir("dv/reviews").unwrap_or_default()
}

fn local_reviews_dir(location: &RepoLocation) -> Result<PathBuf> {
    let RepoLocation::Local(root) = location else {
        anyhow::bail!("local_reviews_dir called for a WSL location");
    };
    Ok(super::io::resolve_local_git_dir(root)?
        .join("dv")
        .join("reviews"))
}
