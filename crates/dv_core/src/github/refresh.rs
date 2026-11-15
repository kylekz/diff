//! Refreshing a PR review (docs/PLAN.md §9.4, §9.5): everything GitHub holds about the pull
//! request read again and written as its mirror, a moved head fetched, and a draft's pins
//! moved to it.
//!
//! A refresh runs when a PR review opens, when the reviewer asks for one, and after every
//! write dv makes to GitHub. Nothing polls.
//!
//! - **What is read** ([`see`], [`gather`]): the metadata and the checks (`gh pr view`), the
//!   commits (`git fetch`, verified against what gh reported), then for the mirror the
//!   viewer's login, every review thread with every comment, the viewer's pending review,
//!   each file's Viewed state and GitHub's own patch for each file.
//! - **One head.** The head is read again after the patches; when it moved meanwhile the
//!   refresh starts over, once, so the mirror's files and its `pr.head_oid` describe the same
//!   commit. Moved again, it fails as [`GithubError::Moved`].
//! - **A failed refresh keeps the last mirror**, which comes back with the error that makes
//!   it stale ([`Refreshed::stale`]). When the viewer can't be read (signed out, offline), or
//!   the clone's GitHub repository can't be told, the mirror's path can't be told either,
//!   and the pull request's most recently read mirror stands in, whoever read it
//!   (`MirrorStore::latest`, `MirrorStore::latest_stored`).
//! - **`synced` survives.** Each file's `synced` (PLAN §9.7) is carried over from the mirror
//!   the new one replaces.
//! - **A draft follows the head** ([`pins`]), and only together with its mirror: once a
//!   mirror read at the fetched head is written, the draft's pins move to that head and its
//!   merge base, with a compare-and-swap. A refresh whose reads fail after the fetch moves
//!   nothing: the pins and the stored mirror stay as they were, one head between them (the
//!   refs under `refs/dv/pr/` may have moved; nothing reads a review's sides from them). A
//!   compare-and-swap lost to another process leaves that process's pins beside this one's
//!   mirror until the next refresh, so nothing takes the mirror's head for the pins'. A
//!   review that isn't a draft keeps its pins.

mod files;
mod threads;

use std::sync::Arc;

use serde_json::{Map, Value};

use super::error::GithubError;
use super::fetch::{self, Fetched};
use super::mirror::store::{MirrorRead, MirrorStore};
use super::mirror::{Checks, Mirror, MirrorKey, PullRequest, ViewedFile};
use super::repo::GithubRepo;
use super::{Gh, pr};
use crate::backend::Backend;
use crate::git::GitRepo;
use crate::review::event::now_ms;
use crate::review::pins::{self, Advance, Pins};
use crate::review::{PrLink, Review, ReviewStatus};
use crate::target::{DiffSpec, Side};

/// What a refresh of a PR review did.
#[derive(Debug, Default)]
pub struct Refreshed {
    /// The mirror to show: the one this refresh wrote, or, when it failed, the last one
    /// stored (the viewer's; the most recently read, whoever read it, when the viewer can't
    /// be told). `None` when the pull request was never read here.
    pub mirror: Option<Box<Mirror>>,
    /// Whether `mirror` is the one this refresh read from GitHub and wrote: what GitHub
    /// holds now. A refresh that failed after that (a draft's pins couldn't move) is stale
    /// with a fresh mirror.
    pub fresh: bool,
    /// Why the refresh failed, which makes `mirror` stale unless it is `fresh`.
    pub stale: Option<GithubError>,
    /// The review's pins after it; `None` for a review without any.
    pub pins: Option<Pins>,
    /// Where the pins stood before, when the refresh moved them.
    pub moved_from: Option<Pins>,
}

/// A pull request as GitHub and the repository hold it at one moment: its metadata, and its
/// commits fetched and verified (PLAN §9.3 steps 1–3).
#[derive(Debug, Clone)]
pub(super) struct Seen {
    pub pr: PullRequest,
    pub checks: Checks,
    pub fetched: Fetched,
}

/// Reads pull request `number` of `repo` and brings its commits into the repository. When
/// the head fetched isn't the one gh reported (a push between the two, or GitHub's
/// `refs/pull/<n>/head` a moment behind its API), both are read again once, after a pause; a
/// second mismatch is [`GithubError::Moved`].
pub(super) fn see(
    git: &GitRepo,
    gh: &Gh,
    repo: &GithubRepo,
    number: u64,
) -> Result<Seen, GithubError> {
    for attempt in 0..2 {
        if attempt > 0 {
            gh.settle();
        }
        let viewed = pr::view(gh, repo, number)?;
        if let Some(fetched) = fetch::fetch(git, repo, number, &viewed)? {
            return Ok(Seen {
                pr: viewed.pr,
                checks: viewed.checks,
                fetched,
            });
        }
    }
    Err(GithubError::Moved { number })
}

/// Refreshes `review`, a PR review of the repository `backend` is bound to (module docs). A
/// local review has no pull request to read: nothing is refreshed.
pub fn refresh(backend: &Arc<dyn Backend>, gh: &Gh, review: &Review) -> Refreshed {
    let Some(link) = review.pr_target() else {
        return Refreshed::default();
    };
    match GithubRepo::of(backend.as_ref(), gh).and_then(|repo| same_repo(repo, link)) {
        Ok(repo) => refresh_seen(backend, gh, &repo, review, None),
        Err(error) => unread(backend, review, link, error),
    }
}

/// What a refresh that couldn't ask GitHub at all leaves (the clone's repository can't be
/// told, the pull request couldn't be read as an open began): the review's pins as they
/// stand and the pull request's most recently read mirror, stale with `error`.
pub(super) fn unread(
    backend: &Arc<dyn Backend>,
    review: &Review,
    link: &PrLink,
    error: GithubError,
) -> Refreshed {
    let mirrors = MirrorStore::new(backend.clone());
    Refreshed {
        mirror: mirrors
            .hosts()
            .and_then(|hosts| mirrors.latest_stored(&hosts, link))
            .unwrap_or_else(|error| {
                log::warn!("{link}: {error}");
                None
            })
            .map(Box::new),
        stale: Some(error),
        pins: pins::read(&GitRepo::new(backend.clone()), &review.id)
            .ok()
            .flatten(),
        ..Refreshed::default()
    }
}

/// `repo`, the clone's repository, when `link`'s pull request is in it;
/// [`GithubError::OtherRepository`] for a review whose store came from another clone, or
/// whose remote now points elsewhere.
fn same_repo(repo: GithubRepo, link: &PrLink) -> Result<GithubRepo, GithubError> {
    if repo.is(&repo.host, &link.owner, &link.repo) {
        Ok(repo)
    } else {
        Err(GithubError::OtherRepository {
            number: link.number,
            repository: format!("{}/{}", link.owner, link.repo),
        })
    }
}

/// [`refresh`] of `review`, a review of pull request `link.number` of `repo`; `seen` is the
/// pull request as an open just read it, which the first pass then doesn't read again.
pub(super) fn refresh_seen(
    backend: &Arc<dyn Backend>,
    gh: &Gh,
    repo: &GithubRepo,
    review: &Review,
    seen: Option<Seen>,
) -> Refreshed {
    let git = GitRepo::new(backend.clone());
    let mirrors = MirrorStore::new(backend.clone());
    let number = match review.pr_target() {
        Some(link) => link.number,
        None => return Refreshed::default(),
    };
    let before = match pins::read(&git, &review.id) {
        Ok(pins) => pins,
        Err(error) => {
            return Refreshed {
                stale: Some(error.into()),
                ..Refreshed::default()
            };
        }
    };
    let login = match gh.viewer(&repo.host) {
        Ok(login) => login,
        // Without the viewer no mirror can be named: the last one anyone read stands in.
        Err(error) => {
            return Refreshed {
                mirror: latest(&mirrors, repo, number),
                stale: Some(error),
                pins: before,
                ..Refreshed::default()
            };
        }
    };
    let key = MirrorKey::new(repo, number, &login);
    let stored = || match mirrors.read(&key) {
        Ok(MirrorRead::Found(mirror)) => Some(mirror),
        Ok(MirrorRead::Refetch(_)) => None,
        Err(error) => {
            log::warn!("{}#{number}: {error}", repo.name_with_owner());
            None
        }
    };
    let written = read(&git, gh, repo, &key, seen).and_then(|(seen, mut mirror)| {
        // What dv last synced for each file is dv's own: it outlives the mirror it was in.
        if let Some(previous) = stored() {
            for (path, file) in &mut mirror.viewed {
                file.synced = previous.viewed.get(path).and_then(|file| file.synced);
            }
        }
        mirrors.write(&mirror)?;
        Ok((seen, mirror))
    });
    let (seen, mirror) = match written {
        Ok(written) => written,
        // One head (module docs): without a mirror written for it, nothing moves.
        Err(error) => {
            return Refreshed {
                mirror: stored(),
                stale: Some(error),
                pins: before,
                ..Refreshed::default()
            };
        }
    };
    let mut refreshed = Refreshed {
        mirror: Some(Box::new(mirror)),
        fresh: true,
        pins: before.clone(),
        ..Refreshed::default()
    };
    // A draft follows the head its mirror was just read at (PLAN §9.5).
    if review.status == ReviewStatus::Draft {
        let to = seen.fetched.pins();
        match pins::advance(&git, &review.id, before.as_ref(), &to) {
            Ok(Advance::Moved { from }) => {
                // A draft without pins showed its target's commits (PLAN §9.5).
                let shown = from.or_else(|| unpinned(&git, review));
                refreshed.moved_from = shown.filter(|shown| *shown != to);
                refreshed.pins = Some(to);
            }
            Ok(Advance::Unchanged) => {}
            Ok(Advance::Lost(theirs)) => {
                refreshed.moved_from = before.filter(|before| *before != theirs);
                refreshed.pins = Some(theirs);
            }
            Err(error) => refreshed.stale = Some(error.into()),
        }
    }
    refreshed
}

/// What `review`, a PR review without pins, shows: its target as it resolves, at the commits
/// it was made at (`pins::resolve`); `None` when those don't resolve any more.
fn unpinned(git: &GitRepo, review: &Review) -> Option<Pins> {
    match review.target.resolve(git) {
        Ok(DiffSpec {
            old: Side::Commit(base),
            new: Side::Commit(head),
        }) => Some(Pins { head, base }),
        _ => None,
    }
}

/// The most recently read mirror of pull request `number` of `repo`, whoever read it; a
/// store that can't be read has none.
pub(super) fn latest(mirrors: &MirrorStore, repo: &GithubRepo, number: u64) -> Option<Box<Mirror>> {
    mirrors
        .latest(repo, number)
        .unwrap_or_else(|error| {
            log::warn!("{}#{number}: {error}", repo.name_with_owner());
            None
        })
        .map(Box::new)
}

/// The pull request `key` names, read whole: its commits fetched and verified, and its
/// mirror. `first` is the pull request as the caller just saw it, read again only when its
/// head moves under the mirror's reads (module docs, "One head").
fn read(
    git: &GitRepo,
    gh: &Gh,
    repo: &GithubRepo,
    key: &MirrorKey,
    first: Option<Seen>,
) -> Result<(Seen, Mirror), GithubError> {
    let mut seen = first;
    for _ in 0..2 {
        let now = match seen.take() {
            Some(seen) => seen,
            None => see(git, gh, repo, key.number)?,
        };
        if let Some(mirror) = gather(gh, repo, key, &now)? {
            return Ok((now, mirror));
        }
    }
    Err(GithubError::Moved { number: key.number })
}

/// Everything the mirror holds beside what `seen` already read, as `key`'s login sees it;
/// `None` when the head is no longer `seen`'s once the patches are read.
fn gather(
    gh: &Gh,
    repo: &GithubRepo,
    key: &MirrorKey,
    seen: &Seen,
) -> Result<Option<Mirror>, GithubError> {
    let number = key.number;
    let threads = threads::read(gh, repo, number)?;
    let pending_review = files::pending_review(gh, repo, number, &key.login)?;
    let viewed = files::viewed(gh, repo, number)?;
    let files = files::patches(gh, repo, number)?;
    if pr::head(gh, repo, number)? != seen.pr.head_oid {
        return Ok(None);
    }
    Ok(Some(Mirror {
        key: key.clone(),
        read_at: now_ms(),
        pr: seen.pr.clone(),
        merge_base_oid: Some(seen.fetched.merge_base.clone()),
        checks: seen.checks,
        threads,
        pending_review,
        files,
        viewed: viewed
            .into_iter()
            .map(|(path, state)| {
                (
                    path,
                    ViewedFile {
                        state,
                        synced: None,
                    },
                )
            })
            .collect(),
    }))
}

/// The variables every query about pull request `number` of `repo` takes, with `more`.
fn pr_variables(
    repo: &GithubRepo,
    number: u64,
    mut more: Map<String, Value>,
) -> Map<String, Value> {
    more.insert("owner".to_string(), Value::from(repo.owner.as_str()));
    more.insert("repo".to_string(), Value::from(repo.repo.as_str()));
    more.insert("number".to_string(), Value::from(number));
    more
}

/// GitHub's `fullDatabaseId`, a `BigInt`, which GraphQL writes as a string.
fn database_id(value: &Value) -> Option<u64> {
    match value {
        Value::String(text) => text.parse().ok(),
        other => other.as_u64(),
    }
}
