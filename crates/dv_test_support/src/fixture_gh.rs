//! `gh`'s side of the fixture GitHub (docs/PLAN.md §9.11): the `mock_gh` scenario entries
//! that answer everything dv asks about one pull request, in the shapes gh and GitHub give
//! them, so a test names a pull request once and gets its `gh pr view`, its review threads,
//! its files and the viewer's login.
//!
//! The entries match by argument and, for GraphQL, by a field only that query selects
//! (`stdin_contains`), so their order among themselves doesn't matter; a test that wants one
//! call to fail puts its own entry before them (the first match answers).

use serde_json::{Value, json};

use crate::GITHUB_OWNER;

/// A pull request as the fixture GitHub's `gh` tells it.
#[derive(Debug, Clone)]
pub struct GhPr {
    pub number: u64,
    /// The head commit `gh` reports (`headRefOid`).
    pub head: String,
    pub title: String,
    pub body: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    pub is_draft: bool,
    /// The author's login; `None` for a deleted account.
    pub author: Option<String>,
    /// The author is a bot (a GitHub App): gh then writes its login `app/<login>`.
    pub bot: bool,
    pub base_branch: String,
    /// GitHub's `baseRefOid`: the base branch's commit as GitHub last recorded it; left out
    /// when `None`.
    pub base_oid: Option<String>,
    /// The commit a merged pull request was merged as (`mergeCommit`).
    pub merge_commit: Option<String>,
    pub head_branch: String,
    /// The fork's owner when the head lives in another repository: `Some(None)` for a fork
    /// that was deleted.
    pub fork: Option<Option<String>>,
    /// `statusCheckRollup`: `null`, or its entries.
    pub checks: Value,
    /// `pullRequest.reviewThreads` nodes ([`thread`]).
    pub threads: Vec<Value>,
    /// The viewer's pending review node, when there is one.
    pub pending_review: Option<Value>,
    /// REST `pulls/<n>/files` items ([`file`]).
    pub files: Vec<Value>,
    /// Each file's `viewerViewedState`, by path; a file of `files` not named here is
    /// `UNVIEWED`.
    pub viewed: Vec<(String, String)>,
}

impl GhPr {
    /// An open pull request `number` from `feat` into `main` whose head is `head`, with no
    /// threads, checks or files.
    pub fn new(number: u64, head: &str) -> Self {
        Self {
            number,
            head: head.to_string(),
            title: "Add the answer".to_string(),
            body: "It was missing.".to_string(),
            state: "OPEN".to_string(),
            is_draft: false,
            author: Some("octocat".to_string()),
            bot: false,
            base_branch: "main".to_string(),
            base_oid: None,
            merge_commit: None,
            head_branch: "feat".to_string(),
            fork: None,
            checks: Value::Null,
            threads: Vec::new(),
            pending_review: None,
            files: Vec::new(),
            viewed: Vec::new(),
        }
    }

    /// What `gh pr view <n> --json …` prints for it in `dv-fixture/<repo>`.
    pub fn view(&self, repo: &str) -> Value {
        let head_owner = match &self.fork {
            None => json!({"id": "O_1", "login": GITHUB_OWNER}),
            Some(Some(owner)) => json!({"id": "O_2", "login": owner}),
            Some(None) => json!({"id": "", "login": ""}),
        };
        // gh's own shapes: a user with its id and name, a bot as `app/<login>`, and what a
        // deleted account leaves of either.
        let author = match (&self.author, self.bot) {
            (Some(login), false) => {
                json!({"id": "U_1", "is_bot": false, "login": login, "name": ""})
            }
            (Some(login), true) => json!({"is_bot": true, "login": format!("app/{login}")}),
            (None, false) => json!({"id": "", "is_bot": false, "login": "", "name": ""}),
            (None, true) => json!({"is_bot": true, "login": "app/"}),
        };
        let mut view = json!({
            "id": format!("PR_{}", self.number),
            "number": self.number,
            "title": self.title,
            "body": self.body,
            "url": format!("https://github.com/{GITHUB_OWNER}/{repo}/pull/{}", self.number),
            "state": self.state,
            "isDraft": self.is_draft,
            "author": author,
            "baseRefName": self.base_branch,
            "headRefName": self.head_branch,
            "headRefOid": self.head,
            "headRepositoryOwner": head_owner,
            "isCrossRepository": self.fork.is_some(),
            "updatedAt": "2026-10-19T08:30:00Z",
            "reviewDecision": "",
            "statusCheckRollup": self.checks,
            "changedFiles": self.files.len(),
            "additions": 3,
            "deletions": 0,
            "mergeCommit": self.merge_commit.as_ref().map(|oid| json!({"oid": oid})),
        });
        if let Some(base) = &self.base_oid {
            view["baseRefOid"] = Value::from(base.as_str());
        }
        view
    }

    /// The `mock_gh` entries answering every call dv makes about it in `dv-fixture/<repo>`
    /// (module docs). The viewer's login is [`viewer`]'s to answer.
    pub fn calls(&self, repo: &str) -> Vec<Value> {
        let number = self.number.to_string();
        let viewed: Vec<Value> = self
            .files
            .iter()
            .filter_map(|file| file["filename"].as_str())
            .map(|path| {
                let state = self
                    .viewed
                    .iter()
                    .find(|(viewed, _)| viewed == path)
                    .map_or("UNVIEWED", |(_, state)| state.as_str());
                json!({"path": path, "viewerViewedState": state})
            })
            .collect();
        let in_pr = |field: &str, value: Value| json!({"data": {"repository": {"pullRequest": {field: value}}}});
        let pending: Vec<&Value> = self.pending_review.iter().collect();
        vec![
            json!({
                "argv": ["pr", "view", number, "--json", "headRefOid", "**"],
                "stdout": {"headRefOid": self.head}
            }),
            json!({
                "argv": ["pr", "view", number, "--json", "*", "**"],
                "stdout": self.view(repo)
            }),
            json!({
                "argv": ["api", "graphql", "**"],
                "stdin_contains": "reviewThreads(first",
                "stdout": in_pr("reviewThreads", connection(self.threads.clone()))
            }),
            json!({
                "argv": ["api", "graphql", "**"],
                "stdin_contains": "viewerViewedState",
                "stdout": in_pr("files", connection(viewed))
            }),
            json!({
                "argv": ["api", "graphql", "**"],
                "stdin_contains": "states: [PENDING]",
                "stdout": in_pr("reviews", json!({"nodes": pending}))
            }),
            json!({
                "argv": [
                    "api",
                    format!("repos/{GITHUB_OWNER}/{repo}/pulls/{number}/files?per_page=100"),
                    "**"
                ],
                "stdout": [self.files]
            }),
        ]
    }
}

/// The `mock_gh` entry answering `gh api user`: gh is signed in as `login`.
pub fn viewer(login: &str) -> Value {
    json!({"argv": ["api", "user", "**"], "stdout": {"login": login, "id": 1}})
}

/// The `mock_gh` entry for a `gh` that isn't signed in, whatever it is asked: gh's exit code
/// 4 and its words.
pub fn signed_out() -> Value {
    json!({
        "argv": ["**"],
        "stderr": "To get started with GitHub CLI, please run:  gh auth login\n\
                   Alternatively, populate the GH_TOKEN environment variable with a GitHub API \
                   authentication token.\n",
        "exit": 4
    })
}

/// A GraphQL connection's one and last page.
pub fn connection(nodes: Vec<Value>) -> Value {
    json!({"nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}})
}

/// One item of REST `pulls/<n>/files`: `status` as GitHub names it (`added`, `modified`,
/// `removed`, `renamed`), `patch` GitHub's unified diff of the file, without a file header.
pub fn file(path: &str, status: &str, patch: Option<&str>) -> Value {
    let mut file = json!({
        "sha": "0000000000000000000000000000000000000000",
        "filename": path,
        "status": status,
        "additions": 1,
        "deletions": 0,
        "changes": 1,
    });
    if let Some(patch) = patch {
        file["patch"] = Value::from(patch);
    }
    file
}

/// One `reviewThreads` node: a thread `id` by `author` on line `line` of `path`'s new side
/// in `commit`, with its one comment, `body`.
pub fn thread(id: &str, path: &str, line: u32, author: &str, body: &str, commit: &str) -> Value {
    json!({
        "id": id,
        "isResolved": false,
        "isOutdated": false,
        "path": path,
        "line": line,
        "originalLine": line,
        "startLine": null,
        "originalStartLine": null,
        "diffSide": "RIGHT",
        "startDiffSide": null,
        "subjectType": "LINE",
        "viewerCanResolve": true,
        "viewerCanUnresolve": false,
        "viewerCanReply": true,
        "comments": connection(vec![json!({
            "id": format!("{id}_C1"),
            "fullDatabaseId": "1001",
            "author": {"login": author},
            "body": body,
            "createdAt": "2026-10-19T08:00:00Z",
            "url": format!("https://github.com/{GITHUB_OWNER}/api/pull/7#discussion_r1001"),
            "diffHunk": "@@ -0,0 +1,3 @@\n+pub fn answer() -> u32 {",
            "outdated": false,
            "commit": {"oid": commit},
            "originalCommit": {"oid": commit},
            "replyTo": null,
            "pullRequestReview": {"id": "PRR_1", "fullDatabaseId": "2001", "state": "COMMENTED"},
        })]),
    })
}
