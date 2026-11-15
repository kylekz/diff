//! `dv review new --pr` and `dv review refresh` (docs/PLAN.md §9.10): a GitHub pull request
//! opened as a review, and a PR review refreshed, through `dv_core::github` and the `gh`
//! where the command runs. A failure of `gh` or of the fetch is exit 5, `github`, with the
//! error's kind first in its detail (`crate::error`).
//!
//! Either may be run by an agent: opening and refreshing read GitHub and write nothing a
//! reviewer wrote. A draft the command makes is the reviewer's all the same (PLAN §2.4), so
//! it is authored by the repository's `user.name`, whoever ran the command.

use dv_core::github::{self, PrRef};
use dv_core::review::reviewer_identity;

use super::format;
use crate::args::GlobalArgs;
use crate::context::Context;
use crate::dto::{MovedDto, ReviewNewDoc, ReviewRefreshDoc, StaleDto};
use crate::error::{CliError, Exit, github_message};
use crate::identity;
use crate::output;
use crate::repo::Repo;
use crate::select;

/// `dv review new --pr <n | #n | owner/repo#n | URL>`: the pull request's commits fetched,
/// its mirror read, and its review made or resumed (PLAN §9.3), printed as `review new`
/// prints one. A mirror that couldn't be read whole leaves the review open, and a GitHub
/// that couldn't be asked opens a review that is already here: a warning says why, the
/// document's `stale` carries it, and `dv review refresh` reads it again.
pub(crate) fn new(
    global: &GlobalArgs,
    reference: &str,
    ctx: &mut Context,
) -> Result<Exit, CliError> {
    let reference: PrRef = reference
        .parse()
        .map_err(|error: github::InvalidPrRef| CliError::usage(format!("--pr: {error}")))?;
    if global.review.is_some() {
        return Err(CliError::usage(
            "`dv review new` takes a pull request or --review, not both: a pull request opens \
             its own review",
        ));
    }
    // The flags that say who is asking are checked, though the draft is the reviewer's.
    identity::agent(global, ctx)?;
    let repo = Repo::open(global, ctx)?;
    let opened = github::open(&repo.backend, &repo.gh, &reference, || {
        reviewer_identity(repo.backend.as_ref())
    })?;
    let stale = opened.refreshed.stale.as_ref().map(|error| StaleDto {
        kind: error.kind(),
        message: github_message(error),
    });
    if let Some(stale) = &stale {
        output::warning(
            ctx,
            &format!(
                "what GitHub holds about the pull request couldn't be read ({}); `dv review \
                 refresh {}` reads it again",
                stale.message, opened.review.id
            ),
        )?;
    }
    let doc = ReviewNewDoc {
        created: opened.created,
        review: repo.review_dto(&opened.review, &repo.labels()),
        stale,
    };
    if global.json {
        output::json(ctx, &doc)?;
    } else {
        let verb = if doc.created { "started" } else { "resumed" };
        output::text(
            ctx,
            &[format!("{verb} {}", format::review_line(&doc.review))],
        )?;
    }
    Ok(Exit::Ok)
}

/// `dv review refresh [<id>]`: the PR review's pull request fetched and read again (PLAN
/// §9.4), a draft following its head (PLAN §9.5). A refresh that fails keeps the last mirror
/// and is exit 5.
pub(crate) fn refresh(
    global: &GlobalArgs,
    id: Option<&str>,
    ctx: &mut Context,
) -> Result<Exit, CliError> {
    identity::agent(global, ctx)?;
    let repo = Repo::open(global, ctx)?;
    let review = select::pr_review(&repo, global, ctx, id)?;
    let refreshed = github::refresh(&repo.backend, &repo.gh, &review);
    if let Some(error) = refreshed.stale {
        return Err(error.into());
    }
    let review = select::load(&repo, &review.id)?;
    let doc = ReviewRefreshDoc {
        review: repo.review_dto(&review, &repo.labels()),
        moved: refreshed
            .moved_from
            .zip(refreshed.pins)
            .filter(|(from, to)| from.head != to.head)
            .map(|(from, to)| MovedDto {
                from: from.head.to_string(),
                to: to.head.to_string(),
            }),
    };
    if global.json {
        output::json(ctx, &doc)?;
    } else {
        let mut lines = vec![format!("refreshed {}", format::review_line(&doc.review))];
        if let Some(moved) = &doc.moved {
            lines.push(format!(
                "the pull request moved: head {} -> {}",
                &moved.from[..7],
                &moved.to[..7]
            ));
        }
        output::text(ctx, &lines)?;
    }
    Ok(Exit::Ok)
}
