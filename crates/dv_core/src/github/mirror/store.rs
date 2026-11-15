//! The mirrors of one repository over its [`Backend`] (docs/PLAN.md §9.4):
//! `<git-common-dir>/dv/github/<host>/<owner>/<repo>/pr-<n>/<login>.json`, written through the
//! backend's atomic store write, so a WSL repository's mirrors live in its distro beside its
//! reviews and a reader never sees half of one. The app follows a mirror another process
//! rewrote through the store's change events (PLAN §5.3), which name [`MirrorKey::path`].
//!
//! **A summary beside each mirror** (`<login>.summary`, written with it): the little a
//! listing shows of a pull request ([`MirrorStore::latest_title`]), so a menu that names a
//! repository's PR reviews reads a few hundred bytes for each, not a mirror of megabytes.
//! A mirror without one (a store an earlier dv wrote) is read whole for it, and gets one
//! the next time it is written. Nothing else reads a summary: it is no mirror
//! ([`super::is_mirror_of`]), holds nothing the mirror doesn't, and goes with the pull
//! request's mirrors (`super::super::forget`).
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

/// What ends a summary's name, after its mirror's stem.
const SUMMARY_SUFFIX: &str = ".summary";
/// What ends a mirror's name.
const MIRROR_SUFFIX: &str = ".json";
/// The summary's format version.
const SUMMARY_VERSION: u32 = 1;

/// A mirror's summary as stored (module docs).
#[derive(Serialize, Deserialize)]
struct Summary {
    v: u32,
    key: MirrorKey,
    read_at: u64,
    title: String,
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
                .is_ok_and(|names| names.iter().any(|name| !name.ends_with(SUMMARY_SUFFIX)))
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
            // A summary is no mirror, and isn't read here.
            if name.ends_with(SUMMARY_SUFFIX) {
                continue;
            }
            // Gone since the listing, or a stray file under a name no mirror has
            // (`kylekz copy.json`), which the store won't read.
            let Some(mirror) = self.read_stored(&format!("{dir}/{name}"))? else {
                continue;
            };
            if is_key_of(&mirror.key, host, owner, repo, number)
                && latest
                    .as_ref()
                    .is_none_or(|latest| latest.read_at < mirror.read_at)
            {
                latest = Some(mirror);
            }
        }
        Ok(latest)
    }

    /// The title of `link`'s pull request as its most recently read mirror has it,
    /// whichever login read it and under whichever of `hosts`: what a listing names a PR
    /// review by (PLAN §9.9). Read from the mirrors' summaries (module docs); a mirror
    /// without one is read whole.
    pub fn latest_title(
        &self,
        hosts: &[String],
        link: &PrLink,
    ) -> Result<Option<String>, BackendError> {
        let mut latest: Option<(u64, String)> = None;
        for host in hosts {
            let dir = pr_dir(host, &link.owner, &link.repo, link.number);
            let names = self.backend.store_list(&dir)?;
            for name in &names {
                let Some(stem) = name.strip_suffix(MIRROR_SUFFIX) else {
                    continue;
                };
                let summary = format!("{stem}{SUMMARY_SUFFIX}");
                // A summary of another pull request than its folder's (a file copied
                // there) is no summary of this one: the mirror itself is read.
                let summarised = match names.contains(&summary) {
                    true => self
                        .read_summary(&format!("{dir}/{summary}"))?
                        .filter(|summary| {
                            is_key_of(&summary.key, host, &link.owner, &link.repo, link.number)
                        }),
                    false => None,
                };
                let found = match summarised {
                    Some(summary) => Some((summary.key, summary.read_at, summary.title)),
                    None => self
                        .read_stored(&format!("{dir}/{name}"))?
                        .map(|mirror| (mirror.key, mirror.read_at, mirror.pr.title)),
                };
                if let Some((key, read_at, title)) = found
                    && is_key_of(&key, host, &link.owner, &link.repo, link.number)
                    && latest.as_ref().is_none_or(|(at, _)| *at < read_at)
                {
                    latest = Some((read_at, title));
                }
            }
        }
        Ok(latest.map(|(_, title)| title))
    }

    /// The summary stored at `path`; `None` for one that isn't there or isn't one this
    /// build reads.
    fn read_summary(&self, path: &str) -> Result<Option<Summary>, BackendError> {
        Ok(self
            .backend
            .store_read(path)?
            .and_then(|bytes| serde_json::from_slice::<Summary>(&bytes).ok())
            .filter(|summary| summary.v == SUMMARY_VERSION))
    }

    /// The mirror stored at `path`; `None` for a file that is gone, has a name the store
    /// won't read (`kylekz copy.json`) or isn't a mirror this build reads.
    fn read_stored(&self, path: &str) -> Result<Option<Mirror>, BackendError> {
        match self.backend.store_read(path) {
            Ok(Some(bytes)) => Ok(parse(&bytes).ok()),
            Ok(None) | Err(BackendError::InvalidPath { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Replaces the stored mirror of `mirror.key` with `mirror`, atomically, and its
    /// summary after it (module docs). A summary that can't be written is removed, so a
    /// listing reads the mirror itself rather than what an earlier one said.
    pub fn write(&self, mirror: &Mirror) -> Result<(), MirrorError> {
        let bytes = serde_json::to_vec(&Stored {
            v: MIRROR_VERSION,
            mirror,
        })?;
        let path = mirror.key.path();
        self.backend.store_write_atomic(&path, &bytes)?;
        let summary = format!(
            "{}{SUMMARY_SUFFIX}",
            path.strip_suffix(MIRROR_SUFFIX).unwrap_or(&path)
        );
        let written = serde_json::to_vec(&Summary {
            v: SUMMARY_VERSION,
            key: mirror.key.clone(),
            read_at: mirror.read_at,
            title: mirror.pr.title.clone(),
        })
        .map_err(MirrorError::from)
        .and_then(|bytes| Ok(self.backend.store_write_atomic(&summary, &bytes)?));
        if let Err(error) = written {
            log::warn!("{summary} wasn't written ({error}); listings read the mirror itself");
            let _ = self.backend.store_remove(&summary);
        }
        Ok(())
    }
}

/// Whether `key` is pull request `number` of `owner/repo` on `host`, whichever login's.
fn is_key_of(key: &MirrorKey, host: &str, owner: &str, repo: &str, number: u64) -> bool {
    key.number == number
        && key.host.eq_ignore_ascii_case(host)
        && key.owner.eq_ignore_ascii_case(owner)
        && key.repo.eq_ignore_ascii_case(repo)
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
