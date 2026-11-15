//! A PR review's side of the session (PLAN §9.4, §9.5): what the last refresh read of its
//! pull request, and the refreshes it runs through the `GitHub` service.
//!
//! A PR review is refreshed when it opens (an open by the pull request's reference already
//! did, and hands over what it read) and whenever the reviewer asks; nothing polls. A
//! refresh that moves the review's pins lists the files again: the sides are resolved from
//! the pins (`dv_core::review::pins::resolve`), so the stream shows the new head and the
//! threads relocate, as a branch review's do after a commit. A refresh another process made
//! (`dv review refresh`) is followed through the store's change events: its mirror's file
//! is this session's to read again, and the pins it moves once the mirror is written are
//! waited for and resolved again.

use std::sync::Arc;
use std::time::Duration;

use dv_core::github::{MirrorStore, is_mirror_of};
use dv_core::review::{Review, ReviewStatus, pins};
use gpui::{AppContext as _, Context, Task};

use super::{Batch, ReviewSession, SessionEvent};
use crate::ActiveServices as _;
use crate::services::github::PrRead;

/// How long a session waits before it reads a draft's pins again, and how many times, when a
/// mirror another process wrote names another head than the pins: a refresh writes the
/// mirror first and moves the pins to its head next (PLAN §9.5), and only the mirror's file
/// is a change the store's watch reports. Pins that stay behind (the refresh lost them to
/// another, or couldn't move them) are shown as they stand.
const PIN_WAIT: Duration = Duration::from_millis(100);
const PIN_READS: usize = 10;

/// The pull request's state inside a session.
#[derive(Default)]
pub(super) struct PrState {
    /// What the last refresh found; `None` before the first one lands.
    pub(super) read: Option<PrRead>,
    /// A refresh running.
    refreshing: Option<Task<()>>,
    /// A mirror another process wrote is being read.
    reading: Option<Task<()>>,
}

impl PrState {
    pub(super) fn is_settled(&self) -> bool {
        self.refreshing.is_none() && self.reading.is_none()
    }
}

impl ReviewSession {
    /// What the last refresh of the review's pull request found; `None` for a local review,
    /// and for a PR review before its first refresh lands.
    pub fn pr(&self) -> Option<&PrRead> {
        self.pr.read.as_ref()
    }

    /// Whether a refresh of the pull request is running.
    pub fn is_refreshing_pr(&self) -> bool {
        self.pr.refreshing.is_some()
    }

    /// Refreshes the review's pull request (PLAN §9.4): everything GitHub holds read again
    /// into its mirror, a moved head fetched, a draft's pins moved to it. Nothing for a local
    /// review, or while a refresh runs.
    pub fn refresh_pr(&mut self, cx: &mut Context<Self>) {
        if !self.review.is_pr_review() || self.pr.refreshing.is_some() {
            return;
        }
        let Some(github) = cx.try_services().map(|services| services.github.clone()) else {
            return;
        };
        let (backend, review) = (self.backend.clone(), self.review.clone());
        let refresh = github.update(cx, |github, cx| github.refresh(backend, review, cx));
        self.pr.refreshing = Some(cx.spawn(async move |this, cx| {
            let read = PrRead::from(refresh.await);
            this.update(cx, |this, cx| {
                this.pr.refreshing = None;
                this.apply_pr_read(read, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// Takes what a refresh read: when the pins moved, or aren't the ones the files were
    /// listed at, the sides are resolved and the files listed again. A refresh that found no
    /// mirror (it failed, and nothing it could read was stored) leaves the one shown, stale:
    /// what was read of the pull request is never replaced with nothing.
    pub fn apply_pr_read(&mut self, mut read: PrRead, cx: &mut Context<Self>) {
        if read.mirror.is_none() {
            read.mirror = self.pr.read.take().and_then(|shown| shown.mirror);
        }
        let listed = self.resolved.as_ref();
        let moved = read.moved_from.is_some()
            || read
                .pins
                .as_ref()
                .is_some_and(|pins| listed.is_some_and(|listed| *listed != pins.spec()));
        self.pr.read = Some(read);
        if moved {
            self.refresh(
                Batch {
                    git: true,
                    ..Batch::default()
                },
                cx,
            );
        }
        cx.emit(SessionEvent::PrChanged);
        cx.notify();
    }

    /// The store changed at `paths`: when one is a mirror of this review's pull request,
    /// another process refreshed it (module docs). Its mirror is read, the pins it moved
    /// waited for ([`PIN_WAIT`]), and the sides resolved from them again.
    pub(super) fn follow_mirror(&mut self, paths: &[String], cx: &mut Context<Self>) {
        let Some(link) = self.review.pr_target().cloned() else {
            return;
        };
        if !paths
            .iter()
            .any(|path| is_mirror_of(path, &link.owner, &link.repo, link.number))
        {
            return;
        }
        let (repo, backend, id) = (
            self.repo.clone(),
            self.backend.clone(),
            self.review.id.clone(),
        );
        // Only a draft's pins follow the head (PLAN §9.5).
        let follows = self.review.status == ReviewStatus::Draft;
        let executor = cx.background_executor().clone();
        let job = cx.background_spawn(async move {
            let mirrors = MirrorStore::new(backend);
            let written = mirrors
                .hosts()
                .and_then(|hosts| mirrors.latest_stored(&hosts, &link))
                .ok()
                .flatten();
            let mut pinned = pins::read(&repo, &id);
            for _ in 0..PIN_READS {
                let behind = match (&pinned, &written) {
                    (Ok(Some(pins)), Some(mirror)) => follows && pins.head != mirror.pr.head_oid,
                    _ => false,
                };
                if !behind {
                    break;
                }
                executor.timer(PIN_WAIT).await;
                pinned = pins::read(&repo, &id);
            }
            (written, pinned)
        });
        self.pr.reading = Some(cx.spawn(async move |this, cx| {
            let (written, pinned) = job.await;
            this.update(cx, |this, cx| {
                this.pr.reading = None;
                // What this session keeps of the pull request follows: the pins as they
                // stand now, and the mirror when it is a newer read of the one shown.
                if let Some(read) = &mut this.pr.read {
                    if let Ok(pinned) = pinned {
                        // The refresh elsewhere moved the head this session showed.
                        if let (Some(now), Some(shown)) = (&pinned, &read.pins)
                            && now != shown
                        {
                            read.moved_from = Some(shown.clone());
                        }
                        read.pins = pinned;
                    }
                    if let Some(mirror) = written
                        && read.mirror.as_ref().is_none_or(|shown| {
                            shown.key == mirror.key && shown.read_at < mirror.read_at
                        })
                    {
                        read.mirror = Some(Arc::new(mirror));
                        read.stale = None;
                    }
                    cx.emit(SessionEvent::PrChanged);
                }
                // The sides are the pins as they stand now.
                this.refresh(
                    Batch {
                        git: true,
                        ..Batch::default()
                    },
                    cx,
                );
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

/// The label of a PR review's target as the title bar shows it: `#482 Fix the cache` once
/// the pull request was read, `#482` before.
pub(super) fn label(review: &Review, read: Option<&PrRead>) -> Option<String> {
    let link = review.pr_target()?;
    Some(match read.and_then(|read| read.mirror.as_ref()) {
        Some(mirror) => format!("#{} {}", link.number, mirror.pr.title),
        None => format!("#{}", link.number),
    })
}
