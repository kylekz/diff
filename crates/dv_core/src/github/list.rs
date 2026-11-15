//! A repository's open pull requests, as the PR picker lists them (docs/PLAN.md §9.9): two
//! `gh pr list` calls with the same fields (`--json number,title,author,isDraft,updatedAt
//! --limit 50`), the open ones and the ones that ask for the viewer's review (`--search
//! review-requested:@me`), put together as one list: the requested ones first, then the rest
//! of the open ones, each once. So a pull request that asks for the viewer's review is
//! listed however many newer ones are open.
//!
//! The first call failing fails the list. The second failing leaves the open ones listed
//! without their marks, and [`Listing::requested`] says why the ones asking for a review
//! couldn't be read.
//!
//! Read leniently where GitHub leaves things out, as `gh pr view` is (`super::pr`): a
//! deleted author is `ghost`, a bot's login comes without gh's `app/`.

use std::collections::HashSet;

use serde::Deserialize;

use super::Gh;
use super::error::{GithubError, unexpected};
use super::pr::{GHOST, Login};
use super::repo::GithubRepo;
use super::time::parse_timestamp;

/// How many pull requests each call asks for.
pub const LIST_LIMIT: u32 = 50;

/// The fields both calls ask `gh pr list --json` for.
pub const LIST_FIELDS: &str = "number,title,author,isDraft,updatedAt";

/// The search the second call hands `gh pr list --search`: the open pull requests that ask
/// for a review by the login gh is signed in as.
pub const REVIEW_REQUESTED: &str = "review-requested:@me";

/// One open pull request of a repository's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedPr {
    pub number: u64,
    pub title: String,
    /// The author's login; `ghost` for a deleted account.
    pub author: String,
    pub draft: bool,
    /// When it last changed, in ms since the Unix epoch; `None` when gh's answer names no
    /// time dv reads.
    pub updated_at: Option<u64>,
    /// It asks for the viewer's review.
    pub review_requested: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Listed {
    number: u64,
    #[serde(default)]
    title: String,
    author: Option<Login>,
    #[serde(default)]
    is_draft: bool,
    updated_at: Option<String>,
}

/// What [`list`] read.
#[derive(Debug)]
pub struct Listing {
    /// The open pull requests, the ones asking for the viewer's review first.
    pub prs: Vec<ListedPr>,
    /// Why the ones asking for the viewer's review couldn't be read: `prs` is then the open
    /// ones, none of them marked.
    pub requested: Option<GithubError>,
}

/// `repo`'s open pull requests (module docs): the ones asking for the viewer's review
/// first, then the rest, each group in gh's order.
pub fn list(gh: &Gh, repo: &GithubRepo) -> Result<Listing, GithubError> {
    let limit = LIST_LIMIT.to_string();
    let open = gh
        .repo_json(
            repo,
            &["pr", "list", "--json", LIST_FIELDS, "--limit", &limit],
        )
        .and_then(read)?;
    let requested = gh
        .repo_json(
            repo,
            &[
                "pr",
                "list",
                "--search",
                REVIEW_REQUESTED,
                "--json",
                LIST_FIELDS,
                "--limit",
                &limit,
            ],
        )
        .and_then(read);
    let (requested, failed) = match requested {
        Ok(requested) => (requested, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let asked: HashSet<u64> = requested.iter().map(|pr| pr.number).collect();
    let listed = |pr: Listed, review_requested: bool| ListedPr {
        review_requested,
        number: pr.number,
        title: pr.title,
        author: Login::name(pr.author).unwrap_or_else(|| GHOST.to_string()),
        draft: pr.is_draft,
        updated_at: pr.updated_at.as_deref().and_then(parse_timestamp),
    };
    let prs = requested
        .into_iter()
        .map(|pr| listed(pr, true))
        .chain(
            open.into_iter()
                .filter(|pr| !asked.contains(&pr.number))
                .map(|pr| listed(pr, false)),
        )
        .collect();
    Ok(Listing {
        prs,
        requested: failed,
    })
}

/// gh's answer as the pull requests the list asked for.
fn read(answer: serde_json::Value) -> Result<Vec<Listed>, GithubError> {
    let text = answer.to_string();
    serde_json::from_value(answer).map_err(|error| unexpected("pr list", &error.to_string(), &text))
}
