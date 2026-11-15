//! The mirrors of one repository over its [`Backend`] (docs/PLAN.md §9.4):
//! `<git-common-dir>/dv/github/<host>/<owner>/<repo>/pr-<n>/<login>.json`, written through the
//! backend's atomic store write, so a WSL repository's mirrors live in its distro beside its
//! reviews and a reader never sees half of one. The app follows a mirror another process
//! rewrote through the store's change events (PLAN §5.3), which name [`MirrorKey::path`].
//!
//! A mirror's file is named by its login, which only `gh` can tell. When it can't,
//! [`MirrorStore::latest`] finds the pull request's most recently read mirror, whoever read
//! it, to show as stale.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{MIRROR_VERSION, Mirror, MirrorKey, pr_dir};
use crate::backend::{Backend, BackendError};
use crate::github::repo::{GithubRepo, remote_hosts};
use crate::review::PrLink;

/// What reading a mirror found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorRead {
    /// Boxed: a mirror is many times the size of a reason.
    Found(Box<Mirror>),
    /// Nothing usable is stored: fetch it again.
    Refetch(Refetch),
}

/// Why a mirror must be fetched again (PLAN §9.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refetch {
    /// The pull request was never read by this login in this repository.
    Missing,
    /// The file isn't a mirror: cut off, edited by hand, or a version's whose shape changed
    /// without its number.
    Unparseable { error: String },
    /// Another dv's format.
    OtherVersion { found: u64 },
    /// The file holds another pull request's or login's mirror: one copied there by hand,
    /// since no two keys share a path.
    OtherKey,
}

/// Why a mirror couldn't be written.
#[derive(Debug, thiserror::Error)]
pub enum MirrorError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error("couldn't encode the mirror: {0}")]
    Encode(#[from] serde_json::Error),
}

/// A mirror as stored: its fields beside the format version.
#[derive(Serialize)]
struct Stored<'a> {
    v: u32,
    #[serde(flatten)]
    mirror: &'a Mirror,
}

/// The format version alone, read before the rest is.
#[derive(Deserialize)]
struct Version {
    v: u64,
}

/// The PR mirrors stored in one repository, shared by all its worktrees.
#[derive(Clone)]
pub struct MirrorStore {
    backend: Arc<dyn Backend>,
}

impl std::fmt::Debug for MirrorStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorStore")
            .field("common_dir", &self.backend.repo().common_dir)
            .finish()
    }
}

impl MirrorStore {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    /// The mirror `key` names, or why it has to be fetched again. Only a failure to read the
    /// store at all is an error.
    pub fn read(&self, key: &MirrorKey) -> Result<MirrorRead, BackendError> {
        Ok(match self.backend.store_read(&key.path())? {
            None => MirrorRead::Refetch(Refetch::Missing),
            Some(bytes) => match parse(&bytes) {
                Err(why) => MirrorRead::Refetch(why),
                Ok(mirror) if mirror.key != *key => MirrorRead::Refetch(Refetch::OtherKey),
                Ok(mirror) => MirrorRead::Found(Box::new(mirror)),
            },
        })
    }

    /// The most recently read mirror of pull request `number` of `repo`, whichever login
    /// read it: what a refresh shows, stale, when the viewer's login can't be read
    /// (PLAN §9.4). Files that aren't this build's mirrors of that pull request are passed
    /// over.
    pub fn latest(&self, repo: &GithubRepo, number: u64) -> Result<Option<Mirror>, BackendError> {
        self.latest_on(&repo.host, &repo.owner, &repo.repo, number)
    }

    /// The hosts a mirror of this clone's pull requests may be kept under: every host a
    /// remote names, and `github.com` (a few git calls, no `gh`).
    pub fn hosts(&self) -> Result<Vec<String>, BackendError> {
        remote_hosts(self.backend.as_ref())
    }

    /// [`MirrorStore::latest`] of `link`'s pull request without knowing which host the clone's
    /// repository is on, which only `gh` can tell for a GitHub Enterprise host: looked for
    /// under each of `hosts` ([`MirrorStore::hosts`]). What a command that only reads shows
    /// (PLAN §9.10), and what stands in when the repository can't be told.
    pub fn latest_stored(
        &self,
        hosts: &[String],
        link: &PrLink,
    ) -> Result<Option<Mirror>, BackendError> {
        let mut latest: Option<Mirror> = None;
        for host in hosts {
            if let Some(mirror) = self.latest_on(host, &link.owner, &link.repo, link.number)?
                && latest
                    .as_ref()
                    .is_none_or(|latest| latest.read_at < mirror.read_at)
            {
                latest = Some(mirror);
            }
        }
        Ok(latest)
    }

    /// Whether anything is stored for `link`'s pull request under any of `hosts`. With no
    /// mirror to read there ([`MirrorStore::latest_stored`]), what is stored can't be read:
    /// an empty or cut file, another version's, another key's.
    pub fn holds_any(&self, hosts: &[String], link: &PrLink) -> bool {
        hosts.iter().any(|host| {
            self.backend
                .store_list(&pr_dir(host, &link.owner, &link.repo, link.number))
                .is_ok_and(|names| !names.is_empty())
        })
    }

    /// The most recently read mirror of pull request `number` of `owner/repo` on `host`.
    fn latest_on(
        &self,
        host: &str,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Option<Mirror>, BackendError> {
        let dir = pr_dir(host, owner, repo, number);
        let mut latest: Option<Mirror> = None;
        for name in self.backend.store_list(&dir)? {
            let bytes = match self.backend.store_read(&format!("{dir}/{name}")) {
                Ok(Some(bytes)) => bytes,
                // Gone since the listing, or a stray file under a name no mirror has
                // (`kylekz copy.json`), which the store won't read.
                Ok(None) | Err(BackendError::InvalidPath { .. }) => continue,
                Err(error) => return Err(error),
            };
            let Ok(mirror) = parse(&bytes) else {
                continue;
            };
            let key = &mirror.key;
            if key.number == number
                && key.host.eq_ignore_ascii_case(host)
                && key.owner.eq_ignore_ascii_case(owner)
                && key.repo.eq_ignore_ascii_case(repo)
                && latest
                    .as_ref()
                    .is_none_or(|latest| latest.read_at < mirror.read_at)
            {
                latest = Some(mirror);
            }
        }
        Ok(latest)
    }

    /// Replaces the stored mirror of `mirror.key` with `mirror`, atomically.
    pub fn write(&self, mirror: &Mirror) -> Result<(), MirrorError> {
        let bytes = serde_json::to_vec(&Stored {
            v: MIRROR_VERSION,
            mirror,
        })?;
        self.backend
            .store_write_atomic(&mirror.key.path(), &bytes)?;
        Ok(())
    }
}

/// A stored mirror's bytes as this build's mirror, or why they aren't one.
fn parse(bytes: &[u8]) -> Result<Mirror, Refetch> {
    let unparseable = |error: serde_json::Error| Refetch::Unparseable {
        error: error.to_string(),
    };
    match serde_json::from_slice::<Version>(bytes).map_err(unparseable)? {
        Version { v } if v != u64::from(MIRROR_VERSION) => Err(Refetch::OtherVersion { found: v }),
        _ => serde_json::from_slice(bytes).map_err(unparseable),
    }
}
