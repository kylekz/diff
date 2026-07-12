//! Change notification for the review store, so the GUI reflects external
//! edits (the agent CLI, another dv window) without reload. Local repos get
//! real file watching (`notify`); a WSL repo with a live, `watch`-capable
//! `dv-host` connection gets real inotify through it too (the `Remote`
//! arm — plan §6); a WSL repo with no host connection falls back to
//! polling a directory digest through the command layer.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use notify::Watcher as _;

use crate::location::RepoLocation;
use crate::remote::client::HostClient;
use crate::remote::manager;

use super::io::StoreIo;

/// How often the WSL fallback polls. Local watching (and the `Remote`
/// host-backed arm) is event-driven.
const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// Keeps the watch alive; dropping it stops callbacks (best-effort — an
/// in-flight callback may still complete).
pub enum ReviewWatcher {
    /// A local repo: a real OS file watcher (`notify`) on `.git/dv/reviews`.
    Local {
        stopped: Arc<AtomicBool>,
        /// Held only for its `Drop` (stops the OS watcher).
        _watcher: notify::RecommendedWatcher,
    },
    /// A WSL repo with no `watch`-capable host connection: the pre-S4
    /// digest-poll fallback (still the only option for `DV_NO_HOST=1`, a
    /// disabled/cooling-down/dead distro, or an older host binary).
    Poll { stopped: Arc<AtomicBool> },
    /// A WSL repo with a live, `watch`-capable `dv-host` connection: real
    /// inotify via `watch/subscribe { kind: "store" }`. `Drop` unregisters
    /// the callback and unsubscribes (best-effort).
    ///
    /// KNOWN S4 GAP (accepted, tracked for S5): if the host connection dies
    /// mid-session (crash, `kill -9`, distro stopped), this variant goes
    /// permanently silent — there's no reconnect/resubscribe-on-respawn
    /// logic yet, unlike `CommandBuilder`'s Route, which transparently
    /// falls back to `wsl.exe` per call. Accepted because it degrades to
    /// "external CLI/other-window edits stop appearing live," never to
    /// data loss or a broken GUI: every mutation this workspace makes
    /// *itself* (the comment editor, resolve/reply, submit) applies
    /// straight to `self.review` in the completion handler and neither
    /// needs nor waits on this watcher at all — only cross-process
    /// notifications are what silently stop. A future session's fresh
    /// `HostClient` (once `manager::client_for`'s cool-down elapses) gets a
    /// fresh working watcher again automatically; it's only the *current*
    /// workspace instance that stays silent for the rest of its life. S5
    /// robustness work should add resubscribe-on-respawn here.
    Remote {
        client: Arc<HostClient>,
        watch_id: u64,
    },
}

impl Drop for ReviewWatcher {
    fn drop(&mut self) {
        match self {
            ReviewWatcher::Local { stopped, .. } | ReviewWatcher::Poll { stopped } => {
                stopped.store(true, Ordering::Relaxed);
            }
            ReviewWatcher::Remote { client, watch_id } => {
                client.unregister_watch_callback(*watch_id);
                let _ = client.watch_unsubscribe(*watch_id);
            }
        }
    }
}

/// Start watching the reviews directory for `location`, invoking
/// `on_change` (from a background thread — either `notify`'s callback
/// thread, the poll thread, or the host client's dedicated watch-dispatch
/// thread) whenever anything in it changes. The callback must be cheap —
/// hand off to a channel.
pub(super) fn watch(
    location: RepoLocation,
    on_change: Box<dyn Fn() + Send + Sync>,
) -> Result<ReviewWatcher> {
    match &location {
        RepoLocation::Local(_) => {
            let stopped = Arc::new(AtomicBool::new(false));
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
            Ok(ReviewWatcher::Local {
                stopped,
                _watcher: watcher,
            })
        }
        RepoLocation::Wsl { distro, path } => {
            // Third arm (plan §6/§8 S4): a live host connection that
            // advertises the `watch` cap gets real inotify instead of the
            // 1s digest poll below. Any failure here (spawn/connect
            // trouble, the host rejecting the subscribe, ...) falls
            // straight through to the poll fallback rather than erroring
            // the whole watcher — "never worse than not having a host",
            // same posture as `CommandBuilder`'s Route selection.
            if let Some(client) = manager::client_for(distro)
                && client.has_cap("watch")
            {
                match client.watch_subscribe(path, "store") {
                    Ok(watch_id) => {
                        client.register_watch_callback(watch_id, move |_params| on_change());
                        return Ok(ReviewWatcher::Remote { client, watch_id });
                    }
                    Err(err) => {
                        eprintln!(
                            "review watcher: dv-host watch/subscribe failed, falling back \
                             to polling: {err:#}"
                        );
                    }
                }
            }

            // Plan §8 S4 gate ("verify the digest-poll thread is ABSENT
            // when the host watch is active"): this line is the runtime
            // proof a test can grep dv's stderr for — the Remote arm above
            // `return`s before ever reaching here, so its absence in a
            // session's log is direct evidence the poll thread never
            // started, not just a code-trace argument. Same
            // `DV_HOST_DEBUG`-gated convention as
            // `remote::manager::note_spawn_fallback`.
            if std::env::var("DV_HOST_DEBUG").as_deref() == Ok("1") {
                eprintln!(
                    "[dv review watcher] starting WSL digest-poll thread for {}",
                    location.display_name()
                );
            }
            let stopped = Arc::new(AtomicBool::new(false));
            let io = StoreIo::new(location);
            let stop = stopped.clone();
            std::thread::Builder::new()
                .name("dv-review-poll".into())
                .spawn(move || {
                    let mut last = digest(&io);
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(POLL_INTERVAL);
                        let next = digest(&io);
                        if next != last {
                            last = next;
                            on_change();
                        }
                    }
                })
                .context("spawning review poll thread")?;
            Ok(ReviewWatcher::Poll { stopped })
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
