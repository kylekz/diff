//! Where a pull request named by a reference can be opened (PLAN §9.9): the known clones of
//! its repository, or none of them. The palette's **Pull request** section and the PR
//! picker's field are both read through here ([`offer`]).
//!
//! Decided from what dv already holds, with no `gh` call and nothing opened: each known
//! repository's GitHub name as last read (`known_repos.json`'s `github`, the `Reviews`
//! service's rows), and the titles of the lists the picker read this session. So a pasted
//! URL is recognised as it lands, and a repository in a stopped distro matches without
//! being started. The open itself checks the clone (PLAN §9.3): a remembered name that
//! went stale ends as `OtherRepository`, never as another repository's pull request.

use dv_core::Location;
use dv_core::github::PrRef;

use crate::services::github::GitHub;
use crate::services::reviews::{RepoRow, Reviews};
use crate::switcher::items::repo_keys;
use crate::thread::drawn;

/// A clone a referenced pull request can be opened in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloneOffer {
    pub location: Location,
    /// The repository's name as the switcher's rows are keyed by it: its name, or its full
    /// path when two known repositories share a name.
    pub key: String,
    /// The repository's name as the switcher shows it.
    pub name: String,
    /// The pull request's title, when a list read this session holds it, as a row draws it
    /// (`crate::thread::drawn`).
    pub title: Option<String>,
}

/// What a reference offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrOffer {
    /// Its repository's known clones, the most recently used first (the repository in view
    /// alone for a reference that names none).
    Clones {
        reference: PrRef,
        /// The pull request as the row names it: `acme/frontend#482`, or `#482` for the
        /// repository in view while its GitHub name isn't known.
        name: String,
        clones: Vec<CloneOffer>,
    },
    /// No known repository is a clone of the reference's: its folder is to be chosen.
    Unknown {
        reference: PrRef,
        /// `acme/frontend`; `ghe.example/acme/frontend` on another host than github.com.
        repository: String,
    },
}

impl PrOffer {
    pub fn reference(&self) -> &PrRef {
        match self {
            Self::Clones { reference, .. } | Self::Unknown { reference, .. } => reference,
        }
    }
}

/// What `reference` offers (module docs). A reference without a repository (`#482`, `482`)
/// names the repository in view, `in_view`: with none in view it offers nothing.
pub fn offer(
    reference: &PrRef,
    in_view: Option<&Location>,
    reviews: &Reviews,
    github: &GitHub,
) -> Option<PrOffer> {
    let rows = reviews.rows();
    let keys = repo_keys(&rows);
    let key_of = |row: &RepoRow| {
        rows.iter()
            .position(|other| other.location == row.location)
            .and_then(|at| keys.get(at).cloned())
            .unwrap_or_else(|| row.path.clone())
    };
    let number = reference.number();
    // The title is the pull request's, not a clone's: the list read for any clone of its
    // repository gives it to every clone's row.
    let clone = |location: &Location, key: String, name: String| CloneOffer {
        title: github
            .listed_title(location, number)
            .or_else(|| {
                let named = reviews.row(location)?.github.as_ref()?;
                github.title_of(named, number)
            })
            .map(drawn),
        location: location.clone(),
        key,
        name,
    };
    let (host, owner, repo) = match reference {
        PrRef::Number(_) | PrRef::Hash(_) => {
            let location = in_view?;
            let row = reviews.row(location);
            let name = match row.and_then(|row| row.github.as_ref()) {
                Some(github) => format!("{}#{number}", github.name_with_owner()),
                None => format!("#{number}"),
            };
            return Some(PrOffer::Clones {
                reference: reference.clone(),
                name,
                clones: vec![clone(
                    location,
                    row.map_or_else(|| location.display_name(), key_of),
                    row.map_or_else(|| location.short_name(), |row| row.name.clone()),
                )],
            });
        }
        PrRef::Short { owner, repo, .. } => (None, owner, repo),
        PrRef::Url {
            host, owner, repo, ..
        } => (Some(host.as_str()), owner, repo),
    };
    let clones: Vec<CloneOffer> = reviews
        .clones_of(host, owner, repo)
        .into_iter()
        .map(|row| clone(&row.location, key_of(row), row.name.clone()))
        .collect();
    Some(if clones.is_empty() {
        PrOffer::Unknown {
            reference: reference.clone(),
            repository: match host {
                Some(host) if !host.eq_ignore_ascii_case(dv_core::github::GITHUB_COM) => {
                    format!("{host}/{owner}/{repo}")
                }
                _ => format!("{owner}/{repo}"),
            },
        }
    } else {
        PrOffer::Clones {
            reference: reference.clone(),
            name: format!("{owner}/{repo}#{number}"),
            clones,
        }
    })
}
