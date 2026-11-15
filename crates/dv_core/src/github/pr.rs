//! A pull request as `gh pr view` tells it (docs/PLAN.md §9.3 step 1): its metadata and its
//! checks summarised, read into the mirror's own types.
//!
//! gh's JSON is read leniently where GitHub leaves things out: a deleted author is `ghost`,
//! a null `statusCheckRollup` and an empty `reviewDecision` are none, a deleted fork has no
//! owner. gh writes a bot's login as `app/<login>` (`is_bot`): the mirror keeps the login
//! alone, as GraphQL names the same bot on its comments. Anything else that isn't the shape asked for is
//! [`GithubError::UnexpectedOutput`].

use serde::Deserialize;

use super::Gh;
use super::error::{GithubError, unexpected};
use super::mirror::{Checks, HeadRepo, PrState, PullRequest, ReviewDecision};
use super::repo::GithubRepo;
use super::time::parse_timestamp;
use crate::git::Oid;

/// The fields [`view`] asks `gh pr view --json` for.
pub const VIEW_FIELDS: &str = "id,number,title,body,url,state,isDraft,author,baseRefName,\
    baseRefOid,headRefName,headRefOid,headRepositoryOwner,isCrossRepository,mergeCommit,\
    updatedAt,reviewDecision,statusCheckRollup,changedFiles,additions,deletions";

/// The login GitHub shows for a deleted account.
pub const GHOST: &str = "ghost";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct View {
    id: String,
    number: u64,
    title: String,
    body: Option<String>,
    url: String,
    state: String,
    #[serde(default)]
    is_draft: bool,
    author: Option<Login>,
    base_ref_name: String,
    base_ref_oid: Option<String>,
    merge_commit: Option<Commit>,
    head_ref_name: String,
    head_ref_oid: Oid,
    head_repository_owner: Option<Login>,
    #[serde(default)]
    is_cross_repository: bool,
    updated_at: String,
    review_decision: Option<String>,
    status_check_rollup: Option<Vec<Check>>,
    #[serde(default)]
    changed_files: u32,
    #[serde(default)]
    additions: u32,
    #[serde(default)]
    deletions: u32,
}

/// An account as gh's own commands write one (`author`, `headRepositoryOwner`).
#[derive(Deserialize)]
pub(super) struct Login {
    login: Option<String>,
    #[serde(default)]
    is_bot: bool,
}

impl Login {
    /// The login, when the account still has one: a bot's without gh's `app/` before it,
    /// and none for the empty login (or the bare `app/`) a deleted account leaves.
    pub(super) fn name(login: Option<Login>) -> Option<String> {
        let Login { login, is_bot } = login?;
        let login = login?;
        let name = match login.strip_prefix("app/") {
            Some(bot) if is_bot || bot.is_empty() => bot,
            _ => login.as_str(),
        };
        (!name.is_empty()).then(|| name.to_string())
    }
}

#[derive(Deserialize)]
struct Commit {
    oid: Option<String>,
}

/// A pull request as `gh pr view` told it ([`view`]): what the mirror keeps, and what only
/// the fetch needs to find the old side (`super::fetch`).
#[derive(Debug, Clone)]
pub(super) struct Viewed {
    pub pr: PullRequest,
    pub checks: Checks,
    /// GitHub's `baseRefOid`: the base branch's commit as GitHub last recorded it, which
    /// outlives a deleted base branch.
    pub base_oid: Option<Oid>,
    /// The commit a merged pull request was merged as.
    pub merge_commit: Option<Oid>,
}

/// One entry of `statusCheckRollup`: a check run (`status`, and `conclusion` once it
/// completed) or a commit status (`state`).
#[derive(Deserialize)]
struct Check {
    state: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Head {
    head_ref_oid: Oid,
}

/// Pull request `number` of `repo`: `gh pr view <n> --json … --repo=…`.
pub(super) fn view(gh: &Gh, repo: &GithubRepo, number: u64) -> Result<Viewed, GithubError> {
    let answer = gh.repo_json(
        repo,
        &["pr", "view", &number.to_string(), "--json", VIEW_FIELDS],
    )?;
    let text = answer.to_string();
    let wrong = |detail: &str| unexpected("pr view", detail, &text);
    let view: View = serde_json::from_value(answer).map_err(|error| wrong(&error.to_string()))?;
    // Everything dv keeps of this answer it keeps under the number it asked for.
    if view.number != number {
        return Err(wrong(&format!(
            "pull request {}, asked for {number}",
            view.number
        )));
    }
    let state = match (view.state.as_str(), view.is_draft) {
        ("MERGED", _) => PrState::Merged,
        ("CLOSED", _) => PrState::Closed,
        ("OPEN", true) => PrState::Draft,
        ("OPEN", false) => PrState::Open,
        (other, _) => return Err(wrong(&format!("an unknown state, {other:?}"))),
    };
    let review_decision = match view.review_decision.as_deref() {
        None | Some("") => None,
        Some("APPROVED") => Some(ReviewDecision::Approved),
        Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
        Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
        // Nothing dv shows rests on it: one it doesn't know is kept as GitHub sent it.
        Some(other) => Some(ReviewDecision::Other(other.to_string())),
    };
    let updated_at = parse_timestamp(&view.updated_at)
        .ok_or_else(|| wrong(&format!("updatedAt isn't a time: {:?}", view.updated_at)))?;
    let checks = summarise(view.status_check_rollup.as_deref().unwrap_or_default());
    // The base branch's name goes into the fetch's refspec (`super::fetch`).
    if !is_branch_name(&view.base_ref_name) {
        return Err(wrong(&format!(
            "baseRefName isn't a branch's name: {:?}",
            view.base_ref_name
        )));
    }
    let pr = PullRequest {
        id: view.id,
        title: view.title,
        body: view.body.unwrap_or_default(),
        url: view.url,
        state,
        author: Login::name(view.author).unwrap_or_else(|| GHOST.to_string()),
        base_ref: view.base_ref_name,
        head_ref: view.head_ref_name,
        head_oid: view.head_ref_oid,
        head_repo: if view.is_cross_repository {
            HeadRepo::Fork {
                owner: Login::name(view.head_repository_owner),
            }
        } else {
            HeadRepo::Same
        },
        updated_at,
        review_decision,
        changed_files: view.changed_files,
        additions: view.additions,
        deletions: view.deletions,
    };
    Ok(Viewed {
        pr,
        checks,
        base_oid: view.base_ref_oid.as_deref().and_then(Oid::parse),
        merge_commit: view
            .merge_commit
            .and_then(|commit| commit.oid)
            .as_deref()
            .and_then(Oid::parse),
    })
}

/// Whether `name` can be a branch's name by git's own rules for a ref (`git
/// check-ref-format`), checked here because gh's answer is put into a refspec, where a `:`
/// would end the source and a leading `-` could read as an option: not empty; no component
/// empty, starting with `.` or ending with `.lock`; no `..`, `@{`, space, control character
/// or any of `~^:?*[\`; not `@` alone; no leading `-`, no trailing `/` or `.`.
pub(super) fn is_branch_name(name: &str) -> bool {
    let component =
        |part: &str| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock");
    !name.is_empty()
        && name != "@"
        && !name.starts_with('-')
        && !name.ends_with(['/', '.'])
        && !name.contains("..")
        && !name.contains("@{")
        && !name
            .chars()
            .any(|c| c.is_control() || " ~^:?*[\\".contains(c))
        && name.split('/').all(component)
}

/// The pull request's head as GitHub has it now: `gh pr view <n> --json headRefOid`.
pub(super) fn head(gh: &Gh, repo: &GithubRepo, number: u64) -> Result<Oid, GithubError> {
    let answer = gh.repo_json(
        repo,
        &["pr", "view", &number.to_string(), "--json", "headRefOid"],
    )?;
    let text = answer.to_string();
    serde_json::from_value::<Head>(answer)
        .map(|head| head.head_ref_oid)
        .map_err(|error| unexpected("pr view", &error.to_string(), &text))
}

/// The checks on the head commit, counted as gh's own `pr status` counts them: a completed
/// check run by its conclusion, one still running by its status, a commit status by its
/// state; success, neutral and skipped pass, failure, error, cancelled, timed out and action
/// required fail, and everything else is pending.
fn summarise(rollup: &[Check]) -> Checks {
    let mut checks = Checks::default();
    for check in rollup {
        let state = match (check.status.as_deref(), check.conclusion.as_deref()) {
            (Some("COMPLETED"), Some(conclusion)) => conclusion,
            (Some(status), _) => status,
            (None, _) => check.state.as_deref().unwrap_or_default(),
        };
        match state {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => checks.passing += 1,
            "ERROR" | "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" => {
                checks.failing += 1;
            }
            _ => checks.pending += 1,
        }
    }
    checks
}
