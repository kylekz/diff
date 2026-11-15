//! What dv keeps for a pull request goes with its last review (docs/PLAN.md §7, §9.3, §9.4):
//! the commits fetched under `refs/dv/pr/<owner>/<repo>/<n>/` and its mirror files, every
//! login's.
//!
//! [`unreviewed`] runs with every deletion of a review and removes both for every pull
//! request no review in the store is of, whichever review was deleted, so a delete that was
//! interrupted after its log went is finished by the next one. The mirror files go first
//! and the refs last: while the refs stand, the sweep finds the pull request again. It is
//! best effort: a sweep that fails leaves the deletion done, and the next one sweeps again.
//! It also drops the locks opens left behind (`super::lock`). An open that fails removes
//! what it fetched of its own pull request alone ([`pull_request`]).
//! While any log in the store doesn't load, nothing is swept: that log may be the review a
//! pull request's refs and mirror are kept for.
//!
//! The mirror's path starts with its host, which a pull request's refs don't name: it is
//! looked for under every host a remote of the clone names, and `github.com`. A mirror kept
//! under a host no remote names any more stays, a cache nothing reads.

use std::collections::BTreeSet;

use super::fetch::refs_prefix;
use super::lock;
use super::mirror::spelled_pr_dir;
use super::repo::remote_hosts;
use super::spelling::spell;
use crate::backend::{Backend, BackendError, GitOpts};
use crate::review::{ListEntry, PrLink, ReviewStore, StoreError};
use crate::target::Target;

/// Removes the fetched refs and the mirror files of every pull request `store` holds no
/// review of (module docs).
pub(crate) fn unreviewed(store: &ReviewStore) -> Result<(), StoreError> {
    let backend = store.backend().as_ref();
    // A lock left by an open that died, for a pull request nobody opened again.
    if let Err(error) = lock::sweep(backend) {
        log::debug!("abandoned locks weren't swept: {error}");
    }
    let output = backend.git(
        &["for-each-ref", "--format=%(refname)", "refs/dv/pr/"],
        &GitOpts::new(),
    )?;
    let text = String::from_utf8_lossy(&output.stdout);
    let fetched: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    if fetched.is_empty() {
        return Ok(());
    }
    // A log that doesn't load may be a review of any of them: nothing is swept on a guess.
    let mut reviewed = BTreeSet::new();
    for entry in store.list()? {
        match entry {
            ListEntry::Ok(summary) => {
                if let Target::Pr {
                    owner,
                    repo,
                    number,
                    ..
                } = &summary.target
                {
                    reviewed.insert(format!("{}/", refs_prefix(owner, repo, *number)));
                }
            }
            ListEntry::Err { .. } => return Ok(()),
        }
    }
    let orphans: Vec<&str> = fetched
        .into_iter()
        .filter(|name| !reviewed.iter().any(|prefix| name.starts_with(prefix)))
        .collect();
    remove(backend, &orphans)
}

/// Removes what dv keeps of `link`'s pull request alone, when `store` holds no review of
/// it: what an open that failed fetched (PLAN §9.3). Nothing of any other pull request is
/// touched, so another process between its own fetch and its review loses nothing to this
/// one's failure. As in [`unreviewed`], nothing goes while any log doesn't load.
pub(crate) fn pull_request(store: &ReviewStore, link: &PrLink) -> Result<(), StoreError> {
    for entry in store.list()? {
        match entry {
            ListEntry::Ok(summary) if link.is_target(&summary.target) => return Ok(()),
            ListEntry::Ok(_) => {}
            ListEntry::Err { .. } => return Ok(()),
        }
    }
    let backend = store.backend().as_ref();
    // `for-each-ref` matches a pattern up to a `/`: this pull request's refs, not #70's.
    let output = backend.git(
        &[
            "for-each-ref",
            "--format=%(refname)",
            &refs_prefix(&link.owner, &link.repo, link.number),
        ],
        &GitOpts::new(),
    )?;
    let text = String::from_utf8_lossy(&output.stdout);
    let fetched: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    // Its mirrors go whether or not its refs are still there (removed by hand, say).
    let (owner, repo) = (spell(&link.owner), spell(&link.repo));
    for host in remote_hosts(backend)? {
        remove_mirrors(backend, &spelled_pr_dir(&host, &owner, &repo, link.number))?;
    }
    remove(backend, &fetched)
}

/// Removes `refs`, pull requests' fetched refs, and the mirror files of the pull requests
/// they belong to: the mirrors first, the refs last (module docs).
fn remove(backend: &dyn Backend, orphans: &[&str]) -> Result<(), StoreError> {
    if orphans.is_empty() {
        return Ok(());
    }
    let hosts = remote_hosts(backend)?;
    let prs: BTreeSet<(&str, &str, u64)> = orphans.iter().filter_map(|name| pr_of(name)).collect();
    for (owner, repo, number) in prs {
        for host in &hosts {
            remove_mirrors(backend, &spelled_pr_dir(host, owner, repo, number))?;
        }
    }
    let commands: String = orphans
        .iter()
        .map(|name| format!("delete {name}\n"))
        .collect();
    backend.git(
        &["update-ref", "--stdin"],
        &GitOpts::new().with_stdin(commands),
    )?;
    Ok(())
}

/// The pull request a ref under `refs/dv/pr/` belongs to: its owner and repository as the
/// ref spells them, and its number.
fn pr_of(name: &str) -> Option<(&str, &str, u64)> {
    let mut parts = name.strip_prefix("refs/dv/pr/")?.split('/');
    let (owner, repo, number) = (parts.next()?, parts.next()?, parts.next()?);
    Some((owner, repo, number.parse().ok()?))
}

/// Removes every file of the store directory `dir`: a pull request's mirrors.
fn remove_mirrors(backend: &dyn Backend, dir: &str) -> Result<(), StoreError> {
    for name in backend.store_list(dir)? {
        match backend.store_remove(&format!("{dir}/{name}")) {
            // A stray file under a name no mirror has, which the store won't touch.
            Ok(_) | Err(BackendError::InvalidPath { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
