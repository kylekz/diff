//! `dv review list` and `dv review show` (docs/PLAN.md §10.1).

use std::collections::HashSet;

use dv_core::review::{Review, ReviewStatus, ReviewSummary, StoreError};

use super::format;
use crate::args::{GlobalArgs, ReviewListArgs, StatusArg};
use crate::context::Context;
use crate::dto::{ReviewDto, ReviewListDoc, ReviewShowDoc};
use crate::error::{CliError, Exit};
use crate::output;
use crate::repo::Repo;
use crate::select::{self, Listing};

/// `dv review list [--all] [--status …]`: the open reviews, every review with `--all`, or
/// those with the statuses given, newest first; logs that don't load are listed apart.
pub(crate) fn list(
    global: &GlobalArgs,
    args: &ReviewListArgs,
    ctx: &mut Context,
) -> Result<Exit, CliError> {
    let repo = Repo::open(global, ctx)?;
    let labels = repo.labels();
    let listing = Listing::read(&repo)?;
    let statuses: HashSet<ReviewStatus> = args.status.iter().copied().map(status).collect();
    let wanted = |summary: &&ReviewSummary| {
        if !statuses.is_empty() {
            statuses.contains(&summary.status)
        } else {
            args.all || summary.status.is_open()
        }
    };
    let reviews = load_all(&repo, listing.summaries.iter().filter(wanted))?;
    let doc = ReviewListDoc {
        reviews: reviews
            .iter()
            .map(|review| repo.review_dto(review, &labels))
            .collect(),
        unreadable: listing.unreadable,
    };
    if global.json {
        output::json(ctx, &doc)?;
        return Ok(Exit::Ok);
    }
    let mut lines: Vec<String> = doc.reviews.iter().map(format::review_line).collect();
    if lines.is_empty() {
        lines.push(
            if args.all || !statuses.is_empty() {
                "no reviews"
            } else {
                "no open reviews"
            }
            .to_string(),
        );
    }
    lines.extend(
        doc.unreadable
            .iter()
            .map(|entry| format!("{}  unreadable: {}", entry.id, entry.error)),
    );
    output::text(ctx, &lines)?;
    Ok(Exit::Ok)
}

/// `dv review show [<id>]`: the review the command means (PLAN §10.2).
pub(crate) fn show(
    global: &GlobalArgs,
    id: Option<&str>,
    ctx: &mut Context,
) -> Result<Exit, CliError> {
    let repo = Repo::open(global, ctx)?;
    let review = select::review(&repo, global, ctx, id)?;
    let doc = ReviewShowDoc {
        review: repo.review_dto(&review, &repo.labels()),
    };
    if global.json {
        output::json(ctx, &doc)?;
    } else {
        output::text(ctx, &show_text(&doc.review))?;
    }
    Ok(Exit::Ok)
}

fn show_text(review: &ReviewDto) -> Vec<String> {
    let mut lines = vec![
        format!("{}  {}", review.id, review.target.label),
        format!("status: {}", review.status),
        format!("round: {}", review.round),
        format!("threads: {}", format::counts(&review.threads)),
        format!("worktree: {}", review.worktree),
    ];
    if let Some(pr) = &review.pr {
        lines.push(format::pr_line(pr));
    }
    lines.extend(review.submissions.iter().map(|submission| {
        format!(
            "submitted to GitHub: {}  {}",
            submission.event, submission.url
        )
    }));
    if let Some(note) = &review.status_note {
        let by = note
            .author
            .as_ref()
            .map(|author| format!(" from {} {}", author.kind, author.name))
            .unwrap_or_default();
        lines.push(format!(
            "note{by} ({}): {}",
            note.status,
            format::first_line(&note.note)
        ));
    }
    // Earlier rounds on a line each; the last with its summary in full, the one the agent acts
    // on.
    if let Some((last, earlier)) = review.rounds.split_last() {
        lines.extend(earlier.iter().map(format::round_line));
        lines.extend(format::round_summary(last));
    }
    lines
}

/// The reviews `summaries` lists, loaded; one deleted since it was listed is left out.
pub(crate) fn load_all<'s>(
    repo: &Repo,
    summaries: impl Iterator<Item = &'s ReviewSummary>,
) -> Result<Vec<Review>, CliError> {
    let mut reviews = Vec::new();
    for summary in summaries {
        match repo.store.load(&summary.id) {
            Ok(loaded) => reviews.push(loaded.review),
            Err(StoreError::UnknownReview { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(reviews)
}

fn status(arg: StatusArg) -> ReviewStatus {
    match arg {
        StatusArg::Open => ReviewStatus::Open,
        StatusArg::ChangesRequested => ReviewStatus::ChangesRequested,
        StatusArg::InProgress => ReviewStatus::InProgress,
        StatusArg::Ready => ReviewStatus::Ready,
        StatusArg::Approved => ReviewStatus::Approved,
        StatusArg::Closed => ReviewStatus::Closed,
        StatusArg::Draft => ReviewStatus::Draft,
        StatusArg::Submitted => ReviewStatus::Submitted,
    }
}
