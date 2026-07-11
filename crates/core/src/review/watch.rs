//! Change notification for the review store, so the GUI reflects external
//! edits (the agent CLI, another dv window) without reload. Local repos get
//! real file watching (`notify`); WSL repos poll a directory digest through
//! the command layer — proper inotify arrives with the Phase-4 host process.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use notify::Watcher as _;

use crate::location::RepoLocation;

use super::io::StoreIo;

/// How often the WSL fallback polls. Local watching is event-driven.
const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// Keeps the watch alive; dropping it stops callbacks (best-effort — an
/// in-flight callback may still complete).
pub struct ReviewWatcher {
    stopped: Arc<AtomicBool>,
    /// Held for its Drop (stops the OS watcher). None in polling mode.
    _watcher: Option<notify::RecommendedWatcher>,
}

impl Drop for ReviewWatcher {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

/// Start watching the reviews directory for `location`, invoking
/// `on_change` (from a background thread) whenever anything in it changes.
/// The callback must be cheap — hand off to a channel.
pub(super) fn watch(
    location: RepoLocation,
    on_change: Box<dyn Fn() + Send + Sync>,
) -> Result<ReviewWatcher> {
    let stopped = Arc::new(AtomicBool::new(false));
    match &location {
        RepoLocation::Local(_) => {
            let dir = local_reviews_dir(&location)?;
            // The directory must exist to be watched; creating it is what
            // the store does on first save anyway.
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            let mut watcher = notify::recommended_watcher(
                move |event: std::result::Result<notify::Event, notify::Error>| {
                    if event.is_ok() {
                        on_change();
                    }
                },
            )
            .context("creating file watcher")?;
            watcher
                .watch(&dir, notify::RecursiveMode::NonRecursive)
                .with_context(|| format!("watching {}", dir.display()))?;
            Ok(ReviewWatcher {
                stopped,
                _watcher: Some(watcher),
            })
        }
        RepoLocation::Wsl { .. } => {
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
            Ok(ReviewWatcher {
                stopped,
                _watcher: None,
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
