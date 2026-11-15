//! Pull requests in the CLI (docs/PLAN.md §9.10): the `gh` a command runs, a PR review's
//! `pr` document, and what an agent is told when it writes to one.
//!
//! The CLI runs `gh` where it runs (PLAN §9.2): inside the distro for a WSL repository, with
//! the distro's own login. The program is `$DV_GH`, else `gh`, as for the setup check
//! (`crate::checks`).

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use dv_core::github::fetch::refs_prefix;
use dv_core::github::{Gh, GhOptions, Mirror, MirrorStore, PrState, program};
use dv_core::review::{
    AuthorKind, GITHUB_PREFIX, ListEntry, OpError, PrLink, Review, ReviewId, ops, pins,
};
use dv_core::target::Target;

use crate::args::GlobalArgs;
use crate::checks::GH_VAR;
use crate::context::{Context, Env};
use crate::dto::{GithubCountsDto, PrDto};
use crate::error::{CliError, Exit, code};
use crate::identity;
use crate::repo::Repo;
use crate::select;

/// The `gh` runner of a command run with `ctx`: `$DV_GH` or `gh`, with an explicit
/// environment's variables (a test's `DV_MOCK_GH`) and the git overlay handed down, as the
/// setup checks hand them to the programs they run.
pub(crate) fn gh(ctx: &Context) -> Gh {
    let mut env: Vec<(OsString, Option<OsString>)> = Vec::new();
    if let Env::Vars(vars) = &ctx.env {
        env.extend(
            vars.iter()
                .map(|(name, value)| (OsString::from(name), Some(OsString::from(value)))),
        );
    }
    env.extend(ctx.git_env.iter().cloned());
    Gh::new(GhOptions {
        env,
        ..GhOptions::new(program(ctx.env.non_empty(GH_VAR).map(OsString::from)))
    })
}

/// What a command that writes names, for [`refuse_agent`].
pub(crate) enum Written<'a> {
    /// The review the command means: its argument (`dv review done <id>`), `--review`,
    /// `$DV_REVIEW`, or the working directory's (`crate::select::review`).
    Review(Option<&'a str>),
    /// A thread or a reply by its id, in `--review`'s review or whichever review holds it
    /// (`crate::select::item`).
    Item(&'a str),
}

/// Refuses a write an agent asks for on a PR review: exit 1, `not_allowed` (PLAN §9.10).
/// Asked before anything else about the command is looked at (its text, the file or the
/// thread it names), so an agent hears the same answer whatever else is wrong with its
/// command line. Whatever keeps the review from being told here (no repository, no such
/// review, none in context, an id that names nothing) is the command's own to report;
/// `review::ops` refuses the write too.
///
/// A write to one of GitHub's threads (`gh:<node id>`, or one of its comments) is refused
/// here too, whoever asks: only a PR review shows them, so an agent hears what it hears of
/// any write to one, and the reviewer that the CLI writes nothing to GitHub's threads.
pub(crate) fn refuse_agent(
    global: &GlobalArgs,
    ctx: &Context,
    written: Written,
) -> Result<(), CliError> {
    let agent = identity::agent(global, ctx).ok().flatten();
    if let Written::Item(id) = written
        && id.trim().starts_with(GITHUB_PREFIX)
    {
        return Err(match agent {
            Some(agent) => pr_reviews_are_the_reviewers(agent.kind),
            None => CliError::new(
                Exit::Failure,
                code::NOT_ALLOWED,
                format!(
                    "{} is GitHub's, and the CLI writes nothing to GitHub's threads: resolve \
                     it in the dv app, or answer it on GitHub (`dv comment show` has its URL)",
                    shown_id(id)
                ),
            ),
        });
    }
    let Some(agent) = agent else {
        return Ok(());
    };
    let Ok(repo) = Repo::open(global, ctx) else {
        return Ok(());
    };
    let review = match (written, &global.review) {
        (Written::Review(named), _) => select::review(&repo, global, ctx, named).ok(),
        // `--review` names the review the thread is looked for in.
        (Written::Item(_), Some(_)) => select::review(&repo, global, ctx, None).ok(),
        (Written::Item(id), None) => select::item(&repo, global, id)
            .ok()
            .map(|(review, _)| review),
    };
    match review {
        Some(review) if review.is_pr_review() => Err(pr_reviews_are_the_reviewers(agent.kind)),
        _ => Ok(()),
    }
}

/// What an agent's write to a PR review is refused with.
fn pr_reviews_are_the_reviewers(actor: AuthorKind) -> CliError {
    OpError::NotAllowed {
        actor,
        action: ops::PR_REVIEWS_ARE_THE_REVIEWERS.to_string(),
    }
    .into()
}

/// What a command's PR reviews are printed from, read once however many it prints
/// ([`Repo::stored_prs`]): the store's PR reviews and the hosts a mirror may be kept under.
/// Nothing here asks `gh`: a command that only reads never starts it (PLAN §9.10).
pub(crate) struct StoredPrs {
    /// Every PR review in the store with its target, oldest first: ids lead with their time.
    reviews: Vec<(ReviewId, Target)>,
    hosts: Vec<String>,
    /// Each pull request's stored mirror, by the pull request's ref namespace
    /// (`refs_prefix`: one spelling per pull request), read when its first review is
    /// printed; `None` for one without a mirror.
    mirrors: Mutex<HashMap<String, Option<Arc<Mirror>>>>,
}

impl StoredPrs {
    /// Read from `repo`'s store and its clone's remotes (blocking: one listing, a few git
    /// calls). What can't be read is left out, as in [`dto`].
    pub fn read(repo: &Repo) -> Self {
        let mut reviews: Vec<_> = repo
            .store
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| match entry {
                ListEntry::Ok(summary) if matches!(summary.target, Target::Pr { .. }) => {
                    Some((summary.id, summary.target))
                }
                _ => None,
            })
            .collect();
        reviews.sort_by(|a, b| a.0.cmp(&b.0));
        Self {
            reviews,
            hosts: MirrorStore::new(repo.backend.clone())
                .hosts()
                .unwrap_or_default(),
            mirrors: Mutex::default(),
        }
    }

    /// `link`'s pull request as its most recently read mirror has it, whichever login read
    /// it and under whichever host a remote names (blocking the first time a pull request is
    /// asked about: its mirror files).
    fn mirror(&self, repo: &Repo, link: &PrLink) -> Option<Arc<Mirror>> {
        let mut mirrors = self
            .mirrors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        mirrors
            .entry(refs_prefix(&link.owner, &link.repo, link.number))
            .or_insert_with(|| {
                MirrorStore::new(repo.backend.clone())
                    .latest_stored(&self.hosts, link)
                    .ok()
                    .flatten()
                    .map(Arc::new)
            })
            .clone()
    }
}

/// The stored mirror of `review`'s pull request, which GitHub's threads on it are read from
/// (PLAN §9.10): the most recently read one. `None` for a local review, and for a pull
/// request without a mirror to read here ([`why_no_mirror`]). No `gh` runs.
pub(crate) fn mirror(repo: &Repo, review: &Review) -> Option<Arc<Mirror>> {
    repo.stored_prs().mirror(repo, review.pr_target()?)
}

/// Why `review`, a PR review without a [`mirror`], has none, and what reads one: its pull
/// request "was never read here", or, when something is stored that isn't a mirror this
/// build reads (an empty or cut file, another version's), what was last read of it "can't
/// be read"; then `dv review refresh <id>`, which reads it again either way.
pub(crate) fn why_no_mirror(repo: &Repo, review: &Review) -> String {
    let stored = review.pr_target().is_some_and(|link| {
        MirrorStore::new(repo.backend.clone()).holds_any(&repo.stored_prs().hosts, link)
    });
    let (why, reads) = if stored {
        ("what dv last read of it can't be read", "reads it again")
    } else {
        ("it was never read here", "reads it")
    };
    format!("{why}; `dv review refresh {}` {reads}", review.id)
}

/// `id`, a thread's id as it was typed, for a message: whole up to [`ID_SHOWN`] characters,
/// else its start and "…". GitHub's node ids are a few dozen characters; what is longer
/// names nothing and would only fill the line.
pub(crate) fn shown_id(id: &str) -> String {
    let id = id.trim();
    if id.chars().count() <= ID_SHOWN {
        id.to_string()
    } else {
        let start: String = id.chars().take(ID_SHOWN - 1).collect();
        format!("{start}…")
    }
}

/// How many characters of an id a message shows.
const ID_SHOWN: usize = 80;

/// The `pr` document of `review`, a PR review; `None` for a local one (blocking: the
/// review's pins and, once per command, [`StoredPrs`] and the pull request's stored mirror).
/// Only what is stored is read, never `gh`. What can't be read is left out rather than
/// failing the command: the head falls back to the one the review was made at, the URL and
/// the state, which only a mirror has, to `null`.
pub(crate) fn dto(repo: &Repo, review: &Review) -> Option<PrDto> {
    let (link, Target::Pr { head_oid, .. }) = (review.pr_target()?, &review.target) else {
        return None;
    };
    let head = match pins::read(&repo.git, &review.id) {
        Ok(Some(pins)) => pins.head.to_string(),
        _ => head_oid.to_string(),
    };
    let stored = repo.stored_prs();
    let round = stored
        .reviews
        .iter()
        .filter(|(_, target)| link.is_target(target))
        .position(|(id, _)| *id == review.id)
        .map_or(1, |index| index + 1);
    let mirror = stored.mirror(repo, link);
    Some(PrDto {
        owner: link.owner.clone(),
        repo: link.repo.clone(),
        number: link.number,
        url: mirror.as_ref().map(|mirror| mirror.pr.url.clone()),
        state: mirror.as_ref().map(|mirror| match mirror.pr.state {
            PrState::Open => "open",
            PrState::Draft => "draft",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
        }),
        head_oid: head,
        pr_round: u32::try_from(round).unwrap_or(u32::MAX),
        threads: mirror.as_deref().map(GithubCountsDto::of),
    })
}
