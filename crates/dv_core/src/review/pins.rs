//! A PR review's pins (docs/PLAN.md §9.5): the two commits it shows, kept under the review's
//! own refs, `refs/dv/reviews/<id>/pr-head` (the pull request's head as the review was last
//! refreshed to it) and `refs/dv/reviews/<id>/pr-base` (the merge base of that head and the
//! base branch: the old side).
//!
//! A PR review's target keeps the commits it was created at (PLAN §2.2: a target never
//! changes), so a draft that follows its pull request's head needs somewhere else to say
//! which head it shows. The pins are that: a refresh moves them, with a compare-and-swap, so
//! a refresh in another process never moves a draft behind the back of the one that read
//! them ([`advance`]); a submitted review's stay where they were when it was submitted. They
//! also keep the commits in the repository after a force-push drops them from GitHub's refs,
//! and they go with the review (`ReviewStore::delete` removes every ref under
//! `refs/dv/reviews/<id>/`).
//!
//! [`resolve`] is how a review's two sides are found, whatever its target: a PR review's
//! from its pins, any other from its target.

use super::id::ReviewId;
use crate::backend::{BackendError, GitOpts};
use crate::git::{GitError, GitRepo, Oid};
use crate::target::{DiffSpec, Side, Target, TargetError};

/// The two commits a PR review shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Pins {
    /// The pull request's head: the new side.
    pub head: Oid,
    /// The merge base of the head and the base branch: the old side.
    pub base: Oid,
}

impl Pins {
    /// The diff the pins show: GitHub's own "Files changed" (PLAN §9.3).
    pub fn spec(&self) -> DiffSpec {
        DiffSpec {
            old: Side::Commit(self.base.clone()),
            new: Side::Commit(self.head.clone()),
        }
    }
}

/// What [`advance`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Advance {
    /// The pins are where they were asked to go; `from` is where they stood, `None` when the
    /// review had none yet.
    Moved { from: Option<Pins> },
    /// They were there already.
    Unchanged,
    /// Another process moved them first, to these: its refresh stands.
    Lost(Pins),
}

/// The ref that pins a PR review's head.
pub fn head_ref(review: &ReviewId) -> String {
    format!("refs/dv/reviews/{review}/pr-head")
}

/// The ref that pins a PR review's merge base.
pub fn base_ref(review: &ReviewId) -> String {
    format!("refs/dv/reviews/{review}/pr-base")
}

/// The review's pins; `None` when it has none, or only one of the two (a review whose refs
/// were removed by hand, or whose store came from another clone).
pub fn read(repo: &GitRepo, review: &ReviewId) -> Result<Option<Pins>, GitError> {
    Ok(match read_each(repo, review)? {
        (Some(head), Some(base)) => Some(Pins { head, base }),
        _ => None,
    })
}

/// What each of the review's two pin refs points at, the head's then the base's, `None`
/// while it doesn't exist.
fn read_each(repo: &GitRepo, review: &ReviewId) -> Result<(Option<Oid>, Option<Oid>), GitError> {
    let (head, base) = (head_ref(review), base_ref(review));
    let mut refs = repo.ref_oids(&[&head, &base])?;
    Ok((refs.remove(&head), refs.remove(&base)))
}

/// Moves the review's pins from `from`, as the caller read them (`None`: it had none), to
/// `to`: both refs in one `git update-ref --stdin` transaction, each only while it still
/// stands where it was read (a compare-and-swap; a pin that didn't exist must still not).
/// When another process moved them meanwhile, nothing is written and its pins come back
/// ([`Advance::Lost`]). A review read as having none may hold one pin alone (the other
/// removed by hand): that one is replaced with its pair.
pub fn advance(
    repo: &GitRepo,
    review: &ReviewId,
    from: Option<&Pins>,
    to: &Pins,
) -> Result<Advance, GitError> {
    if from == Some(to) {
        return Ok(Advance::Unchanged);
    }
    let (old_head, old_base) = match from {
        Some(from) => (Some(from.head.clone()), Some(from.base.clone())),
        None => match read_each(repo, review)? {
            (Some(head), Some(base)) => return Ok(Advance::Lost(Pins { head, base })),
            alone => alone,
        },
    };
    let zero = "0".repeat(repo.object_format().hex_len());
    let old = |oid: &Option<Oid>| oid.as_ref().map_or(zero.clone(), Oid::to_string);
    let commands = format!(
        "update {} {} {}\nupdate {} {} {}\n",
        head_ref(review),
        to.head,
        old(&old_head),
        base_ref(review),
        to.base,
        old(&old_base),
    );
    let moved = repo.backend().git(
        &["update-ref", "--stdin"],
        &GitOpts::new().with_stdin(commands),
    );
    let error = match moved {
        Ok(_) => {
            return Ok(Advance::Moved {
                from: from.cloned(),
            });
        }
        // git refused the transaction: a pin no longer stands where it was read.
        Err(error @ BackendError::Git { .. }) => error,
        Err(error) => return Err(error.into()),
    };
    match read(repo, review)? {
        Some(now) if Some(&now) != from => Ok(Advance::Lost(now)),
        _ => Err(error.into()),
    }
}

/// The two sides a review of `target` shows (PLAN §2.2): a PR review's are its pins, which a
/// refresh moves and a submission freezes; any other review's are its target as it resolves
/// in `repo` now. A PR review without pins (above) resolves as its target does, at the
/// commits it was created at.
pub fn resolve(
    repo: &GitRepo,
    review: &ReviewId,
    target: &Target,
) -> Result<DiffSpec, TargetError> {
    if matches!(target, Target::Pr { .. })
        && let Some(pins) = read(repo, review)?
    {
        return Ok(pins.spec());
    }
    target.resolve(repo)
}
