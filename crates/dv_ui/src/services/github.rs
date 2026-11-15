//! GitHub in the app (PLAN §11 `GitHub`, §9): the app's one `gh` runner
//! (`dv_core::github::Gh`), and the opens and refreshes of pull requests it runs off the UI
//! thread.
//!
//! One runner for the whole app, so its pace holds across every review (calls one at a time,
//! mutations a second apart, PLAN §9.2) and the viewer's login is asked once a session. It
//! runs `gh` on this machine for every repository, a WSL one too (PLAN §9.2): only `git
//! fetch` runs where the repository lives, through its backend.
//!
//! The program is the setup check's (`SetupConfig`'s seams): `$DV_GH`, else `gh`; an
//! automation run or a test without `DV_GH` names a program that isn't there, so nothing
//! automated reaches the real `gh` (CLAUDE.md).

use std::sync::Arc;

use dv_cli::checks::Seams;
use dv_core::Backend;
use dv_core::github::{self, Gh, GhOptions, Mirror, OpenedPr, PrRef, Refreshed};
use dv_core::review::{Pins, Review, reviewer_identity};
use futures::channel::oneshot;
use gpui::{AppContext as _, Context, Task};

use crate::failure::Failure;

/// What a refresh of a PR review found, as a session keeps it (`dv_core::github::Refreshed`
/// with its error ready for a banner).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrRead {
    /// The mirror to show: the one the refresh wrote, or the last one stored when it failed.
    pub mirror: Option<Arc<Mirror>>,
    /// Why the refresh failed, which makes `mirror` stale.
    pub stale: Option<Failure>,
    /// The review's pins after it.
    pub pins: Option<Pins>,
    /// Where the pins stood before, when the refresh moved them.
    pub moved_from: Option<Pins>,
}

impl From<Refreshed> for PrRead {
    fn from(refreshed: Refreshed) -> Self {
        Self {
            mirror: refreshed.mirror.map(|mirror| Arc::new(*mirror)),
            stale: refreshed.stale.as_ref().map(Failure::from),
            pins: refreshed.pins,
            moved_from: refreshed.moved_from,
        }
    }
}

/// The GitHub service (module docs).
pub struct GitHub {
    gh: Arc<Gh>,
    /// Opens and refreshes running.
    running: usize,
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
        }
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
        let job = cx.background_spawn(async move { work() });
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
