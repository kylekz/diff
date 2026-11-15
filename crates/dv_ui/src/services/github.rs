//! GitHub in the app (PLAN §11 `GitHub`, §9): the app's one `gh` runner
//! (`dv_core::github::Gh`), and what it runs off the UI thread: the opens and refreshes of
//! pull requests, Resolve / Unresolve of GitHub's threads, and the PR picker's lists.
//!
//! One runner for the whole app, so its pace holds across every review (calls one at a time,
//! mutations a second apart, PLAN §9.2). The viewer's login is asked by every operation,
//! never kept (PLAN §9.3 step 1). It
//! runs `gh` on this machine for every repository, a WSL one too (PLAN §9.2): only `git
//! fetch` runs where the repository lives, through its backend.
//!
//! The program is the setup check's (`SetupConfig`'s seams): `$DV_GH`, else `gh`; an
//! automation run or a test without `DV_GH` names a program that isn't there, so nothing
//! automated reaches the real `gh` (CLAUDE.md).
//!
//! **The picker's lists** (PLAN §9.9): a repository's open pull requests ([`PrList`]), kept
//! per repository for the app's session and read again each time the picker opens
//! ([`GitHub::refresh_list`]): the list read before shows meanwhile. Reading one tells which
//! GitHub repository the clone is (PLAN §9.1) and runs the two `gh pr list` calls
//! (`dv_core::github::list`). A read that lands for a repository belongs to that repository's
//! list whatever the picker shows by then, and while one runs for a repository no other is
//! started for it, however often the picker opens meanwhile. A list is one GitHub
//! repository's: when a read finds the clone is another repository than the list was read
//! from (`gh repo set-default`, a remote re-pointed), what was listed goes, whether or not
//! the new repository's list could be read ([`PrList::apply`]). Nothing here runs because of
//! what is typed: filtering a list and matching a reference use what is held.

use std::collections::HashMap;
use std::sync::Arc;

use dv_cli::checks::Seams;
use dv_core::github::{
    self, Gh, GhOptions, GithubRepo, ListedPr, Listing, Mirror, MirrorKey, OpenedPr, PrRef,
    Refreshed, RepoUrl, Resolved,
};
use dv_core::review::{Pins, Review, reviewer_identity};
use dv_core::{Backend, Location};
use futures::channel::oneshot;
use gpui::{AppContext as _, Context, Task};

use crate::failure::Failure;

/// What a refresh of a PR review found, as a session keeps it (`dv_core::github::Refreshed`
/// with its error ready for a banner).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrRead {
    /// The mirror to show: the one the refresh wrote, or the last one stored when it failed.
    pub mirror: Option<Arc<Mirror>>,
    /// Whether `mirror` is one read from GitHub by this refresh (`Refreshed::fresh`), or by
    /// the refresh of another process the session followed: what GitHub holds now, though a
    /// draft's pins may not have moved (`stale`).
    pub fresh: bool,
    /// Why the refresh failed, which makes `mirror` stale.
    pub stale: Option<Failure>,
    /// The review's pins after it.
    pub pins: Option<Pins>,
    /// Where the pins stood before, when the refresh moved them.
    pub moved_from: Option<Pins>,
}

impl PrRead {
    /// What was last read of the pull request and stored, shown before this session's first
    /// refresh lands (PLAN §9.4): no read of this session's, and no failure either.
    pub fn stored(mirror: Mirror, pins: Option<Pins>) -> Self {
        Self {
            mirror: Some(Arc::new(mirror)),
            pins,
            ..Self::default()
        }
    }

    /// Whether what shows is [`PrRead::stored`]'s: a mirror nothing has read again yet.
    pub fn is_stored(&self) -> bool {
        self.mirror.is_some() && !self.fresh && self.stale.is_none()
    }
}

impl From<Refreshed> for PrRead {
    fn from(refreshed: Refreshed) -> Self {
        Self {
            mirror: refreshed.mirror.map(|mirror| Arc::new(*mirror)),
            fresh: refreshed.fresh,
            stale: refreshed.stale.as_ref().map(Failure::from),
            pins: refreshed.pins,
            moved_from: refreshed.moved_from,
        }
    }
}

/// Why a repository's pull requests couldn't be listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListFailure {
    /// What kind of trouble it is, which the picker words its state by.
    pub kind: ListFailureKind,
    /// The one line, with the command that fixes it when there is one, and gh's own words
    /// as its detail.
    pub failure: Failure,
}

/// The kinds of [`ListFailure`] the picker tells apart (DESIGN §4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListFailureKind {
    /// The clone has no repository on GitHub dv can pick (PLAN §9.1).
    NotOnGithub,
    /// `gh` isn't installed.
    GhMissing,
    /// `gh` isn't signed in to the repository's host.
    SignedOut,
    /// The repository can't be reached where it lives (a stopped distro, a lost connection).
    Unreachable,
    /// Anything else: GitHub out of reach, a rate limit, an answer dv can't read.
    Other,
}

impl ListFailureKind {
    /// `"not_on_github"`, `"gh_missing"`, `"signed_out"`, `"unreachable"` or `"other"`, as
    /// automation state spells it.
    pub fn key(self) -> &'static str {
        match self {
            Self::NotOnGithub => "not_on_github",
            Self::GhMissing => "gh_missing",
            Self::SignedOut => "signed_out",
            Self::Unreachable => "unreachable",
            Self::Other => "other",
        }
    }
}

impl From<&github::GithubError> for ListFailure {
    fn from(error: &github::GithubError) -> Self {
        use github::GithubError as E;
        let kind = match error {
            E::NoRepository { .. }
            | E::NoRemote { .. }
            | E::UnreadableDefault { .. }
            | E::NotGithub { .. } => ListFailureKind::NotOnGithub,
            E::GhMissing { .. } => ListFailureKind::GhMissing,
            E::NotAuthenticated { .. } => ListFailureKind::SignedOut,
            other if other.is_disconnected() => ListFailureKind::Unreachable,
            _ => ListFailureKind::Other,
        };
        Self {
            kind,
            failure: Failure::from(error),
        }
    }
}

/// A repository's open pull requests as the picker lists them (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrList {
    /// The repository on GitHub the list is of, once a read told it.
    pub repo: Option<GithubRepo>,
    /// The last list read, the ones asking for the viewer's review first; `None` before
    /// the first one lands.
    pub prs: Option<Arc<Vec<ListedPr>>>,
    /// When `prs` was read, in ms since the Unix epoch.
    pub read_at: Option<u64>,
    /// A read is running.
    pub loading: bool,
    /// Why the last read failed; `prs` is then the list read before it, if any.
    pub failure: Option<ListFailure>,
    /// Why the last read couldn't tell which pull requests ask for the viewer's review:
    /// `prs` is then the open ones, none of them marked.
    pub unmarked: Option<Failure>,
}

impl PrList {
    /// The pull request `number` of this list as a reference that names the list's own
    /// repository: what a listed row opens, so a clone that became another repository
    /// since the list was read is refused by the open's own check (PLAN §9.3), never
    /// opened as that repository's pull request of the same number.
    pub fn reference(&self, number: u64) -> PrRef {
        match &self.repo {
            Some(repo) => PrRef::Url {
                host: repo.host.clone(),
                owner: repo.owner.clone(),
                repo: repo.repo.clone(),
                number,
            },
            None => PrRef::Hash(number),
        }
    }

    /// Takes what a read found, at `now` (module docs).
    pub(crate) fn apply(&mut self, read: ListRead, now: u64) {
        self.loading = false;
        // Another repository than the one listed: its rows aren't this one's.
        let moved = match (&self.repo, &read.repo) {
            (Some(listed), Some(found)) => !listed.is(&found.host, &found.owner, &found.repo),
            _ => false,
        };
        if moved {
            self.prs = None;
            self.read_at = None;
            self.unmarked = None;
        }
        if read.repo.is_some() {
            self.repo = read.repo;
        }
        match read.prs {
            Ok(listing) => {
                self.prs = Some(Arc::new(listing.prs));
                self.read_at = Some(now);
                self.failure = None;
                self.unmarked = listing.requested.as_ref().map(Failure::from);
            }
            Err(failure) => {
                // A clone that is no longer on GitHub has no list to show.
                if failure.kind == ListFailureKind::NotOnGithub {
                    self.repo = None;
                    self.prs = None;
                    self.read_at = None;
                    self.unmarked = None;
                }
                self.failure = Some(failure);
            }
        }
    }
}

/// What a read of a repository's list found.
pub(crate) struct ListRead {
    /// The repository the clone is, when the read could tell.
    pub(crate) repo: Option<GithubRepo>,
    pub(crate) prs: Result<Listing, ListFailure>,
}

/// The GitHub service (module docs).
pub struct GitHub {
    gh: Arc<Gh>,
    /// Opens, refreshes and lists running.
    running: usize,
    /// The picker's lists, by the repository's location; one read of each at a time
    /// ([`PrList::loading`]).
    lists: HashMap<Location, PrList>,
    /// What the jobs started while it is set wait for ([`GitHub::hold`]).
    #[cfg(test)]
    gate: Option<futures::future::Shared<oneshot::Receiver<()>>>,
}

impl GitHub {
    /// A service running `gh` as the setup checks do: `seams`' program and the environment
    /// they give the programs they run.
    pub fn new(seams: &Seams) -> Self {
        Self::with_gh(Gh::new(GhOptions {
            env: seams.child_env.clone(),
            ..GhOptions::new(seams.gh.clone())
        }))
    }

    /// A service over `gh` (tests: `mock_gh`).
    pub fn with_gh(gh: Gh) -> Self {
        Self {
            gh: Arc::new(gh),
            running: 0,
            lists: HashMap::new(),
            #[cfg(test)]
            gate: None,
        }
    }

    /// Holds every job started from now on until the sender hears or is dropped: a test's
    /// `gh` that hasn't answered yet, whatever the executor runs meanwhile.
    #[cfg(test)]
    pub fn hold(&mut self) -> oneshot::Sender<()> {
        let (sender, receiver) = oneshot::channel();
        self.gate = Some(futures::FutureExt::shared(receiver));
        sender
    }

    /// A service whose `gh` is a program that isn't there: tests that open no pull request.
    #[cfg(test)]
    pub fn without_gh() -> Self {
        Self::with_gh(Gh::new(GhOptions::new("dv-test-has-no-gh".into())))
    }

    /// The app's `gh` runner, for what runs it beside this service's own jobs.
    pub fn gh(&self) -> Arc<Gh> {
        self.gh.clone()
    }

    /// Nothing is being opened or refreshed: automation's `settled`.
    pub fn is_settled(&self) -> bool {
        self.running == 0
    }

    /// Opens the pull request `reference` names in the repository `backend` is bound to, as
    /// the reviewer (`dv_core::github::open`, PLAN §9.3): its commits fetched, its review
    /// found or made, its mirror refreshed. A failure names the pull request ("couldn't open
    /// #482: …"): its banner may show over another pull request's review.
    pub fn open(
        &mut self,
        backend: Arc<dyn Backend>,
        reference: PrRef,
        cx: &mut Context<Self>,
    ) -> Task<Result<OpenedPr, Failure>> {
        let gh = self.gh.clone();
        self.run(cx, move || {
            github::open(&backend, &gh, &reference, || {
                reviewer_identity(backend.as_ref())
            })
            .map_err(|error| {
                let failure = Failure::from(&error);
                Failure {
                    message: format!("couldn't open #{}: {}", reference.number(), failure.message),
                    ..failure
                }
            })
        })
    }

    /// Refreshes `review`, a PR review of the repository `backend` is bound to
    /// (`dv_core::github::refresh`, PLAN §9.4): a failure comes back in
    /// [`Refreshed::stale`], with the last mirror.
    pub fn refresh(
        &mut self,
        backend: Arc<dyn Backend>,
        review: Review,
        cx: &mut Context<Self>,
    ) -> Task<Refreshed> {
        let gh = self.gh.clone();
        let job = self.run(cx, move || Ok(github::refresh(&backend, &gh, &review)));
        // The app closing cuts a refresh short: nothing read, nothing moved.
        cx.background_spawn(async move { job.await.unwrap_or_default() })
    }

    /// Resolves (`resolved`) or unresolves GitHub's thread `thread` of `review`'s pull
    /// request, whose mirror is `key`'s, then refreshes the mirror
    /// (`dv_core::github::set_resolved`, PLAN §9.6): dv's one write to GitHub beside a
    /// submission. An error is the mutation's, and nothing changed; once GitHub took it,
    /// its answer comes back beside the refresh's outcome.
    pub fn set_resolved(
        &mut self,
        backend: Arc<dyn Backend>,
        review: Review,
        key: MirrorKey,
        thread: String,
        resolved: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<Resolved, Failure>> {
        let gh = self.gh.clone();
        self.run(cx, move || {
            github::set_resolved(&backend, &gh, &review, &key, &thread, resolved)
                .map_err(|error| Failure::from(&error))
        })
    }

    /// The list of the repository at `location`, as far as it was read this session.
    pub fn list(&self, location: &Location) -> Option<&PrList> {
        self.lists.get(location)
    }

    /// The title of pull request `number` in the list of the repository at `location`,
    /// when a list read this session holds it: nothing is asked.
    pub fn listed_title(&self, location: &Location, number: u64) -> Option<&str> {
        self.lists
            .get(location)?
            .prs
            .as_ref()?
            .iter()
            .find(|pr| pr.number == number)
            .map(|pr| pr.title.as_str())
    }

    /// The title of pull request `number` of the GitHub repository `repo`, when a list
    /// read this session for any clone of it holds it (the most recently read one):
    /// nothing is asked.
    pub fn title_of(&self, repo: &RepoUrl, number: u64) -> Option<&str> {
        self.lists
            .values()
            .filter(|list| {
                list.repo
                    .as_ref()
                    .is_some_and(|listed| repo.is(Some(&listed.host), &listed.owner, &listed.repo))
            })
            .filter_map(|list| {
                let pr = list.prs.as_ref()?.iter().find(|pr| pr.number == number)?;
                Some((list.read_at, pr.title.as_str()))
            })
            .max_by_key(|(read_at, _)| *read_at)
            .map(|(_, title)| title)
    }

    /// Whether a read of the list of the repository at `location` runs.
    pub fn is_listing(&self, location: &Location) -> bool {
        self.lists.get(location).is_some_and(|list| list.loading)
    }

    /// Reads the open pull requests of the repository at `location` again, in the
    /// background (module docs): `backend` gives the repository's, as a listing reaches it
    /// ([`crate::services::Locations::reach`]), or why it can't be reached. The list read
    /// before shows until this one lands. Nothing is started, `backend` not even asked for,
    /// while a read of that list runs.
    pub fn refresh_list(
        &mut self,
        location: Location,
        backend: impl FnOnce(&mut Context<Self>) -> Task<Result<Arc<dyn Backend>, Failure>>,
        cx: &mut Context<Self>,
    ) {
        if self.is_listing(&location) {
            return;
        }
        self.lists.entry(location.clone()).or_default().loading = true;
        self.running += 1;
        cx.notify();
        let backend = backend(cx);
        let gh = self.gh.clone();
        #[cfg(test)]
        let gate = self.gate.clone();
        cx.spawn(async move |this, cx| {
            let read = match backend.await {
                Ok(backend) => {
                    cx.background_spawn(async move {
                        #[cfg(test)]
                        if let Some(gate) = gate {
                            let _ = gate.await;
                        }
                        match GithubRepo::of(backend.as_ref(), &gh) {
                            Ok(repo) => ListRead {
                                prs: github::list(&gh, &repo)
                                    .map_err(|error| ListFailure::from(&error)),
                                repo: Some(repo),
                            },
                            Err(error) => ListRead {
                                repo: None,
                                prs: Err(ListFailure::from(&error)),
                            },
                        }
                    })
                    .await
                }
                Err(failure) => ListRead {
                    repo: None,
                    prs: Err(ListFailure {
                        kind: ListFailureKind::Unreachable,
                        failure,
                    }),
                },
            };
            this.update(cx, |this, cx| {
                this.running -= 1;
                this.lists
                    .entry(location)
                    .or_default()
                    .apply(read, dv_core::review::event::now_ms());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Runs `work` on a background thread, counted until it ends whether or not the caller
    /// keeps the task, which only carries the result.
    fn run<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce() -> Result<T, Failure> + Send + 'static,
    ) -> Task<Result<T, Failure>> {
        self.running += 1;
        cx.notify();
        let (sender, receiver) = oneshot::channel();
        #[cfg(test)]
        let gate = self.gate.clone();
        let job = cx.background_spawn(async move {
            #[cfg(test)]
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            work()
        });
        cx.spawn(async move |this, cx| {
            let done = job.await;
            this.update(cx, |this, cx| {
                this.running -= 1;
                cx.notify();
            })
            .ok();
            let _ = sender.send(done);
        })
        .detach();
        cx.background_spawn(async move {
            receiver
                .await
                .unwrap_or_else(|_| Err(Failure::new("dv is closing")))
        })
    }
}

#[cfg(test)]
mod tests;
