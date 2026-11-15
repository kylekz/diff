//! Opening a pull request as a review (docs/PLAN.md §9.3).
//!
//! 1. **Which pull request**: the reference must name the clone's own repository
//!    ([`PrRef::in_repo`]); one that names another is [`GithubError::OtherRepository`].
//! 2. **Metadata, fetch, verify** (`refresh::see`): `gh pr view`, the commits fetched into
//!    `refs/dv/pr/…` through the repository's backend, the fetched head checked against the
//!    one gh reported, and the old side as `merge-base(base, head)`. When this fails
//!    (offline, signed out, the pull request gone) and the store already holds a review of
//!    the pull request, its open draft or else its latest submitted review, that review opens
//!    as it stands, its mirror stale with the failure. With no review there is nothing to
//!    open without GitHub's word and the commits: the open fails. A connection to the
//!    repository's backend that died is no word about GitHub: that failure is the open's
//!    either way, and the caller opens again once the connection is back (PLAN §5.2).
//! 3. **The review**: the pull request's open draft when it has one, whatever head it was
//!    fetched at (at most one, PLAN §2.4); else its latest submitted review when that was
//!    submitted at this head (nothing new to review); else a new draft, its target the pull
//!    request at these commits, its pins at the head and the merge base (PLAN §9.5), and its
//!    `pr_linked` event. Finding or making it holds the pull request's lock
//!    (`super::lock`), so two openers at once (the app and the CLI) end with the one draft.
//! 4. **The mirror** is refreshed (`refresh`), which also moves a resumed draft's pins to
//!    the head. A failure there leaves the review open and its mirror stale.
//!
//! An open that fails makes nothing: with no review of the pull request in the store, what
//! it fetched of that pull request goes again (`forget::pull_request`).

use std::sync::Arc;

use super::error::GithubError;
use super::lock::{LOCK_WAIT, locked};
use super::pr_ref::PrRef;
use super::refresh::{self, Refreshed, Seen};
use super::repo::GithubRepo;
use super::{Gh, forget};
use crate::backend::Backend;
use crate::git::{GitRepo, Oid};
use crate::review::pins;
use crate::review::{
    Author, EventBody, ListEntry, PrLink, Review, ReviewStatus, ReviewStore, ReviewSummary,
};
use crate::target::Target;

/// What [`open`] found or made.
#[derive(Debug)]
pub struct OpenedPr {
    pub review: Review,
    /// `false` when a review of the pull request already existed and is returned instead.
    pub created: bool,
    /// The refresh the open ended with: the mirror, and why it is stale when it is.
    pub refreshed: Refreshed,
}

/// Opens the pull request `reference` names in the repository `backend` is bound to (module
/// docs). `author` is asked who the reviewer is only when a draft is made.
pub fn open(
    backend: &Arc<dyn Backend>,
    gh: &Gh,
    reference: &PrRef,
    author: impl FnOnce() -> Author,
) -> Result<OpenedPr, GithubError> {
    let repo = GithubRepo::of(backend.as_ref(), gh)?;
    let link = reference.in_repo(&repo)?;
    let git = GitRepo::new(backend.clone());
    let store = ReviewStore::new(backend.clone());
    let seen = match refresh::see(&git, gh, &repo, link.number) {
        Ok(seen) => seen,
        Err(error) if error.is_disconnected() => return Err(error),
        Err(error) => {
            return match existing(&store, &git, &link, None)? {
                Some(review) => Ok(OpenedPr {
                    refreshed: refresh::unread(backend, &review, &link, error),
                    review,
                    created: false,
                }),
                None => Err(nothing_made(&store, &link, error)),
            };
        }
    };
    let found = locked(&git, &link, LOCK_WAIT, || {
        Ok(
            match existing(&store, &git, &link, Some(&seen.fetched.head))? {
                Some(review) => (review, false),
                None => (create(&store, &git, link.clone(), &seen, author())?, true),
            },
        )
    });
    let (review, created) = found.map_err(|error| nothing_made(&store, &link, error))?;
    let refreshed = refresh::refresh_seen(backend, gh, &repo, &review, Some(seen));
    Ok(OpenedPr {
        review,
        created,
        refreshed,
    })
}

/// `error`, an open's, once what the open fetched of `link`'s pull request is gone again
/// when the store holds no review of it (best effort, `forget::pull_request`). Nothing is
/// asked of a backend whose connection died.
fn nothing_made(store: &ReviewStore, link: &PrLink, error: GithubError) -> GithubError {
    if !error.is_disconnected()
        && let Err(swept) = forget::pull_request(store, link)
    {
        log::warn!("{link}: what the failed open fetched wasn't removed: {swept}");
    }
    error
}

/// The review of `link`'s pull request an open resumes: its open draft (one, `super::lock`; the
/// most recently updated, should a store put together by hand hold several), else its latest submitted review when that one
/// shows `head`, the head just fetched; with no `head` (GitHub couldn't be asked), its
/// latest submitted review whatever it shows. Logs that fail to load are passed over.
fn existing(
    store: &ReviewStore,
    git: &GitRepo,
    link: &PrLink,
    head: Option<&Oid>,
) -> Result<Option<Review>, GithubError> {
    let reviews: Vec<ReviewSummary> = store
        .list()?
        .into_iter()
        .filter_map(|entry| match entry {
            ListEntry::Ok(summary) => Some(*summary),
            ListEntry::Err { .. } => None,
        })
        .filter(|summary| link.is_target(&summary.target))
        .collect();
    let latest = |wanted: fn(ReviewStatus) -> bool| {
        reviews
            .iter()
            .filter(|summary| wanted(summary.status))
            .max_by_key(|summary| summary.updated_at)
    };
    let found = match latest(ReviewStatus::is_open) {
        Some(draft) => Some(draft),
        None => match (latest(|status| status == ReviewStatus::Submitted), head) {
            (Some(submitted), Some(head))
                if pins::read(git, &submitted.id)?.is_some_and(|pins| pins.head == *head) =>
            {
                Some(submitted)
            }
            (submitted, None) => submitted,
            _ => None,
        },
    };
    Ok(match found {
        Some(summary) => Some(store.load(&summary.id)?.review),
        None => None,
    })
}

/// A new draft of `link`'s pull request at the commits `seen` fetched, by `author`.
fn create(
    store: &ReviewStore,
    git: &GitRepo,
    link: PrLink,
    seen: &Seen,
    author: Author,
) -> Result<Review, GithubError> {
    let review = store.create(
        Target::Pr {
            owner: link.owner.clone(),
            repo: link.repo.clone(),
            number: link.number,
            base_oid: seen.fetched.base.clone(),
            head_oid: seen.fetched.head.clone(),
        },
        author,
    )?;
    pins::advance(git, &review.id, None, &seen.fetched.pins())?;
    store.append(
        &review.id,
        EventBody::PrLinked {
            owner: link.owner,
            repo: link.repo,
            number: link.number,
        },
    )?;
    Ok(review)
}
