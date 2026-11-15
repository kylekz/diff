//! A PR review's side of the session (PLAN §9.4, §9.5): what the last refresh read of its
//! pull request, and the refreshes it runs through the `GitHub` service.
//!
//! A PR review is refreshed when it opens (an open by the pull request's reference already
//! did, and hands over what it read) and whenever the reviewer asks; nothing polls. A
//! review opened by its id shows its stored mirror meanwhile (`show_stored_pr`): the pull
//! request as it was last read, its threads placed, while the strip says it is reading; a
//! refresh that then fails leaves that mirror, stale. A
//! refresh that moves the review's pins lists the files again: the sides are resolved from
//! the pins (`dv_core::review::pins::resolve`), so the stream shows the new head and the
//! threads relocate, as a branch review's do after a commit. A refresh another process made
//! (`dv review refresh`) is followed through the store's change events: its mirror's file
//! is this session's to read again, and the pins it moves once the mirror is written are
//! waited for and resolved again.
//!
//! **One at a time.** What a session asks GitHub about its pull request, a refresh or a
//! Resolve / Unresolve with the refresh after it ([`PrOp`]), runs one after the other in the
//! order asked: a Resolve asked for while a refresh runs waits for it, and a refresh asked
//! for while a Resolve runs waits for that one's own refresh. So no read that began before
//! a change can land after it and show the thread as it was (PLAN §9.6). Another process
//! refreshing at that moment isn't waited for: the next refresh puts it right.
//!
//! **What the strip says beside the mirror** ([`PrFacts`], DESIGN §4.5) is read in the
//! background whenever what was read of the pull request changes: the review's PR round
//! (its place among the pull request's reviews in the store, PLAN §9.5) and how far the
//! pull request moved ([`PrMove`]): GitHub's head against the head the review shows, which
//! are the mirror's and the pins' and need not be the same commit (PLAN §9.5), else the
//! head the last refresh moved the review from.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use dv_core::git::Oid;
use dv_core::github::{MirrorStore, PrRef, is_mirror_of};
use dv_core::location::Location;
use dv_core::review::{ListEntry, Review, ReviewStatus, pins};
use futures::channel::oneshot;
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
    /// What is being asked of GitHub (module docs, "One at a time").
    running: Option<(PrOp, Task<()>)>,
    /// What was asked for meanwhile, in the order it runs.
    waiting: VecDeque<PrOp>,
    /// A mirror another process wrote is being read.
    reading: Option<Task<()>>,
    /// What the strip says beside the mirror (module docs).
    facts: PrFacts,
    /// The read of `facts` that is running; a newer one replaces it.
    reading_facts: Option<Task<()>>,
    /// Who waits for the refreshes asked for to be done ([`ReviewSession::refresh_pr_then`]).
    refreshed: Vec<oneshot::Sender<()>>,
}

impl PrState {
    pub(super) fn is_settled(&self) -> bool {
        self.running.is_none()
            && self.waiting.is_empty()
            && self.reading.is_none()
            && self.reading_facts.is_none()
    }
}

/// How two commits of a pull request differ in history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Commits {
    /// The newer one holds the older one and this many commits more.
    New(u32),
    /// Each holds commits the other doesn't: history was rewritten (a force-push).
    Rewritten,
    /// The older one holds the newer one: the pull request was put back at an earlier
    /// commit, or what was read of it is older than the head the review shows.
    Earlier,
}

/// Why the head a PR review shows isn't, or wasn't, the pull request's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveKind {
    /// GitHub's head, as last read, is another commit than the one the review shows: the
    /// pull request moved on and the review didn't follow (a submitted or closed review,
    /// pins another process moved).
    Ahead,
    /// The last refresh moved the review to the pull request's head.
    Followed,
}

/// How far a pull request moved, from the commit `from` to `to` (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrMove {
    pub kind: MoveKind,
    pub from: Oid,
    pub to: Oid,
    /// `None` until it is counted, and when it can't be (a commit that isn't in the clone).
    pub commits: Option<Commits>,
}

/// What the strip says beside the mirror (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrFacts {
    /// The review's PR round; `None` until the store was listed.
    pub round: Option<u32>,
    pub moved: Option<PrMove>,
}

/// One thing a session asks GitHub about its pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PrOp {
    Refresh,
    /// A Resolve (`resolved`) or an Unresolve of GitHub's thread `node`, and the refresh
    /// after it.
    Resolve {
        node: String,
        resolved: bool,
    },
}

impl ReviewSession {
    /// What the last refresh of the review's pull request found; `None` for a local review,
    /// and for a PR review before its first refresh lands.
    pub fn pr(&self) -> Option<&PrRead> {
        self.pr.read.as_ref()
    }

    /// What the strip says beside the mirror (module docs).
    pub fn pr_facts(&self) -> &PrFacts {
        &self.pr.facts
    }

    /// Reads [`PrFacts`] again for what is known of the pull request now (module docs); the
    /// commits of a move already counted aren't counted again.
    pub(super) fn read_pr_facts(&mut self, cx: &mut Context<Self>) {
        let Some(link) = self.review.pr_target().cloned() else {
            return;
        };
        let read = self.pr.read.as_ref();
        let shown = read
            .and_then(|read| read.pins.as_ref())
            .map(|pins| &pins.head);
        let github = read
            .and_then(|read| read.mirror.as_ref())
            .map(|mirror| &mirror.pr.head_oid);
        let moved_from = read
            .and_then(|read| read.moved_from.as_ref())
            .map(|pins| &pins.head);
        let moved = match (shown, github, moved_from) {
            (Some(shown), Some(github), _) if shown != github => {
                Some((MoveKind::Ahead, shown.clone(), github.clone()))
            }
            (Some(shown), _, Some(from)) if from != shown => {
                Some((MoveKind::Followed, from.clone(), shown.clone()))
            }
            _ => None,
        };
        let counted = self.pr.facts.moved.take().filter(|known| {
            moved.as_ref().is_some_and(|(kind, from, to)| {
                known.kind == *kind && &known.from == from && &known.to == to
            })
        });
        let count = moved
            .as_ref()
            .filter(|_| counted.as_ref().is_none_or(|known| known.commits.is_none()))
            .map(|(_, from, to)| (from.clone(), to.clone()));
        self.pr.facts.moved = counted.or(moved.map(|(kind, from, to)| PrMove {
            kind,
            from,
            to,
            commits: None,
        }));
        let (store, repo, id) = (
            self.store.clone(),
            self.repo.clone(),
            self.review.id.clone(),
        );
        let job = cx.background_spawn(async move {
            let round = store.list().ok().map(|entries| {
                let reviews: Vec<_> = entries
                    .into_iter()
                    .filter_map(|entry| match entry {
                        ListEntry::Ok(summary) => Some((summary.id, summary.target)),
                        ListEntry::Err { .. } => None,
                    })
                    .collect();
                link.round_of(&id, reviews.iter().map(|(id, target)| (id, target)))
            });
            let commits = count.and_then(|(from, to)| {
                let counted = repo.commits_apart(&from, &to).ok()?;
                Some(match counted {
                    (0, ahead) => Commits::New(ahead),
                    (_, 0) => Commits::Earlier,
                    _ => Commits::Rewritten,
                })
            });
            (round, commits)
        });
        self.pr.reading_facts = Some(cx.spawn(async move |this, cx| {
            let (round, commits) = job.await;
            this.update(cx, |this, cx| {
                this.pr.reading_facts = None;
                if round.is_some() {
                    this.pr.facts.round = round;
                }
                if let (Some(moved), Some(commits)) = (&mut this.pr.facts.moved, commits) {
                    moved.commits = Some(commits);
                }
                // The strip alone shows them: nothing the stream draws changed.
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Shows what was last read of the review's pull request, from its stored mirror, until
    /// this session's first refresh lands (module docs): read in the background, and dropped
    /// when the refresh landed first with a mirror of its own (one that failed before it
    /// read the store hands none back, and what is stored then shows, stale). Nothing for a local review, or a pull request never
    /// read here.
    pub(super) fn show_stored_pr(&mut self, cx: &mut Context<Self>) {
        let Some(link) = self.review.pr_target().cloned() else {
            return;
        };
        let (repo, backend, id) = (
            self.repo.clone(),
            self.backend.clone(),
            self.review.id.clone(),
        );
        let job = cx.background_spawn(async move {
            let mirrors = MirrorStore::new(backend);
            let stored = mirrors
                .hosts()
                .and_then(|hosts| mirrors.latest_stored(&hosts, &link))
                .ok()
                .flatten();
            (stored, pins::read(&repo, &id).ok().flatten())
        });
        self.pr.reading = Some(cx.spawn(async move |this, cx| {
            let (stored, pinned) = job.await;
            this.update(cx, |this, cx| {
                this.pr.reading = None;
                // Taken while nothing shows of the pull request: before the refresh
                // lands, or after one that failed without a mirror to hand back.
                let shown = match (&mut this.pr.read, stored) {
                    (None, Some(mirror)) => {
                        this.pr.read = Some(PrRead::stored(mirror, pinned));
                        true
                    }
                    (Some(read), Some(mirror)) if read.mirror.is_none() => {
                        read.mirror = Some(Arc::new(mirror));
                        read.pins = read.pins.take().or(pinned);
                        true
                    }
                    _ => false,
                };
                if shown {
                    this.read_github(cx);
                    this.read_pr_facts(cx);
                    cx.emit(SessionEvent::PrChanged);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Whether a refresh of the pull request was asked for and isn't done: running, or
    /// waiting its turn behind a Resolve.
    pub fn is_refreshing_pr(&self) -> bool {
        matches!(self.pr.running, Some((PrOp::Refresh, _)))
            || self.pr.waiting.contains(&PrOp::Refresh)
    }

    /// Refreshes the review's pull request (PLAN §9.4): everything GitHub holds read again
    /// into its mirror, a moved head fetched, a draft's pins moved to it. Nothing for a local
    /// review, or while a refresh runs with nothing after it; after a Resolve that is
    /// running or waiting (module docs, "One at a time").
    pub fn refresh_pr(&mut self, cx: &mut Context<Self>) {
        // A refresh that runs with nothing after it reads what this one would.
        let covered =
            matches!(self.pr.running, Some((PrOp::Refresh, _))) && self.pr.waiting.is_empty();
        if self.review.is_pr_review() && !covered {
            self.ask_pr(PrOp::Refresh, cx);
        }
    }

    /// [`ReviewSession::refresh_pr`], and a receiver that hears once no refresh is running
    /// or waiting any more (at once when there is nothing to refresh).
    pub fn refresh_pr_then(&mut self, cx: &mut Context<Self>) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        self.refresh_pr(cx);
        if self.is_refreshing_pr() {
            self.pr.refreshed.push(sender);
        } else {
            let _ = sender.send(());
        }
        receiver
    }

    /// Whether opening `reference` from `location` would resume this very review (PLAN §9.3):
    /// it is open, was opened from there, and its pull request is the one named: its
    /// target's owner, repository and number, and, when the reference names a host and the
    /// pull request was read here, the host it was read from (a review's target names none,
    /// §2.2; before a mirror there is none to hold against, and the refresh itself checks
    /// the clone, §9.4). The shell then refreshes it here, in its turn (module docs, "One at
    /// a time"), rather than open it again beside what the session is asking GitHub.
    pub fn is_open_pr(&self, location: &Location, reference: &PrRef) -> bool {
        let Some(link) = self.review.pr_target() else {
            return false;
        };
        let named = |owner: &str, repo: &str| {
            link.owner.eq_ignore_ascii_case(owner) && link.repo.eq_ignore_ascii_case(repo)
        };
        self.review.status.is_open()
            && self.origin == *location
            && link.number == reference.number()
            && match reference {
                PrRef::Number(_) | PrRef::Hash(_) => true,
                PrRef::Short { owner, repo, .. } => named(owner, repo),
                PrRef::Url {
                    host, owner, repo, ..
                } => {
                    named(owner, repo)
                        && self
                            .pr()
                            .and_then(|read| read.mirror.as_ref())
                            .is_none_or(|mirror| mirror.key.host.eq_ignore_ascii_case(host))
                }
            }
    }

    /// Runs `op` now, or once what is running and what was asked before it are done (module
    /// docs, "One at a time"). What already waits isn't asked for twice.
    pub(super) fn ask_pr(&mut self, op: PrOp, cx: &mut Context<Self>) {
        if self.pr.running.is_none() {
            self.start_pr(op, cx);
        } else if !self.pr.waiting.contains(&op) {
            self.pr.waiting.push_back(op);
        }
        cx.notify();
    }

    /// Starts `op`, or, when there is nothing to ask for it any more, the next one waiting.
    fn start_pr(&mut self, op: PrOp, cx: &mut Context<Self>) {
        let mut next = Some(op);
        while let Some(op) = next {
            let started = match &op {
                PrOp::Refresh => self.start_refresh(cx),
                PrOp::Resolve { node, resolved } => self.start_resolve(node, *resolved, cx),
            };
            match started {
                Some(task) => {
                    self.pr.running = Some((op, task));
                    return;
                }
                None => next = self.pr.waiting.pop_front(),
            }
        }
    }

    /// What was running is done: the next one waiting starts.
    pub(super) fn pr_done(&mut self, cx: &mut Context<Self>) {
        self.pr.running = None;
        if let Some(op) = self.pr.waiting.pop_front() {
            self.start_pr(op, cx);
        }
        if !self.is_refreshing_pr() {
            for waiting in self.pr.refreshed.drain(..) {
                let _ = waiting.send(());
            }
        }
        cx.notify();
    }

    fn start_refresh(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        let github = cx.try_services()?.github.clone();
        let (backend, review) = (self.backend.clone(), self.review.clone());
        let refresh = github.update(cx, |github, cx| github.refresh(backend, review, cx));
        Some(cx.spawn(async move |this, cx| {
            let read = PrRead::from(refresh.await);
            this.update(cx, |this, cx| {
                this.apply_pr_read(read, cx);
                this.pr_done(cx);
            })
            .ok();
        }))
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
        self.read_github(cx);
        self.read_pr_facts(cx);
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
                        read.fresh = true;
                        read.stale = None;
                    }
                    cx.emit(SessionEvent::PrChanged);
                }
                this.read_github(cx);
                this.read_pr_facts(cx);
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
/// the pull request was read, `#482` before, as the switcher names it.
pub(super) fn label(review: &Review, read: Option<&PrRead>) -> Option<String> {
    let link = review.pr_target()?;
    Some(crate::switcher::items::pr_label(
        link.number,
        read.and_then(|read| read.mirror.as_ref())
            .map(|mirror| mirror.pr.title.as_str()),
    ))
}
