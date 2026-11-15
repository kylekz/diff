//! GitHub's threads in a session (PLAN §9.6): the threads of the pull request's mirror as
//! the stream shows them beside the review's own, and Resolve / Unresolve.
//!
//! **Shown, never stored.** A GitHub thread is the mirror's `GithubThread` with the anchor
//! `dv_core::github::anchor_threads` reads for it ([`GithubShown`]); it is in no log, and its
//! key is its node id ([`ThreadKey::Github`]). The stream, the comments list and the keys see
//! every thread of the review, its own and GitHub's, as a [`Card`].
//!
//! **Anchors** are read in the background whenever the mirror shown or the sides the
//! review shows change: against the review's own sides (its pins,
//! `ReviewSession::resolved`), never the mirror's head (PLAN §9.5). The session keeps the
//! text of the files they are read from (`dv_core::github::ThreadTexts`: one read per commit
//! and path) for as long as the head and those sides stay, so a refresh that changes no code
//! (a Resolve, a mirror another process wrote, the same mirror arriving twice) reads no file
//! again. Meanwhile each thread the mirror still holds keeps the anchor it had, so such a
//! refresh shows at once and no card jumps; a thread new to the mirror shows once its anchor
//! is read. Placement is the review's own (`placement`): a GitHub thread relocates as the
//! review's threads do, and one without a place in the diff (outdated on GitHub, or not
//! placed here) is in the "Not in the diff" section.
//!
//! **Resolve / Unresolve** change GitHub through the `GitHub` service, in the pull request's
//! turn (`pr`, "One at a time"), then take the refresh that follows
//! (`dv_core::github::set_resolved`). Nothing is written to the review's log, so a finished
//! review resolves too. From the moment it is asked the thread is
//! [`ReviewSession::resolving`], shown as GitHub has it; a call GitHub refuses leaves it so,
//! with why ([`ReviewSession::github_error`]) until the next call on it, or a refresh that
//! shows the thread in the other state: a refresh alone, one that waited its turn behind
//! the call included, doesn't take the reason away. When GitHub took the change and the
//! refresh after it read no mirror, the thread shows GitHub's answer all the same, and the
//! card says that the refresh failed and why, until a refresh reads the pull request.

use std::collections::HashMap;
use std::sync::Arc;

use dv_core::DiffSpec;
use dv_core::github::{GithubThread, Mirror, Resolved, Standing, ThreadTexts, anchor_threads};
use dv_core::review::{Anchor, Thread, ThreadKey, ThreadState};
use gpui::{AppContext as _, Context, Task};

use super::pr::PrOp;
use super::{ReviewSession, SessionEvent};
use crate::ActiveServices as _;
use crate::failure::Failure;
use crate::services::github::PrRead;

/// One of GitHub's threads as the session shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct GithubShown {
    pub thread: GithubThread,
    /// Where GitHub has it (`dv_core::github::GithubAnchor`): the code it is on when
    /// [`Standing::Anchored`], else its original lines and its diff hunk's.
    pub anchor: Anchor,
    pub standing: Standing,
}

impl GithubShown {
    pub fn key(&self) -> ThreadKey {
        ThreadKey::Github(self.thread.id.clone())
    }
}

/// A thread the stream shows a card for: one of the review's own, or one of GitHub's.
#[derive(Clone, Copy, Debug)]
pub enum Card<'a> {
    Local(&'a Thread),
    Github(&'a GithubShown),
}

impl<'a> Card<'a> {
    pub fn key(self) -> ThreadKey {
        match self {
            Self::Local(thread) => ThreadKey::Local(thread.id.clone()),
            Self::Github(shown) => shown.key(),
        }
    }

    /// Where it is attached: a review's thread's anchor, or the one read for a GitHub thread.
    pub fn anchor(self) -> &'a Anchor {
        match self {
            Self::Local(thread) => &thread.anchor,
            Self::Github(shown) => &shown.anchor,
        }
    }

    /// The anchor it was written on: a GitHub thread's is the one it has.
    pub fn first_anchor(self) -> &'a Anchor {
        match self {
            Self::Local(thread) => thread.first_anchor(),
            Self::Github(shown) => &shown.anchor,
        }
    }

    pub fn local(self) -> Option<&'a Thread> {
        match self {
            Self::Local(thread) => Some(thread),
            Self::Github(_) => None,
        }
    }

    pub fn github(self) -> Option<&'a GithubShown> {
        match self {
            Self::Local(_) => None,
            Self::Github(shown) => Some(shown),
        }
    }

    /// Whether it waits on someone, which `]`/`[` stop at: an open or addressed thread of
    /// the review, a GitHub thread GitHub doesn't call resolved.
    pub fn is_waiting(self) -> bool {
        match self {
            Self::Local(thread) => thread.state.is_waiting(),
            Self::Github(shown) => !shown.thread.resolved,
        }
    }

    /// Whether its card collapses to one line until opened.
    pub fn is_resolved(self) -> bool {
        match self {
            Self::Local(thread) => thread.state == ThreadState::Resolved,
            Self::Github(shown) => shown.thread.resolved,
        }
    }

    /// An agent's annotation nobody asked about (PLAN §19): never one of GitHub's.
    pub fn is_annotation(self) -> bool {
        matches!(self, Self::Local(thread) if thread.state == ThreadState::Annotation)
    }

    /// Whether its anchor is code placement looks for: every thread of the review, and a
    /// GitHub thread that has a place in the diff.
    pub fn is_anchored(self) -> bool {
        match self {
            Self::Local(_) => true,
            Self::Github(shown) => shown.standing == Standing::Anchored,
        }
    }

    /// A comment on the whole file.
    pub fn is_file_level(self) -> bool {
        match self {
            Self::Local(thread) => thread.anchor.is_file_level(),
            Self::Github(shown) => shown.thread.is_file_level(),
        }
    }

    /// The first comment's Markdown.
    pub fn body(self) -> &'a str {
        match self {
            Self::Local(thread) => &thread.body,
            Self::Github(shown) => shown
                .thread
                .comments
                .first()
                .map_or("", |comment| comment.body.as_str()),
        }
    }
}

/// GitHub's threads inside a session.
#[derive(Default)]
pub(super) struct GithubState {
    /// The mirror's threads that have an anchor, in the mirror's order.
    shown: Vec<GithubShown>,
    /// The mirror and the sides `shown`'s anchors were read for.
    basis: Option<(Arc<Mirror>, DiffSpec)>,
    /// The files' text those anchors are read from, kept while the head and the sides stay
    /// (module docs, "Anchors").
    texts: Arc<ThreadTexts>,
    reading: Option<Task<()>>,
    /// The threads a Resolve (`true`) or an Unresolve is running for, by node id.
    resolving: HashMap<String, bool>,
    /// What the last Resolve or Unresolve that didn't end well left on its thread's card.
    error: Option<ResolveError>,
}

/// Why a Resolve or an Unresolve of GitHub's thread `node` failed, or what it left unread,
/// for the card's error row.
struct ResolveError {
    node: String,
    failure: Failure,
    until: Until,
}

/// When a [`ResolveError`] is over, beside the next Resolve or Unresolve asked of its
/// thread, and the thread going from GitHub.
enum Until {
    /// GitHub refused the change, the thread staying `resolved` or not as it was: a refresh
    /// that shows it in the other state. A refresh alone says nothing about why the change
    /// failed, so the reason stays.
    Flipped { resolved: bool },
    /// GitHub took the change and the refresh after it read no mirror: a refresh that reads
    /// the pull request (`PrRead::fresh`), whether or not a draft's pins then moved.
    Read,
}

impl GithubState {
    pub(super) fn is_settled(&self) -> bool {
        self.reading.is_none() && self.resolving.is_empty()
    }
}

/// What asking for a Resolve or an Unresolve of a GitHub thread came to
/// ([`ReviewSession::toggle_github_resolved`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveAsked {
    /// Asked of GitHub, or waiting its turn: a Resolve (`true`) or an Unresolve.
    Asked(bool),
    /// GitHub doesn't let the viewer resolve (`true`) or unresolve this thread.
    NotAllowed(bool),
    /// There is no such thread, or a call for it is already on its way.
    Nothing,
}

impl ReviewSession {
    /// GitHub's threads on the review's pull request, in GitHub's order; none for a local
    /// review, and before the pull request was read.
    pub fn github_threads(&self) -> &[GithubShown] {
        &self.github.shown
    }

    /// The GitHub thread with node id `node`.
    pub fn github_thread(&self, node: &str) -> Option<&GithubShown> {
        self.github
            .shown
            .iter()
            .find(|shown| shown.thread.id == node)
    }

    /// GitHub's thread `node` on GitHub, when its URL is a page of the pull request's host
    /// (`GithubThread::url_on`): what "Open on GitHub" opens and a prompt names. `None`
    /// leaves the action out.
    pub fn github_url(&self, node: &str) -> Option<&str> {
        let host = &self.pr()?.mirror.as_ref()?.key.host;
        self.github_thread(node)?.thread.url_on(host)
    }

    /// Every thread the review shows: its own live ones in the order they were added, then
    /// GitHub's in GitHub's order.
    pub fn cards(&self) -> impl Iterator<Item = Card<'_>> {
        self.review
            .live_threads()
            .map(Card::Local)
            .chain(self.github.shown.iter().map(Card::Github))
    }

    /// The thread `key` names, when the review shows it (a deleted thread isn't).
    pub fn card(&self, key: &ThreadKey) -> Option<Card<'_>> {
        match key {
            ThreadKey::Local(id) => self
                .review
                .thread(id)
                .filter(|thread| !thread.is_deleted())
                .map(Card::Local),
            ThreadKey::Github(node) => self.github_thread(node).map(Card::Github),
        }
    }

    /// Whether a Resolve (`Some(true)`) or an Unresolve (`Some(false)`) of GitHub's thread
    /// `node` is running.
    pub fn resolving(&self, node: &str) -> Option<bool> {
        self.github.resolving.get(node).copied()
    }

    /// Why the last Resolve or Unresolve of GitHub's thread `node` failed, or that GitHub
    /// took it and the refresh after it failed: its card's error row.
    pub fn github_error(&self, node: &str) -> Option<&Failure> {
        self.github
            .error
            .as_ref()
            .filter(|error| error.node == node)
            .map(|error| &error.failure)
    }

    /// Resolves GitHub's thread `node`, or unresolves it when GitHub calls it resolved (PLAN
    /// §9.6): only where GitHub allows the viewer, and not while a call for it is on its
    /// way. The call runs in the pull request's turn (`pr`, "One at a time"); the thread
    /// is [`ReviewSession::resolving`] from now.
    pub fn toggle_github_resolved(&mut self, node: &str, cx: &mut Context<Self>) -> ResolveAsked {
        let Some(shown) = self.github_thread(node) else {
            return ResolveAsked::Nothing;
        };
        let thread = &shown.thread;
        let resolved = !thread.resolved;
        let allowed = if resolved {
            thread.viewer_can_resolve
        } else {
            thread.viewer_can_unresolve
        };
        if self.github.resolving.contains_key(node) || cx.try_services().is_none() {
            return ResolveAsked::Nothing;
        }
        if !allowed {
            return ResolveAsked::NotAllowed(resolved);
        }
        self.github.resolving.insert(node.to_string(), resolved);
        if self.github_error(node).is_some() {
            self.github.error = None;
        }
        self.ask_pr(
            PrOp::Resolve {
                node: node.to_string(),
                resolved,
            },
            cx,
        );
        cx.emit(SessionEvent::PrChanged);
        ResolveAsked::Asked(resolved)
    }

    /// Asks GitHub to resolve (`resolved`) or unresolve its thread `node`, as the pull
    /// request's turn comes to it. `None` when there is nothing to ask any more: what ran
    /// before it took the thread away, or left it in the state asked for.
    pub(super) fn start_resolve(
        &mut self,
        node: &str,
        resolved: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<()>> {
        let key = self
            .pr()
            .and_then(|read| read.mirror.as_ref())
            .map(|mirror| mirror.key.clone());
        let wanted = self
            .github_thread(node)
            .is_some_and(|shown| shown.thread.resolved != resolved);
        let github = cx.try_services().map(|services| services.github.clone());
        let (Some(key), true, Some(github)) = (key, wanted, github) else {
            self.github.resolving.remove(node);
            cx.emit(SessionEvent::PrChanged);
            return None;
        };
        let (backend, review, node) = (self.backend.clone(), self.review.clone(), node.to_string());
        let asked = github.update(cx, |github, cx| {
            github.set_resolved(backend, review, key, node.clone(), resolved, cx)
        });
        Some(cx.spawn(async move |this, cx| {
            let done = asked.await;
            this.update(cx, |this, cx| {
                this.github.resolving.remove(&node);
                match done {
                    Ok(Resolved {
                        resolved,
                        refreshed,
                    }) => {
                        let unread =
                            (!refreshed.fresh).then(|| refreshed.stale.as_ref().map(Failure::from));
                        this.apply_pr_read(PrRead::from(refreshed), cx);
                        if let Some(why) = unread {
                            this.keep_github_answer(&node, resolved, why, cx);
                        }
                    }
                    Err(failure) => {
                        this.github.error = Some(ResolveError {
                            node,
                            failure,
                            until: Until::Flipped {
                                resolved: !resolved,
                            },
                        });
                        cx.emit(SessionEvent::PrChanged);
                    }
                }
                this.pr_done(cx);
            })
            .ok();
        }))
    }

    /// GitHub took a Resolve (`resolved`) or an Unresolve of thread `node`, and the refresh
    /// after it read no mirror, for `why`: the thread shows GitHub's answer, written into
    /// the mirror the session shows when the refresh didn't already, and its card says what
    /// happened (module docs).
    fn keep_github_answer(
        &mut self,
        node: &str,
        resolved: bool,
        why: Option<Failure>,
        cx: &mut Context<Self>,
    ) {
        if let Some(mirror) = self.pr.read.as_mut().and_then(|read| read.mirror.as_mut())
            && let Some(index) = mirror
                .threads
                .iter()
                .position(|thread| thread.id == node && thread.resolved != resolved)
        {
            let thread = &mut Arc::make_mut(mirror).threads[index];
            thread.resolved = resolved;
            thread.viewer_can_resolve = !resolved;
            thread.viewer_can_unresolve = resolved;
        }
        self.read_github(cx);
        let did = if resolved { "Resolved" } else { "Unresolved" };
        let why = why.unwrap_or_else(|| Failure::new("the pull request wasn't read again"));
        self.github.error = Some(ResolveError {
            node: node.to_string(),
            failure: Failure {
                message: format!("{did} on GitHub, but the refresh failed: {}", why.message),
                ..why
            },
            until: Until::Read,
        });
        cx.emit(SessionEvent::PrChanged);
        cx.notify();
    }

    /// Brings GitHub's threads into line with the mirror shown and the sides the review shows
    /// (module docs, "Anchors"): the threads the mirror holds, each with the anchor it had,
    /// at once; then their anchors read again in the background when either changed.
    pub(super) fn read_github(&mut self, cx: &mut Context<Self>) {
        let mirror = self.pr().and_then(|read| read.mirror.clone());
        let (Some(mirror), Some(spec)) = (mirror, self.resolved.clone()) else {
            return;
        };
        if let Some((read, sides)) = &self.github.basis
            && Arc::ptr_eq(read, &mirror)
            && *sides == spec
        {
            return;
        }
        let mut had: HashMap<String, GithubShown> = std::mem::take(&mut self.github.shown)
            .into_iter()
            .map(|shown| (shown.thread.id.clone(), shown))
            .collect();
        self.github.shown = mirror
            .threads
            .iter()
            .filter_map(|thread| {
                let shown = had.remove(&thread.id)?;
                Some(GithubShown {
                    thread: thread.clone(),
                    ..shown
                })
            })
            .collect();
        // What a failed Resolve left on its card goes when this mirror answers it (`Until`).
        let read = self.pr().is_some_and(|read| read.fresh);
        let over = self.github.error.as_ref().is_some_and(|error| {
            let thread = mirror.threads.iter().find(|thread| thread.id == error.node);
            match (&error.until, thread) {
                (_, None) => true,
                (Until::Flipped { resolved }, Some(thread)) => thread.resolved != *resolved,
                (Until::Read, Some(_)) => read,
            }
        });
        if over {
            self.github.error = None;
        }
        self.github.basis = Some((mirror.clone(), spec.clone()));
        let (repo, texts) = (self.repo.clone(), self.github.texts.clone());
        let job = cx.background_spawn({
            let (mirror, spec) = (mirror.clone(), spec.clone());
            async move { anchor_threads(&repo, &mirror, &spec, &texts) }
        });
        self.github.reading = Some(cx.spawn(async move |this, cx| {
            let anchors = job.await;
            this.update(cx, |this, cx| {
                // A newer mirror or other sides are being read for.
                let current = this
                    .github
                    .basis
                    .as_ref()
                    .is_some_and(|(read, sides)| Arc::ptr_eq(read, &mirror) && *sides == spec);
                if !current {
                    return;
                }
                this.github.reading = None;
                this.github.shown = mirror
                    .threads
                    .iter()
                    .zip(anchors)
                    .map(|(thread, anchored)| GithubShown {
                        thread: thread.clone(),
                        anchor: anchored.anchor,
                        standing: anchored.standing,
                    })
                    .collect();
                this.threads_changed(cx);
                cx.emit(SessionEvent::PrChanged);
                cx.notify();
            })
            .ok();
        }));
        self.threads_changed(cx);
        cx.emit(SessionEvent::PrChanged);
        cx.notify();
    }
}
