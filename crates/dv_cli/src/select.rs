//! Which review a command means (docs/PLAN.md §10.2): the id it was given (`dv review show
//! <id>` or `--review <id>`), else `$DV_REVIEW`, else the one open review of the work checked
//! out here (`dv_core::review::ops::in_context`: a review whose home is this worktree, a
//! `working` or `staged` review made in it or a `branch` review of the branch checked out);
//! else exit 2 with the repository's open reviews as candidates. Never a guess.
//!
//! A command that takes a thread id finds the thread's review itself (thread ids are unique
//! across a repository's reviews), so an agent never needs `--review` for one; `--review`
//! narrows the search to that review, `$DV_REVIEW` doesn't. One of GitHub's threads
//! (`gh:<node id>`, PLAN §9.6) is in no review's log: it is found in the stored mirror of a
//! PR review's pull request. Several reviews of one pull request hold it, so there
//! `$DV_REVIEW` says which is meant, when it names one of them; else the pull request's
//! open review comes before its finished ones.

use std::sync::Arc;

use dv_core::github::Mirror;
use dv_core::label::Labels;
use dv_core::review::{
    ItemId, ListEntry, Review, ReviewId, ReviewSummary, StoreError, Thread, ThreadId, ThreadKey,
    displayed_round, ops,
};
use dv_core::target::Target;

use crate::args::GlobalArgs;
use crate::context::Context;
use crate::dto::{CandidateDto, UnreadableDto};
use crate::error::{CliError, Exit, code};
use crate::repo::Repo;

/// The repository's reviews as the store lists them: summaries newest first, and the logs
/// that don't load.
pub(crate) struct Listing {
    pub summaries: Vec<ReviewSummary>,
    pub unreadable: Vec<UnreadableDto>,
}

impl Listing {
    pub fn read(repo: &Repo) -> Result<Self, CliError> {
        let mut listing = Self {
            summaries: Vec::new(),
            unreadable: Vec::new(),
        };
        for entry in repo.store.list()? {
            match entry {
                ListEntry::Ok(summary) => listing.summaries.push(*summary),
                ListEntry::Err { id, error } => listing.unreadable.push(UnreadableDto {
                    id: id.to_string(),
                    error: error.to_string(),
                }),
            }
        }
        Ok(listing)
    }

    /// The open reviews, newest first.
    pub fn open(&self) -> impl Iterator<Item = &ReviewSummary> {
        self.summaries
            .iter()
            .filter(|summary| summary.status.is_open())
    }
}

/// The review id the command line or the environment names, if any: `positional` (an
/// argument such as `dv review show <id>`), `--review`, then `$DV_REVIEW`. Naming two
/// different reviews is a usage error.
pub(crate) fn named(
    global: &GlobalArgs,
    ctx: &Context,
    positional: Option<&str>,
) -> Result<Option<ReviewId>, CliError> {
    if let (Some(arg), Some(flag)) = (positional, global.review.as_deref())
        && arg != flag
    {
        return Err(CliError::usage(format!(
            "two reviews named: {arg} and --review {flag}"
        )));
    }
    if let Some(id) = positional {
        return parse(id, "the review id").map(Some);
    }
    if let Some(id) = &global.review {
        return parse(id, "--review").map(Some);
    }
    if let Some(id) = ctx.env.non_empty("DV_REVIEW") {
        return parse(&id, "$DV_REVIEW").map(Some);
    }
    Ok(None)
}

fn parse(id: &str, source: &str) -> Result<ReviewId, CliError> {
    ReviewId::parse(id.trim()).map_err(|error| CliError::usage(format!("{source}: {error}")))
}

/// Loads a review by id; one the store doesn't have is `not_found`.
pub(crate) fn load(repo: &Repo, id: &ReviewId) -> Result<Review, CliError> {
    match repo.store.load(id) {
        Ok(loaded) => Ok(loaded.review),
        Err(StoreError::UnknownReview { .. }) => Err(CliError::not_found(format!(
            "no review {id} in {}",
            repo.root()
        ))),
        Err(error) => Err(error.into()),
    }
}

/// The review the command means (module docs).
pub(crate) fn review(
    repo: &Repo,
    global: &GlobalArgs,
    ctx: &Context,
    positional: Option<&str>,
) -> Result<Review, CliError> {
    if let Some(id) = named(global, ctx, positional)? {
        return load(repo, &id);
    }
    let listing = Listing::read(repo)?;
    match in_context(repo, &listing)?.as_slice() {
        [one] => load(repo, &one.id),
        matches => Err(unclear(repo, &listing, matches.len())),
    }
}

/// The PR review `dv review refresh` means (PLAN §9.10): the one named (`positional`,
/// `--review`, `$DV_REVIEW`), which must be a PR review, else the repository's one open PR
/// review; none or several is exit 2 with the open PR reviews as candidates. A PR review is
/// the same from every worktree, so no worktree is its context ([`review`]'s rule).
pub(crate) fn pr_review(
    repo: &Repo,
    global: &GlobalArgs,
    ctx: &Context,
    positional: Option<&str>,
) -> Result<Review, CliError> {
    if let Some(id) = named(global, ctx, positional)? {
        let review = load(repo, &id)?;
        if !review.is_pr_review() {
            return Err(CliError::usage(format!(
                "review {id} is a local review: only a PR review (`dv review new --pr <n>`) \
                 refreshes from GitHub"
            )));
        }
        return Ok(review);
    }
    let listing = Listing::read(repo)?;
    let open: Vec<&ReviewSummary> = listing
        .open()
        .filter(|summary| matches!(summary.target, Target::Pr { .. }))
        .collect();
    match open.as_slice() {
        [one] => load(repo, &one.id),
        [] => Err(CliError::new(
            Exit::Usage,
            code::NO_REVIEW,
            "no open PR review in this repository; open one with `dv review new --pr <n>`",
        )
        .with_candidates(Vec::new())),
        several => {
            let labels = repo.labels();
            Err(CliError::new(
                Exit::Usage,
                code::AMBIGUOUS_REVIEW,
                format!(
                    "{} open PR reviews in this repository; name one with --review <id>",
                    several.len()
                ),
            )
            .with_candidates(
                several
                    .iter()
                    .map(|summary| candidate(repo, summary, &labels))
                    .collect(),
            ))
        }
    }
}

/// The review the command means when it can tell, for `dv status`: the named one (which must
/// exist), else the one open review in context, else `None`.
pub(crate) fn current(
    repo: &Repo,
    global: &GlobalArgs,
    ctx: &Context,
    listing: &Listing,
) -> Result<Option<Review>, CliError> {
    if let Some(id) = named(global, ctx, None)? {
        return load(repo, &id).map(Some);
    }
    match in_context(repo, listing)?.as_slice() {
        [one] => load(repo, &one.id).map(Some),
        _ => Ok(None),
    }
}

/// The open reviews of the work checked out here: those whose home is this worktree
/// (`dv_core::review::ops::in_context`), the worktree their target resolves in.
fn in_context<'l>(repo: &Repo, listing: &'l Listing) -> Result<Vec<&'l ReviewSummary>, CliError> {
    Ok(ops::in_context(listing.open(), &repo.git)?)
}

/// Exit 2 for a command that can't tell which review it means: `matches` open reviews of the
/// current work (none, or several), with every open review of the repository as a candidate.
fn unclear(repo: &Repo, listing: &Listing, matches: usize) -> CliError {
    let labels = repo.labels();
    let candidates: Vec<CandidateDto> = listing
        .open()
        .map(|summary| candidate(repo, summary, &labels))
        .collect();
    let (code, message) = if matches > 1 {
        (
            code::AMBIGUOUS_REVIEW,
            format!(
                "{matches} open reviews match the work checked out here; name one with --review \
                 <id>"
            ),
        )
    } else if candidates.is_empty() {
        (
            code::NO_REVIEW,
            "no open review in this repository; the reviewer starts one in dv, or with `dv \
             review new`"
                .to_string(),
        )
    } else {
        (
            code::NO_REVIEW,
            "no open review of the work checked out here; name one with --review <id>".to_string(),
        )
    };
    CliError::new(Exit::Usage, code, message).with_candidates(candidates)
}

fn candidate(repo: &Repo, summary: &ReviewSummary, labels: &Labels) -> CandidateDto {
    // A PR review's place among its pull request's reviews in the store (PLAN §9.5).
    let pr_round = summary
        .pr
        .as_ref()
        .filter(|link| link.is_target(&summary.target))
        .map(|link| repo.stored_prs().round_of(link, &summary.id));
    CandidateDto {
        id: summary.id.to_string(),
        label: labels.label(&summary.target, &summary.worktree),
        status: summary.status.as_str(),
        round: summary.current_round,
        shown_round: displayed_round(summary.current_round, pr_round),
        worktree: summary.worktree.clone(),
    }
}

/// The review holding thread `id` and the thread (module docs): searched in `--review`'s
/// review when it is given, else in every review of the repository. A deleted thread is not
/// found.
pub(crate) fn thread(
    repo: &Repo,
    global: &GlobalArgs,
    id: &str,
) -> Result<(Review, ThreadId), CliError> {
    let id = ThreadId::parse(id.trim()).map_err(|error| CliError::usage(error.to_string()))?;
    let live = |review: &Review| {
        review
            .thread(&id)
            .is_some_and(|thread| !thread.is_deleted())
    };
    if let Some(review) = &global.review {
        let review = load(repo, &parse(review, "--review")?)?;
        return if live(&review) {
            Ok((review, id))
        } else {
            Err(CliError::not_found(format!(
                "no thread {id} in review {}",
                review.id
            )))
        };
    }
    for summary in Listing::read(repo)?.summaries {
        let review = match repo.store.load(&summary.id) {
            Ok(loaded) => loaded.review,
            // Deleted since it was listed.
            Err(StoreError::UnknownReview { .. }) => continue,
            Err(error) => return Err(error.into()),
        };
        if live(&review) {
            return Ok((review, id));
        }
    }
    Err(CliError::not_found(format!(
        "no thread {id} in the reviews of {}",
        repo.root()
    )))
}

/// A thread a reading command names, with the review that shows it.
pub(crate) enum Shown {
    /// One of the review's own.
    Local(Review, ThreadId),
    /// One of GitHub's on the review's pull request: the stored mirror that holds it, and
    /// its place among the mirror's threads.
    Github(Review, Arc<Mirror>, usize),
}

/// The thread `id` names for a command that reads it (`dv comment show`): a thread of a
/// review ([`thread`]), or, for `gh:<node id>`, one of GitHub's threads on a PR review's pull
/// request, read from its stored mirror (module docs). `--review` narrows to that review
/// alone. Without it, the first PR review whose mirror holds the thread: the one
/// `$DV_REVIEW` names, when it names a PR review of this repository, then the open ones,
/// then the newest. So `$DV_REVIEW` only chooses among reviews that hold the same thread (a
/// pull request's earlier submitted review and its open draft) and never keeps one from
/// being found: one that doesn't parse, or names no PR review here, chooses nothing, as it
/// is nothing to a thread of a review's own ([`thread`]).
pub(crate) fn shown(
    repo: &Repo,
    global: &GlobalArgs,
    ctx: &Context,
    id: &str,
) -> Result<Shown, CliError> {
    let key = ThreadKey::parse(id.trim()).map_err(|error| {
        CliError::usage(format!(
            "invalid thread id {:?}: {}",
            crate::pr::shown_id(id),
            error.reason
        ))
    })?;
    let ThreadKey::Github(node) = key else {
        return thread(repo, global, id).map(|(review, id)| Shown::Local(review, id));
    };
    let held = |review: &Review| {
        let mirror = crate::pr::mirror(repo, review)?;
        let index = mirror.threads.iter().position(|thread| thread.id == node)?;
        Some((mirror, index))
    };
    if let Some(review) = &global.review {
        let review = load(repo, &parse(review, "--review")?)?;
        let named = crate::pr::shown_id(id);
        return match held(&review) {
            Some((mirror, index)) => Ok(Shown::Github(review, mirror, index)),
            None if review.is_pr_review() && crate::pr::mirror(repo, &review).is_none() => {
                Err(CliError::not_found(format!(
                    "no thread {named} to show: review {} is of a pull request, and {}",
                    review.id,
                    crate::pr::why_no_mirror(repo, &review)
                )))
            }
            None => Err(CliError::not_found(format!(
                "no thread {named} on GitHub in what review {} last read of its pull request",
                review.id
            ))),
        };
    }
    let mut summaries: Vec<ReviewSummary> = Listing::read(repo)?
        .summaries
        .into_iter()
        .filter(|summary| matches!(summary.target, Target::Pr { .. }))
        .collect();
    let preferred = ctx
        .env
        .non_empty("DV_REVIEW")
        .and_then(|id| ReviewId::parse(id.trim()).ok());
    summaries.sort_by_key(|summary| {
        (
            Some(&summary.id) != preferred.as_ref(),
            !summary.status.is_open(),
        )
    });
    for summary in summaries {
        let review = match repo.store.load(&summary.id) {
            Ok(loaded) => loaded.review,
            Err(StoreError::UnknownReview { .. }) => continue,
            Err(error) => return Err(error.into()),
        };
        if let Some((mirror, index)) = held(&review) {
            return Ok(Shown::Github(review, mirror, index));
        }
    }
    Err(CliError::not_found(format!(
        "no thread {} on GitHub in what the PR reviews of {} last read; `dv review \
         refresh` reads a pull request again",
        crate::pr::shown_id(id),
        repo.root()
    )))
}

/// The review holding the thread or reply `id` (`dv comment edit | delete`), found as
/// [`thread`] finds a thread: in `--review`'s review, else in every review of the repository.
/// A deleted item, or a reply in a deleted thread, is not found.
pub(crate) fn item(
    repo: &Repo,
    global: &GlobalArgs,
    id: &str,
) -> Result<(Review, ItemId), CliError> {
    let id = ItemId::parse(id.trim()).map_err(|error| CliError::usage(error.to_string()))?;
    let live = |review: &Review| match &id {
        ItemId::Thread(thread) => review
            .thread(thread)
            .is_some_and(|thread| !thread.is_deleted()),
        ItemId::Comment(reply) => review
            .live_threads()
            .any(|thread| thread.reply(reply).is_some_and(|reply| !reply.is_deleted())),
    };
    if let Some(review) = &global.review {
        let review = load(repo, &parse(review, "--review")?)?;
        return if live(&review) {
            Ok((review, id))
        } else {
            Err(CliError::not_found(format!(
                "no {id} in review {}",
                review.id
            )))
        };
    }
    for summary in Listing::read(repo)?.summaries {
        let review = match repo.store.load(&summary.id) {
            Ok(loaded) => loaded.review,
            Err(StoreError::UnknownReview { .. }) => continue,
            Err(error) => return Err(error.into()),
        };
        if live(&review) {
            return Ok((review, id));
        }
    }
    Err(CliError::not_found(format!(
        "no {id} in the reviews of {}",
        repo.root()
    )))
}

/// The live thread `id` of `review`, which [`thread`] found there.
pub(crate) fn live_thread<'r>(review: &'r Review, id: &ThreadId) -> Result<&'r Thread, CliError> {
    review
        .thread(id)
        .filter(|thread| !thread.is_deleted())
        .ok_or_else(|| CliError::not_found(format!("no thread {id} in review {}", review.id)))
}
