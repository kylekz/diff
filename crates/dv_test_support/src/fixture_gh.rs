//! `gh`'s side of the fixture GitHub (docs/PLAN.md §9.11): the `mock_gh` scenario entries
//! that answer everything dv asks about one pull request, in the shapes gh and GitHub give
//! them, so a test names a pull request once and gets its `gh pr view`, its review threads,
//! its files and the viewer's login.
//!
//! The entries match by argument and, for GraphQL, by a field only that query selects
//! (`stdin_contains`), so their order among themselves doesn't matter; a test that wants one
//! call to fail puts its own entry before them (the first match answers).

use serde_json::{Value, json};

use crate::{FixtureGithub, FixtureRepo, GITHUB_OWNER};

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

    /// [`GhPr::calls`] about it in `github`'s repository, each line thread that names no
    /// `diffHunk` given the one GitHub shows above it, cut from the author's repository
    /// ([`cut_hunk`]).
    pub fn calls_in(&self, github: &FixtureGithub) -> Vec<Value> {
        let mut pr = self.clone();
        for thread in &mut pr.threads {
            cut_hunk(github.author(), &self.base_branch, thread);
        }
        pr.calls(github.name())
    }
}

/**
 * Gives `thread`, a `reviewThreads` node on a line whose first comment has an empty
 * `diffHunk`, the hunk GitHub shows above it: the hunk of the pull request's diff of its
 * file (the merge base with `base_branch` against the comment's commit) from its header
 * through the thread's line on its side, where GitHub cuts it. A line no hunk holds gets a
 * hunk of that line alone. A thread whose commit `repo` doesn't hold, or whose line its
 * file doesn't have, stays as it is.
 */
fn cut_hunk(repo: &FixtureRepo, base_branch: &str, thread: &mut Value) {
    let comment = &thread["comments"]["nodes"][0];
    if thread["subjectType"] != "LINE" || comment["diffHunk"] != "" {
        return;
    }
    let git = |args: &[&str]| {
        let output = repo.git_command().args(args).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let left = thread["diffSide"] == "LEFT";
    let hunk = (|| {
        let commit = comment["commit"]["oid"]
            .as_str()
            .or_else(|| comment["originalCommit"]["oid"].as_str())?;
        let line = thread["line"]
            .as_u64()
            .or_else(|| thread["originalLine"].as_u64())?;
        let path = thread["path"].as_str()?;
        let base = git(&["merge-base", base_branch, commit])?;
        let base = base.trim();
        let diff = git(&["diff", "--no-color", "-U3", "-M", base, commit])?;
        cut(&diff, path, left, line).or_else(|| {
            // Outside every hunk: the line alone, as it stands on its side.
            let at = if left { base } else { commit };
            let text = git(&["show", &format!("{at}:{path}")])?;
            // Not text: GitHub shows no hunk for it.
            if text.contains('\0') {
                return None;
            }
            let row = text
                .lines()
                .nth(usize::try_from(line).ok()?.checked_sub(1)?)?;
            Some(format!("@@ -{line} +{line} @@\n {row}"))
        })
    })();
    if let Some(hunk) = hunk {
        thread["comments"]["nodes"][0]["diffHunk"] = Value::from(hunk);
    }
}

/// The hunk of `path` in `diff` (git's patch output) that holds `line` of the old (`left`)
/// or the new side, from its header through that line.
fn cut(diff: &str, path: &str, left: bool, line: u64) -> Option<String> {
    let (mut old_path, mut new_path) = ("", "");
    let (mut in_header, mut in_file) = (true, false);
    let (mut old_no, mut new_no) = (0, 0);
    let mut hunk: Vec<&str> = Vec::new();
    // Split at `\n` alone: a CRLF file's rows keep their `\r`, as GitHub's do.
    for row in diff.split('\n') {
        if row.starts_with("diff --git ") {
            in_header = true;
        } else if in_header && let Some(old) = row.strip_prefix("--- ") {
            old_path = old.strip_prefix("a/").unwrap_or(old);
        } else if in_header && let Some(new) = row.strip_prefix("+++ ") {
            new_path = new.strip_prefix("b/").unwrap_or(new);
        } else if let Some(header) = row.strip_prefix("@@ -") {
            in_header = false;
            in_file = new_path == path || (new_path == "/dev/null" && old_path == path);
            let mut starts = header.split(' ').map(|range| {
                let start = range.trim_start_matches('+');
                start.split(',').next().and_then(|start| start.parse().ok())
            });
            old_no = starts.next()??;
            new_no = starts.next()??;
            hunk = vec![row];
        } else if !in_header && in_file {
            hunk.push(row);
            let (on_old, on_new) = match row.as_bytes().first() {
                Some(b'+') => (false, true),
                Some(b'-') => (true, false),
                Some(b'\\') => (false, false),
                _ => (true, true),
            };
            if (left && on_old && old_no == line) || (!left && on_new && new_no == line) {
                return Some(hunk.join("\n"));
            }
            old_no += u64::from(on_old);
            new_no += u64::from(on_new);
        }
    }
    None
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

/// One item of `gh pr list --json number,title,author,isDraft,updatedAt`: an open pull
/// request by `author`, last changed at `updated_at` (`2026-10-19T08:30:00Z`).
pub fn listed(number: u64, title: &str, author: &str, draft: bool, updated_at: &str) -> Value {
    json!({
        "number": number,
        "title": title,
        "author": {"id": "U_kgDOfixture", "is_bot": false, "login": author, "name": ""},
        "isDraft": draft,
        "updatedAt": updated_at,
    })
}

/// The two `mock_gh` entries answering the PR picker's list (PLAN §9.9): `open`, the
/// repository's open pull requests ([`listed`]), and `requested`, the ones that ask for the
/// viewer's review, in the same shape.
pub fn pr_list(open: Vec<Value>, requested: Vec<Value>) -> Vec<Value> {
    vec![
        json!({"argv": ["pr", "list", "--search", "**"], "stdout": requested}),
        json!({"argv": ["pr", "list", "--json", "**"], "stdout": open}),
    ]
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
/// in `commit`, with its one comment, `body`. Its `diffHunk` is empty until
/// [`GhPr::calls_in`] cuts the one GitHub shows above it.
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
            "diffHunk": "",
            "outdated": false,
            "commit": {"oid": commit},
            "originalCommit": {"oid": commit},
            "replyTo": null,
            "pullRequestReview": {"id": "PRR_1", "fullDatabaseId": "2001", "state": "COMMENTED"},
        })]),
    })
}

/// The `mock_gh` entry answering Resolve (`resolved`) or Unresolve of thread `id` as GitHub
/// does when it takes it: `resolveReviewThread` / `unresolveReviewThread`, matched by the
/// mutation's name and the thread's id in the request.
pub fn resolve(id: &str, resolved: bool) -> Value {
    let name = resolve_mutation(resolved);
    json!({
        "argv": ["api", "graphql", "**"],
        "stdin_contains": resolve_request(id, resolved),
        "stdout": {"data": {name: {"thread": {"id": id, "isResolved": resolved}}}}
    })
}

/// What a request to resolve (or unresolve) thread `id` holds, for an entry's
/// `stdin_contains`: the mutation's name as the request opens it (`resolveReviewThread` alone
/// is also the end of `unresolveReviewThread`) and the thread's id among its variables.
pub fn resolve_request(id: &str, resolved: bool) -> Vec<String> {
    vec![
        format!("{{ {}(", resolve_mutation(resolved)),
        format!("\"thread\":\"{id}\""),
    ]
}

fn resolve_mutation(resolved: bool) -> &'static str {
    if resolved {
        "resolveReviewThread"
    } else {
        "unresolveReviewThread"
    }
}
