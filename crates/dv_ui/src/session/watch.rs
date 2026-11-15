//! The session's watch (PLAN §5.3): which parts of the repository it watches, and how a batch
//! becomes a refresh or a fold reload.
//!
//! A live target watches its git state (a `working` target's old side moves when the agent
//! commits, a `staged` one lives in the index, a branch's tip moves) and, when its new side is
//! the working tree, the worktree; every target watches the review store, where agents append.
//! The watch starts with the session, off the UI thread (registering it walks the worktree), and
//! restarts when what it should cover changes (a branch checked out or left).
//!
//! **Nothing is lost between a read and the watch.** A change made before the OS watch is
//! registered is never reported, and a list or a diff read before then may already be stale. So
//! the session notes every read made while no registered watch covers it, and once one is
//! registered it reads again what those were: the list (and every diff, when diffs were read),
//! and the fold, which was read when the review opened.
//!
//! **Stopping never blocks a frame.** Dropping a [`WatchHandle`] joins its thread, which may be
//! inside git loading the ignore rules, so the session drops handles on a thread of their own
//! ([`stop_in_background`]): when the watch restarts, and with the session itself.
//!
//! **A watch that ends by itself** (its sink dropped while the session still holds its handle:
//! a remote watch the new server couldn't take back as it was after a reconnect, PLAN §5.2)
//! starts again through [`ReviewSession::ensure_watch`], whose new handle says what it covers
//! now (`worktree_error`), and what was read meanwhile is read again; while the connection is
//! down it starts again once it is back.

use dv_core::backend::{WatchEvent, WatchHandle, WatchSpec};
use dv_core::{BackendError, DiffSpec, Side, Target};
use futures::StreamExt as _;
use gpui::{AppContext as _, Context, Task};

use super::{Batch, REFRESH_DEBOUNCE, ReviewSession};
use crate::failure::Failure;

/// The watch's state inside a session.
#[derive(Default)]
pub(super) struct WatchState {
    /// What the current watch covers.
    spec: Option<WatchSpec>,
    /// Keeps the watch running; dropping it stops the watch.
    handle: Option<WatchHandle>,
    starting: Option<Task<()>>,
    /// Receives the watch's batches.
    events: Option<Task<()>>,
    /// Tells this watch's batches and start from a superseded one's.
    generation: u64,
    /// The watch of this generation ended before its start landed (module docs).
    ended: Option<u64>,
    notice: Option<String>,
    /// A file list was read while no registered watch covered it (module docs).
    unwatched_list: bool,
    /// A file diff was read while no registered watch covered it.
    unwatched_load: bool,
    /// Worktree and git changes waiting for the debounce.
    pending: Batch,
    debounce: Option<Task<()>>,
}

impl WatchState {
    /// What the watch covers (or is starting to).
    #[cfg(test)]
    pub(super) fn spec(&self) -> Option<WatchSpec> {
        self.spec
    }

    /// Whether a watch is registered.
    #[cfg(test)]
    pub(super) fn is_registered(&self) -> bool {
        self.handle.is_some()
    }

    /// Makes the session believe the watch covers `spec`, so the next `ensure_watch` restarts it.
    #[cfg(test)]
    pub(super) fn pretend_spec(&mut self, spec: WatchSpec) {
        self.spec = Some(spec);
    }

    pub(super) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub(super) fn is_settled(&self) -> bool {
        self.starting.is_none() && self.debounce.is_none()
    }

    /// Whether a watch is registered or starting.
    pub(super) fn is_running(&self) -> bool {
        self.handle.is_some() || self.starting.is_some()
    }

    /// Whether a registered watch reports every change from now on.
    fn covers(&self) -> bool {
        self.handle.is_some() && self.notice.is_none()
    }

    /// A file list is about to be read.
    pub(super) fn note_list(&mut self) {
        if !self.covers() {
            self.unwatched_list = true;
        }
    }

    /// A file diff is about to be read.
    pub(super) fn note_load(&mut self) {
        if !self.covers() {
            self.unwatched_load = true;
        }
    }
}

impl Drop for WatchState {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            stop_in_background(handle);
        }
    }
}

/// Drops `handle` on a thread of its own: stopping a watch joins its thread, which may be in the
/// middle of a git call (module docs).
pub(super) fn stop_in_background(handle: WatchHandle) {
    let spawned = std::thread::Builder::new()
        .name("dv-watch-stop".to_string())
        .spawn(move || drop(handle));
    if let Err(error) = spawned {
        // The closure, and the handle with it, was dropped on this thread instead.
        log::warn!("stopping a watch in the background failed: {error}");
    }
}

/// What a session over `target` watches, resolved to `spec`, or not resolved (yet): then the
/// store and the git state, so a branch that comes back or a first commit refreshes it, and the
/// worktree for a `working` target, whose new side is always the working tree.
pub fn watch_spec(target: &Target, spec: Option<&DiffSpec>) -> WatchSpec {
    WatchSpec {
        worktree: match spec {
            Some(spec) => spec.new == Side::Worktree,
            None => matches!(target, Target::Working),
        },
        store: true,
        git: matches!(
            target,
            Target::Working | Target::Staged | Target::Branch { .. }
        ),
    }
}

impl ReviewSession {
    /// Starts the watch, or restarts it when it should cover something else.
    pub(super) fn ensure_watch(&mut self, cx: &mut Context<Self>) {
        let wanted = watch_spec(&self.review.target, self.spec.as_ref());
        let watch = &mut self.watch;
        if watch.spec == Some(wanted) && (watch.handle.is_some() || watch.starting.is_some()) {
            return;
        }
        watch.generation += 1;
        let generation = watch.generation;
        if watch.spec.is_some() {
            // The watch that ran didn't cover what this one will: what was read under it is
            // read again once this one is registered.
            watch.unwatched_list = true;
            watch.unwatched_load = true;
        }
        watch.spec = Some(wanted);
        if let Some(handle) = watch.handle.take() {
            stop_in_background(handle);
        }
        let (sender, mut receiver) = futures::channel::mpsc::unbounded::<WatchEvent>();
        let backend = self.backend.clone();
        let start = cx.background_spawn(async move {
            backend.watch(
                wanted,
                Box::new(move |event| {
                    // The receiver is gone only once the session is.
                    let _ = sender.unbounded_send(event);
                }),
            )
        });
        watch.starting = Some(cx.spawn(async move |this, cx| {
            let started = start.await;
            this.update(cx, |this, cx| this.watch_started(generation, started, cx))
                .ok();
        }));
        watch.events = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = receiver.next().await {
                if this
                    .update(cx, |this, cx| this.on_watch_event(generation, event, cx))
                    .is_err()
                {
                    return;
                }
            }
            // The sink is gone: the watch ended by itself, or was stopped.
            this.update(cx, |this, cx| this.watch_ended(generation, cx))
                .ok();
        }));
    }

    /// The watch of `generation` delivers nothing more (module docs): when it is the current
    /// one, it starts again. A watch that never started is `watch_started`'s to report.
    fn watch_ended(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation != self.watch.generation {
            return;
        }
        let Some(handle) = self.watch.handle.take() else {
            if self.watch.starting.is_some() {
                self.watch.ended = Some(generation);
            }
            return;
        };
        log::info!(
            "review {}: the watch ended; it starts again",
            self.review.id
        );
        stop_in_background(handle);
        self.watch.notice = None;
        // Down, it starts again once the connection is back (`connection`).
        if !self.link.down {
            self.ensure_watch(cx);
        }
        cx.notify();
    }

    pub(super) fn watch_started(
        &mut self,
        generation: u64,
        started: Result<WatchHandle, BackendError>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.watch.generation {
            // Superseded: dropping the handle stops that watch.
            if let Ok(handle) = started {
                stop_in_background(handle);
            }
            return;
        }
        self.watch.starting = None;
        match started {
            Ok(handle) if self.watch.ended == Some(generation) => {
                // It ended before it landed: start it again.
                self.watch.ended = None;
                stop_in_background(handle);
                if !self.link.down {
                    self.ensure_watch(cx);
                }
            }
            Ok(handle) => {
                self.watch.notice = handle.worktree_error().map(str::to_string);
                self.watch.handle = Some(handle);
                self.catch_up(cx);
            }
            Err(error) => {
                let failure = Failure::from(&error);
                match self.lost_connection(&failure) {
                    // The connection came back meanwhile: start it on the new server.
                    Some(true) => self.ensure_watch(cx),
                    // It starts again once the connection is back (`connection`).
                    Some(false) => {}
                    None => {
                        log::warn!("review {}: no watch: {error}", self.review.id);
                        self.watch.notice = Some(failure.message);
                    }
                }
            }
        }
        cx.notify();
    }

    /// The watch is registered: reads made before it may have missed a change it will never
    /// report, so they are made again (module docs). The fold always is: it was read when the
    /// review opened.
    fn catch_up(&mut self, cx: &mut Context<Self>) {
        self.reload_fold(cx);
        if !self.watch.covers() {
            return;
        }
        let list = std::mem::take(&mut self.watch.unwatched_list);
        let loads = std::mem::take(&mut self.watch.unwatched_load);
        if loads {
            self.refresh(Batch::everything(), cx);
        } else if list {
            self.refresh(Batch::default(), cx);
        }
    }

    /// One batch from the watch: an append to this review's log reloads the fold; a PR
    /// review's mirror rewritten is followed (`pr`), and its round read again when another
    /// review's log came or went; worktree and git changes wait
    /// [`REFRESH_DEBOUNCE`] for more, then refresh. Other store paths (snapshot caches, the
    /// snapshot's temporary index) change nothing here.
    pub(super) fn on_watch_event(
        &mut self,
        generation: u64,
        event: WatchEvent,
        cx: &mut Context<Self>,
    ) {
        if generation != self.watch.generation {
            return;
        }
        let log = format!("reviews/{}.jsonl", self.review.id);
        if event.rescan || event.store_paths.contains(&log) {
            self.reload_fold(cx);
        }
        self.follow_mirror(&event.store_paths, cx);
        // A PR review's round is its place among its pull request's reviews in the store
        // (PLAN §9.5): another review made or deleted may change it.
        let reviews_changed = event
            .store_paths
            .iter()
            .any(|path| path.starts_with("reviews/") && path.ends_with(".jsonl") && *path != log);
        if self.review.is_pr_review() && (event.rescan || reviews_changed) {
            self.read_pr_facts(cx);
        }
        if event.rescan || !event.worktree_paths.is_empty() {
            cx.emit(super::SessionEvent::WorktreeChanged {
                paths: event.worktree_paths.iter().cloned().collect(),
                rescan: event.rescan,
            });
        }
        let batch = Batch {
            paths: event.worktree_paths.into_iter().collect(),
            git: event.git_changed,
            rescan: event.rescan,
        };
        if !batch.is_empty() {
            self.watch.pending.merge(batch);
            self.schedule_refresh(cx);
        }
    }

    /// Refreshes with the pending batch once [`REFRESH_DEBOUNCE`] has passed since the first
    /// change of it.
    fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        if self.watch.debounce.is_some() {
            return;
        }
        self.watch.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                this.watch.debounce = None;
                let batch = std::mem::take(&mut this.watch.pending);
                this.refresh(batch, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// The window was activated: a session whose watch can't cover the worktree refreshes
    /// everything instead (PLAN §5.3), and the file view's file read from the disk (the working
    /// tree as it stands, a file outside the repository), which no watch of a review whose new
    /// side isn't the working tree covers, is read again (`side`).
    pub fn window_activated(&mut self, cx: &mut Context<Self>) {
        if self.watch.notice.is_some() {
            self.rescan(cx);
        }
        self.read_disk_view_again(cx);
    }
}
