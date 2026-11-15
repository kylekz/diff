//! How a pull request is named (docs/PLAN.md §9.3): `482`, `#482`, `owner/repo#482` or its
//! URL, `https://<host>/<owner>/<repo>/pull/482` with any trailing path, query or fragment
//! (`/files`, `#discussion_r1`), read as people paste it ([`PrRef::parse`]).

use std::str::FromStr;

use super::GithubError;
use super::repo::{GithubRepo, is_name, url_host};
use crate::review::PrLink;

/// A parsed pull request reference. The spelling is kept, because it decides what the
/// reference is taken for: the palette reads `#482` as a pull request of the repository in
/// view and `482` as a search (PLAN §9.9).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PrRef {
    /// `482`: a pull request of the clone's repository.
    Number(u64),
    /// `#482`: the same.
    Hash(u64),
    /// `owner/repo#482`, on the clone's host.
    Short {
        owner: String,
        repo: String,
        number: u64,
    },
    /// The pull request's URL.
    Url {
        /// Lowercase, as [`GithubRepo::host`] is.
        host: String,
        owner: String,
        repo: String,
        number: u64,
    },
}

impl PrRef {
    /// `text` as a reference, when it is one, as people paste one: space around it, a
    /// closing bracket, a full stop or a comma after it and angle brackets around it are
    /// ignored, and a URL may come without its scheme or as the pull request's `.diff` or
    /// `.patch`.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_end_matches([')', '.', ',']).trim_end();
        let text = text
            .strip_prefix('<')
            .and_then(|text| text.strip_suffix('>'))
            .unwrap_or(text);
        if let Some(number) = parse_number(text) {
            return Some(Self::Number(number));
        }
        // A URL's address and its scheme's port. Without a scheme it is one only with a
        // pull request's whole path after a host that reads as one (`github.com`, not
        // `acme`), which no other spelling has.
        let address = match text.split_once("://") {
            Some((scheme, address)) => match scheme.to_ascii_lowercase().as_str() {
                "https" => Some((address, "443")),
                "http" => Some((address, "80")),
                _ => return None,
            },
            None => text
                .split_once('/')
                .filter(|(host, path)| host.contains('.') && path.contains("/pull/"))
                .map(|_| (text, "443")),
        };
        if let Some((address, default_port)) = address {
            let address = address.split(['?', '#']).next().unwrap_or(address);
            let mut parts = address.split('/');
            let (authority, owner, repo) = (parts.next()?, parts.next()?, parts.next()?);
            let number = match (parts.next()?, parts.next()?) {
                ("pull", number) => parse_number(
                    number
                        .strip_suffix(".diff")
                        .or_else(|| number.strip_suffix(".patch"))
                        .unwrap_or(number),
                )?,
                _ => return None,
            };
            let host = url_host(authority, Some(default_port))?;
            return (is_name(owner) && is_name(repo)).then(|| Self::Url {
                host,
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
            });
        }
        let (repository, number) = text.split_once('#')?;
        let number = parse_number(number)?;
        if repository.is_empty() {
            return Some(Self::Hash(number));
        }
        let (owner, repo) = repository.split_once('/')?;
        (is_name(owner) && is_name(repo)).then(|| Self::Short {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        })
    }

    pub fn number(&self) -> u64 {
        match self {
            Self::Number(number)
            | Self::Hash(number)
            | Self::Short { number, .. }
            | Self::Url { number, .. } => *number,
        }
    }

    /// The pull request in `repo`, the clone's repository; [`GithubError::OtherRepository`]
    /// when the reference names another one (PLAN §9.3).
    pub fn in_repo(&self, repo: &GithubRepo) -> Result<PrLink, GithubError> {
        let other = match self {
            Self::Number(_) | Self::Hash(_) => None,
            Self::Short {
                owner, repo: name, ..
            } => (!repo.is(&repo.host, owner, name)).then(|| format!("{owner}/{name}")),
            Self::Url {
                host,
                owner,
                repo: name,
                ..
            } => (!repo.is(host, owner, name)).then(|| {
                // The same name on another host is told apart by its host.
                if repo.is(&repo.host, owner, name) {
                    format!("{host}/{owner}/{name}")
                } else {
                    format!("{owner}/{name}")
                }
            }),
        };
        match other {
            Some(repository) => Err(GithubError::OtherRepository {
                number: self.number(),
                repository,
            }),
            None => Ok(PrLink {
                owner: repo.owner.clone(),
                repo: repo.repo.clone(),
                number: self.number(),
            }),
        }
    }
}

/// A pull request's number: decimal digits, not zero.
fn parse_number(text: &str) -> Option<u64> {
    let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    text.parse().ok().filter(|&number| digits && number > 0)
}

impl FromStr for PrRef {
    type Err = InvalidPrRef;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text).ok_or_else(|| InvalidPrRef(text.to_string()))
    }
}

/// A string that names no pull request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a pull request (a number, #482, owner/repo#482 or its URL): {0:?}")]
pub struct InvalidPrRef(pub String);
