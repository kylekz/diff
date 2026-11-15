//! Tests for the GitHub plumbing that needs no `gh` (docs/PLAN.md §9.11): the remote rules,
//! the PR reference parser, the mirror and its store, how names are spelled, timestamps. The runner's are beside it
//! (`gh/tests`).

mod mirror;
mod pr_ref;
mod repo;
mod spelling;
mod time;

use super::*;

fn github_repo(host: &str, owner: &str, repo: &str) -> GithubRepo {
    GithubRepo {
        host: host.to_string(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        remote: "origin".to_string(),
    }
}
