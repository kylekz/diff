//! Fetching a pull request's commits (docs/PLAN.md §9.3 steps 2–3): through the repository's
//! backend, into dv's own namespace, then the old side computed from what was fetched.
//!
//! - **The refs** are `refs/dv/pr/<owner>/<repo>/<n>/head` and `…/base`, owner and repository
//!   spelled as [`spell`] spells a GitHub name, so one git refuses (`.github`, `x.lock`)
//!   still makes a ref. They are shared by every review of the pull request and go with its
//!   last one (`super::forget`).
//! - **The fetch** is `git fetch --no-tags --no-write-fetch-head --no-recurse-submodules
//!   --no-auto-maintenance --refmap= <remote> +refs/pull/<n>/head:<head ref>
//!   +refs/heads/<base branch>:<base ref>`, every time a pull request is opened or refreshed:
//!   a pull request can be retargeted without its head moving. Nothing else of the user's
//!   repository is written (PLAN §2.4): `--refmap=` keeps git from also moving the remote's
//!   tracking branch (`origin/main`), which other reviews' default base reads;
//!   `--no-recurse-submodules` keeps it out of the submodules a new head points into, whose
//!   own refs a fetch on demand would move; `--no-auto-maintenance` keeps it from starting
//!   a `gc` or a maintenance run in the user's repository. `refs/pull/<n>/head` lives on the
//!   base repository for a fork's pull request too, so the one remote always serves.
//! - **A base branch that is gone** (deleted once its own pull request merged, say) fails
//!   that fetch whole. The head is then fetched alone and the base taken from GitHub's
//!   `baseRefOid`: fetched by its id, when the server hands a commit out by its id; else
//!   that commit when the clone has it already; else the base ref an earlier fetch left;
//!   else [`GithubError::BaseGone`].
//! - **Two fetches of one pull request at once** contend for the same refs: one that fails
//!   on a ref's lock is run once more, and so is one whose refs are gone when it reads them
//!   back (an open of the same pull request that failed elsewhere removed what it fetched,
//!   `super::forget`).
//! - **The old side** is `merge-base(base, head)` of the fetched commits, so the diff is
//!   GitHub's own "Files changed" (three dots). A pull request merged by a merge commit or
//!   by a fast-forward has its head in the base branch, where that merge base is the head
//!   itself and the diff empty ([`old_side`] finds where it started instead).

use std::time::Duration;

use super::error::GithubError;
use super::mirror::PrState;
use super::pr::Viewed;
use super::repo::GithubRepo;
use super::spelling::spell;
use crate::backend::{BackendError, GitOpts};
use crate::git::{GitError, GitRepo, Oid};
use crate::review::Pins;
use crate::target::TargetError;

/// How long a pull request's fetch may take: a network call, of a first fetch's size at most.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(300);

/// The namespace holding what dv fetched of pull request `number` of `owner/repo`:
/// `refs/dv/pr/<owner>/<repo>/<n>`.
pub fn refs_prefix(owner: &str, repo: &str, number: u64) -> String {
    format!("refs/dv/pr/{}/{}/{number}", spell(owner), spell(repo))
}

/// The ref a pull request's head is fetched into.
pub fn head_ref(owner: &str, repo: &str, number: u64) -> String {
    format!("{}/head", refs_prefix(owner, repo, number))
}

/// The ref a pull request's base branch is fetched into.
pub fn base_ref(owner: &str, repo: &str, number: u64) -> String {
    format!("{}/base", refs_prefix(owner, repo, number))
}

/// A pull request's commits in the repository, as last fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// The pull request's head.
    pub head: Oid,
    /// The base branch's tip (GitHub's `baseRefOid` once the branch is gone).
    pub base: Oid,
    /// Where the pull request's changes start ([`old_side`]): the old side.
    pub merge_base: Oid,
}

impl Fetched {
    /// The pins a draft that follows this head shows (PLAN §9.5).
    pub fn pins(&self) -> Pins {
        Pins {
            head: self.head.clone(),
            base: self.merge_base.clone(),
        }
    }
}

/// Brings pull request `number` of `repo`, as `gh` just told it (`viewed`), into the
/// repository (module docs). `None` when the head fetched isn't the one gh reported: a push
/// landed between gh's answer and the fetch, or GitHub's `refs/pull/<n>/head` isn't there
/// yet.
pub(super) fn fetch(
    git: &GitRepo,
    repo: &GithubRepo,
    number: u64,
    viewed: &Viewed,
) -> Result<Option<Fetched>, GithubError> {
    let (head_ref, base_ref) = (
        head_ref(&repo.owner, &repo.repo, number),
        base_ref(&repo.owner, &repo.repo, number),
    );
    let base_branch = &viewed.pr.base_ref;
    let fetch = |refspecs: &[String]| {
        let mut args = vec![
            // Whatever the user's configuration says, the fetch writes no commit-graph:
            // only the refs and their objects (PLAN §2.4).
            "-c",
            "fetch.writeCommitGraph=false",
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            "--refmap=",
            "--",
            &repo.remote,
        ];
        args.extend(refspecs.iter().map(String::as_str));
        git.backend()
            .git(&args, &GitOpts::new().with_timeout(FETCH_TIMEOUT))
    };
    // Another fetch of the same pull request (the app and the CLI at once) may hold a ref's
    // lock for longer than git waits for one: once more, when it has let go.
    let run = |refspecs: &[String]| match fetch(refspecs) {
        Err(BackendError::Git { stderr, .. }) if stderr.contains("cannot lock ref") => {
            fetch(refspecs)
        }
        fetched => fetched,
    };
    let head_spec = format!("+refs/pull/{number}/head:{head_ref}");
    let base_spec = format!("+refs/heads/{base_branch}:{base_ref}");
    // Fetches both and reads them back: `None` when they aren't there after the fetch.
    let bring = || -> Result<Option<(Oid, Oid)>, GithubError> {
        match run(&[head_spec.clone(), base_spec.clone()]) {
            Ok(_) => {}
            Err(BackendError::Git { stderr, .. })
                if stderr.contains(&format!(
                    "couldn't find remote ref refs/heads/{base_branch}"
                )) =>
            {
                run(std::slice::from_ref(&head_spec))?;
                // The commit GitHub recorded for the branch: fetched by its id when the
                // server hands it out, taken as it is when the clone has it already; else
                // what an earlier fetch of the branch left.
                let recorded = match &viewed.base_oid {
                    Some(base) if run(&[format!("+{base}:{base_ref}")]).is_ok() => true,
                    Some(base) if git.has_commit(base)? => {
                        git.backend()
                            .git(&["update-ref", &base_ref, base.as_str()], &GitOpts::new())?;
                        true
                    }
                    _ => false,
                };
                if !recorded && !git.ref_oids(&[&base_ref])?.contains_key(&base_ref) {
                    return Err(GithubError::BaseGone {
                        number,
                        branch: base_branch.clone(),
                    });
                }
            }
            Err(error) => return Err(error.into()),
        }
        let mut refs = git.ref_oids(&[&head_ref, &base_ref])?;
        Ok(refs.remove(&head_ref).zip(refs.remove(&base_ref)))
    };
    // An open of the same pull request that failed elsewhere removes what it fetched, which
    // are these refs (`super::forget`): gone between the fetch and the read, they are
    // fetched once more.
    let brought = match bring()? {
        Some(brought) => Some(brought),
        None => bring()?,
    };
    let Some((head, base)) = brought else {
        return Err(GitError::UnexpectedOutput {
            command: "fetch",
            detail: format!("{head_ref} and {base_ref} aren't there after the fetch"),
        }
        .into());
    };
    if head != viewed.pr.head_oid {
        return Ok(None);
    }
    let merge_base = old_side(git, number, viewed, &base, &head)?;
    Ok(Some(Fetched {
        head,
        base,
        merge_base,
    }))
}

/// Where the pull request's changes start: `merge-base(base, head)`, GitHub's three dots.
///
/// A merged pull request whose head is in the base branch (merged by a merge commit, or by a
/// fast-forward) has no changes by that rule: the merge base is the head. Where it started
/// is then read from how it was merged: a merge commit with the head as a parent merged it
/// into its other parent, so the old side is their merge base; else GitHub's `baseRefOid`,
/// the base branch before the merge, when that commit is here and doesn't hold the head. A
/// merge commit's first parent is never taken on trust: a rebase merge's is the last of the
/// rebased commits. A squash or a rebase merge leaves the head outside the base branch, and
/// resolves as an open pull request does. When neither tells, the old side stays the head:
/// nothing to show, which is all dv knows.
fn old_side(
    git: &GitRepo,
    number: u64,
    viewed: &Viewed,
    base: &Oid,
    head: &Oid,
) -> Result<Oid, GithubError> {
    let merge_base = |base: &Oid| git.merge_base(base.as_str(), head.as_str());
    let common = merge_base(base)?.ok_or_else(|| TargetError::NoMergeBase {
        a: viewed.pr.base_ref.clone(),
        b: format!("#{number}"),
    })?;
    if common != *head || viewed.pr.state != PrState::Merged {
        return Ok(common);
    }
    if let Some(merge) = &viewed.merge_commit
        && git.has_commit(merge)?
    {
        let parents = git.parents(merge)?;
        if let [first, second] = parents.as_slice()
            && (first == head || second == head)
        {
            let other = if first == head { second } else { first };
            return Ok(merge_base(other)?.unwrap_or(common));
        }
    }
    if let Some(before) = &viewed.base_oid
        && git.has_commit(before)?
        && let Some(started) = merge_base(before)?
        && started != *head
    {
        return Ok(started);
    }
    Ok(common)
}
