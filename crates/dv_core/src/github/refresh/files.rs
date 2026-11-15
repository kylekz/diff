//! What the mirror holds per file and per viewer (docs/PLAN.md §9.4): each file's Viewed
//! state (GraphQL `pullRequest.files`, PLAN §9.7), GitHub's own patch for each file at the
//! head (REST `pulls/<n>/files`, PLAN §9.8), and the viewer's pending review.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::{database_id, pr_variables};
use crate::git::Oid;
use crate::github::error::{GithubError, unexpected};
use crate::github::mirror::{FilePatch, FileStatus, PendingReview, ViewedState};
use crate::github::repo::GithubRepo;
use crate::github::{Connection, Gh};

/// GitHub's Viewed state of each file of pull request `number` for the viewer, by path.
pub(super) fn viewed(
    gh: &Gh,
    repo: &GithubRepo,
    number: u64,
) -> Result<BTreeMap<String, ViewedState>, GithubError> {
    const QUERY: &str = "query($owner: String!, $repo: String!, $number: Int!, $first: Int!, \
        $cursor: String) { repository(owner: $owner, name: $repo) { pullRequest(number: \
        $number) { files(first: $first, after: $cursor) { nodes { path viewerViewedState } \
        pageInfo { hasNextPage endCursor } } } } }";
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct File {
        path: String,
        viewer_viewed_state: String,
    }
    let paged = gh.connection(
        &repo.host,
        &Connection {
            query: QUERY,
            variables: pr_variables(repo, number, Map::new()),
            path: &["repository", "pullRequest", "files"],
            nested: None,
        },
    )?;
    if let Some(why) = paged.truncated {
        log::warn!(
            "{}#{number}: its files' Viewed states were cut short ({why:?}); the mirror holds \
             the first {}",
            repo.name_with_owner(),
            paged.nodes.len()
        );
    }
    paged
        .nodes
        .into_iter()
        .map(|node| {
            let text = node.to_string();
            let wrong = |detail: &str| unexpected("api graphql", detail, &text);
            let file: File =
                serde_json::from_value(node).map_err(|error| wrong(&error.to_string()))?;
            let state = match file.viewer_viewed_state.as_str() {
                "VIEWED" => ViewedState::Viewed,
                "UNVIEWED" => ViewedState::Unviewed,
                "DISMISSED" => ViewedState::Dismissed,
                other => ViewedState::Other(other.to_string()),
            };
            Ok((file.path, state))
        })
        .collect()
}

/// The files pull request `number` changes at its head, each with GitHub's own patch, in
/// GitHub's order (its first 3,000).
pub(super) fn patches(
    gh: &Gh,
    repo: &GithubRepo,
    number: u64,
) -> Result<Vec<FilePatch>, GithubError> {
    #[derive(Deserialize)]
    struct File {
        filename: String,
        previous_filename: Option<String>,
        status: FileStatus,
        patch: Option<String>,
    }
    gh.repo_list(repo, &format!("pulls/{number}/files"))?
        .into_iter()
        .map(|item| {
            let text = item.to_string();
            let file: File = serde_json::from_value(item)
                .map_err(|error| unexpected("api", &error.to_string(), &text))?;
            Ok(FilePatch {
                path: file.filename,
                previous_path: file.previous_filename,
                status: file.status,
                patch: file.patch,
            })
        })
        .collect()
}

/// The pending review `login`, the viewer, has on pull request `number`, when there is one:
/// GitHub allows each user one per pull request, and shows it to nobody else.
pub(super) fn pending_review(
    gh: &Gh,
    repo: &GithubRepo,
    number: u64,
    login: &str,
) -> Result<Option<PendingReview>, GithubError> {
    const QUERY: &str = "query($owner: String!, $repo: String!, $number: Int!, $login: \
        String!) { repository(owner: $owner, name: $repo) { pullRequest(number: $number) { \
        reviews(states: [PENDING], author: $login, first: 1) { nodes { id fullDatabaseId \
        body commit { oid } } } } } }";
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Review {
        id: String,
        /// `null` or absent: kept as none.
        #[serde(default)]
        full_database_id: Value,
        #[serde(default)]
        body: String,
        commit: Option<Commit>,
    }
    #[derive(Deserialize)]
    struct Commit {
        oid: Oid,
    }
    let login = Map::from_iter([("login".to_string(), Value::from(login))]);
    let mut data = gh.graphql(&repo.host, QUERY, &pr_variables(repo, number, login))?;
    let text = data.to_string();
    let wrong = |detail: &str| unexpected("api graphql", detail, &text);
    let nodes = data
        .pointer_mut("/repository/pullRequest/reviews/nodes")
        .map(Value::take)
        .ok_or_else(|| wrong("no repository.pullRequest.reviews.nodes"))?;
    let reviews: Vec<Review> =
        serde_json::from_value(nodes).map_err(|error| wrong(&error.to_string()))?;
    reviews
        .into_iter()
        .next()
        .map(|review| {
            Ok(PendingReview {
                database_id: database_id(&review.full_database_id)
                    .ok_or_else(|| wrong("a review without its fullDatabaseId"))?,
                id: review.id,
                body: review.body,
                commit: review.commit.map(|commit| commit.oid),
            })
        })
        .transpose()
}
