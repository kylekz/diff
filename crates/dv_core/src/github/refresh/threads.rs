//! GitHub's review threads on a pull request (docs/PLAN.md §9.6), read whole for the mirror:
//! GraphQL `pullRequest.reviewThreads`, every page, and every page of each thread's comments
//! (PLAN §9.2).

use serde::Deserialize;
use serde_json::{Map, Value};

use super::{database_id, pr_variables};
use crate::git::Oid;
use crate::github::error::{GithubError, unexpected};
use crate::github::mirror::{
    CommentReview, GithubComment, GithubThread, ReviewState, ThreadLines, ThreadStart,
    ThreadSubject,
};
use crate::github::pr::GHOST;
use crate::github::repo::GithubRepo;
use crate::github::time::parse_timestamp;
use crate::github::{Connection, Gh, Nested};
use crate::review::AnchorSide;

/// What a comment's node selects (PLAN §9.6's field list).
const COMMENT_FIELDS: &str = "id fullDatabaseId author { login } body createdAt url diffHunk \
    outdated commit { oid } originalCommit { oid } replyTo { id } \
    pullRequestReview { id fullDatabaseId state }";

/// The threads of pull request `number` of `repo`, in GitHub's order.
pub(super) fn read(
    gh: &Gh,
    repo: &GithubRepo,
    number: u64,
) -> Result<Vec<GithubThread>, GithubError> {
    let query = format!(
        "query($owner: String!, $repo: String!, $number: Int!, $first: Int!, $cursor: String) {{ \
         repository(owner: $owner, name: $repo) {{ pullRequest(number: $number) {{ \
         reviewThreads(first: $first, after: $cursor) {{ nodes {{ id isResolved isOutdated path \
         line originalLine startLine originalStartLine diffSide startDiffSide subjectType \
         viewerCanResolve viewerCanUnresolve viewerCanReply comments(first: 100) {{ nodes {{ \
         {COMMENT_FIELDS} }} pageInfo {{ hasNextPage endCursor }} }} }} pageInfo {{ hasNextPage \
         endCursor }} }} }} }} }}"
    );
    let more_comments = format!(
        "query($id: ID!, $first: Int!, $cursor: String) {{ node(id: $id) {{ ... on \
         PullRequestReviewThread {{ comments(first: $first, after: $cursor) {{ nodes {{ \
         {COMMENT_FIELDS} }} pageInfo {{ hasNextPage endCursor }} }} }} }} }}"
    );
    let paged = gh.connection(
        &repo.host,
        &Connection {
            query: &query,
            variables: pr_variables(repo, number, Map::new()),
            path: &["repository", "pullRequest", "reviewThreads"],
            nested: Some(Nested {
                field: "comments",
                query: &more_comments,
                path: &["node", "comments"],
            }),
        },
    )?;
    if let Some(why) = paged.truncated {
        log::warn!(
            "{}#{number}: its review threads were cut short ({why:?}); the mirror holds the \
             first {}",
            repo.name_with_owner(),
            paged.nodes.len()
        );
    }
    paged.nodes.into_iter().map(thread).collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadNode {
    id: String,
    #[serde(default)]
    is_resolved: bool,
    #[serde(default)]
    is_outdated: bool,
    path: String,
    line: Option<u32>,
    original_line: Option<u32>,
    start_line: Option<u32>,
    original_start_line: Option<u32>,
    diff_side: Option<String>,
    start_diff_side: Option<String>,
    subject_type: Option<String>,
    #[serde(default)]
    viewer_can_resolve: bool,
    #[serde(default)]
    viewer_can_unresolve: bool,
    #[serde(default)]
    viewer_can_reply: bool,
    comments: Nodes<CommentNode>,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentNode {
    id: String,
    /// `null` or absent: kept as none.
    #[serde(default)]
    full_database_id: Value,
    author: Option<Login>,
    #[serde(default)]
    body: String,
    created_at: String,
    url: String,
    #[serde(default)]
    diff_hunk: String,
    #[serde(default)]
    outdated: bool,
    commit: Option<Commit>,
    original_commit: Option<Commit>,
    reply_to: Option<Id>,
    pull_request_review: Option<ReviewNode>,
}

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
struct Commit {
    oid: Oid,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewNode {
    id: String,
    /// `null` or absent: kept as none.
    #[serde(default)]
    full_database_id: Value,
    state: String,
}

/// One `reviewThreads` node as the mirror keeps it.
fn thread(node: Value) -> Result<GithubThread, GithubError> {
    let text = node.to_string();
    let wrong = |detail: &str| unexpected("api graphql", detail, &text);
    let node: ThreadNode =
        serde_json::from_value(node).map_err(|error| wrong(&error.to_string()))?;
    let side = |side: Option<&str>| match side {
        Some("LEFT") => Ok(AnchorSide::Old),
        Some("RIGHT") | None => Ok(AnchorSide::New),
        Some(other) => Err(wrong(&format!("an unknown diff side, {other:?}"))),
    };
    let subject = match node.subject_type.as_deref() {
        Some("FILE") => ThreadSubject::File,
        Some("LINE") | None => ThreadSubject::Lines(ThreadLines {
            side: side(node.diff_side.as_deref())?,
            line: node.line,
            original_line: node.original_line,
            start: match (node.start_line, node.original_start_line) {
                (None, None) => None,
                (line, original_line) => Some(ThreadStart {
                    side: side(
                        node.start_diff_side
                            .as_deref()
                            .or(node.diff_side.as_deref()),
                    )?,
                    line,
                    original_line,
                }),
            },
        }),
        Some(other) => return Err(wrong(&format!("an unknown subject type, {other:?}"))),
    };
    let comments = node
        .comments
        .nodes
        .into_iter()
        .map(|comment| comment_of(comment, &wrong))
        .collect::<Result<_, _>>()?;
    Ok(GithubThread {
        id: node.id,
        path: node.path,
        resolved: node.is_resolved,
        outdated: node.is_outdated,
        subject,
        viewer_can_resolve: node.viewer_can_resolve,
        viewer_can_unresolve: node.viewer_can_unresolve,
        viewer_can_reply: node.viewer_can_reply,
        comments,
    })
}

fn comment_of(
    node: CommentNode,
    wrong: &impl Fn(&str) -> GithubError,
) -> Result<GithubComment, GithubError> {
    let review = match node.pull_request_review {
        None => None,
        Some(review) => Some(CommentReview {
            database_id: database_id(&review.full_database_id),
            state: match review.state.as_str() {
                "PENDING" => ReviewState::Pending,
                "COMMENTED" => ReviewState::Commented,
                "APPROVED" => ReviewState::Approved,
                "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
                "DISMISSED" => ReviewState::Dismissed,
                // Only `PENDING` changes what a thread shows: another is kept as sent.
                other => ReviewState::Other(other.to_string()),
            },
            id: review.id,
        }),
    };
    Ok(GithubComment {
        // The read side names a comment by its node id: a missing database id costs nothing.
        database_id: database_id(&node.full_database_id),
        author: node
            .author
            .map_or_else(|| GHOST.to_string(), |author| author.login),
        created_at: parse_timestamp(&node.created_at)
            .ok_or_else(|| wrong(&format!("createdAt isn't a time: {:?}", node.created_at)))?,
        id: node.id,
        body: node.body,
        url: node.url,
        diff_hunk: node.diff_hunk,
        outdated: node.outdated,
        commit: node.commit.map(|commit| commit.oid),
        original_commit: node.original_commit.map(|commit| commit.oid),
        reply_to: node.reply_to.map(|reply| reply.id),
        review,
    })
}
