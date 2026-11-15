//! `dv review new`, `done`, `submit`, `close` and `delete` (docs/PLAN.md §10.1, §2.3, §2.4).
//!
//! Who may do what is `dv_core::review::ops`'s rule (agents hand a round back with `done`; the
//! reviewer submits, closes and deletes), except `delete`, which the store doesn't judge: only
//! the reviewer deletes a review.

use dv_core::review::{OpError, ReviewId, ReviewStatus, Verdict, ops};
use dv_core::target::Target;

use super::format;
use crate::args::{GlobalArgs, ReviewDoneArgs, ReviewNewArgs, ReviewSubmitArgs, VerdictArg};
use crate::body;
use crate::context::Context;
use crate::dto::{ReviewChangeDoc, ReviewDeleteDoc, ReviewNewDoc, ReviewSubmitDoc, RoundDto};
use crate::error::{CliError, Exit, code};
use crate::identity;
use crate::output;
use crate::pr::Written;
use crate::repo::Repo;
use crate::select;

/// `dv review new [--staged | --branch [<b>] [--base <base>] | --commit <rev> | --range
/// <a..b>]`: opens the target's review, or resumes its open one (PLAN §2.4: at most one open
/// review per target), as whoever the command acts as. The target must resolve now, so a
/// mistyped revision is an error rather than a review of nothing.
pub(crate) fn new(
    global: &GlobalArgs,
    args: &ReviewNewArgs,
    ctx: &mut Context,
) -> Result<Exit, CliError> {
    let repo = Repo::open(global, ctx)?;
    let author = identity::author(global, ctx, repo.backend.as_ref())?;
    let target = target(&repo, args)?;
    target.resolve(&repo.git)?;
    let opened = ops::open_review(&repo.store, target, author)?;
    let doc = ReviewNewDoc {
        created: opened.created,
        review: repo.review_dto(&opened.review, &repo.labels()),
        stale: None,
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

/// The target `dv review new`'s flags name (PLAN §2.2); the working tree when none is given.
/// `--branch` alone means the branch checked out, compared with `--base` or, without one, the
/// default branch, as the switcher's "Branch" row does.
fn target(repo: &Repo, args: &ReviewNewArgs) -> Result<Target, CliError> {
    if args.staged {
        return Ok(Target::Staged);
    }
    if let Some(branch) = &args.branch {
        let branch = match branch {
            Some(branch) => branch.clone(),
            None => repo.git.current_branch()?.ok_or_else(|| {
                CliError::usage("HEAD is detached: name the branch, --branch <name>")
            })?,
        };
        return Ok(Target::Branch {
            branch,
            base: args.base.clone(),
        });
    }
    if let Some(rev) = &args.commit {
        return Ok(Target::Commit { rev: rev.clone() });
    }
    if let Some(range) = &args.range {
        return Ok(Target::Range {
            from: range.from.clone(),
            to: range.to.clone(),
            three_dot: range.three_dot,
        });
    }
    Ok(Target::Working)
}

/// `dv review done [<id>] [--note <t> | --note-file <f|->] [--round <n>]`: an agent hands the
/// round back (status `ready`, PLAN §2.4, §3 workflow A step 4), with a note for the reviewer
/// the GUI shows. `--round <n>` names the round the agent worked on, which must be the last
/// one closed; without it, a round the reviewer finished while the agent was working is
/// `unseen_round` (exit 1) until the agent has read it and names it
/// (`dv_core::review::ops::hand_back`, which also reads the log back after writing).
pub(crate) fn done(
    global: &GlobalArgs,
    args: &ReviewDoneArgs,
    ctx: &mut Context,
) -> Result<Exit, CliError> {
    crate::pr::refuse_agent(global, ctx, Written::Review(args.review.id.as_deref()))?;
    let agent = identity::require_agent(
        global,
        ctx,
        "dv review done",
        "handoff",
        "the reviewer finishes a round with `dv review submit`",
    )?;
    let note = body::note(&args.note, ctx)?;
    let repo = Repo::open(global, ctx)?;
    let review = select::review(&repo, global, ctx, args.review.id.as_deref())?;
    let changed = ops::hand_back(&repo.store, &review.id, &agent, note.as_deref(), args.round)
        .map_err(|error| match error {
            OpError::UnseenRound { round } => unseen_round(&review.id, round),
            other => other.into(),
        })?;
    let review = select::load(&repo, &review.id)?;
    print_change(
        global,
        ctx,
        &repo,
        &review,
        changed,
        "handed back to the reviewer",
    )
}

/// The `unseen_round` error (exit 1): the reviewer finished `round` of `review` while the agent
/// was working, so what to do is read it and acknowledge it.
fn unseen_round(review: &ReviewId, round: u32) -> CliError {
    CliError::new(
        Exit::Failure,
        code::UNSEEN_ROUND,
        format!(
            "the reviewer finished round {round} while you were working, so the review wasn't \
             handed back: read round {round}'s summary (the `body` of the last of `rounds` in \
             `dv review show {review} --json`) and its open threads (`dv comment list --status \
             open`), act on them, then run `dv review done {review} --round {round}`"
        ),
    )
}

/// `dv review submit [<id>] --verdict approve|request-changes [--body | --body-file]`: the
/// reviewer finishes the round in progress of a local review (PLAN §2.3: snapshot, then
/// `round_closed` and the verdict's status). A local round approves or requests changes; a PR
/// review is submitted to GitHub, which arrives in a later version (PLAN §9.12: exit 2), and
/// is never an agent's to submit (`not_allowed`, as its every other write).
pub(crate) fn submit(
    global: &GlobalArgs,
    args: &ReviewSubmitArgs,
    ctx: &mut Context,
) -> Result<Exit, CliError> {
    crate::pr::refuse_agent(global, ctx, Written::Review(args.review.id.as_deref()))?;
    let body = body::optional(&args.body, ctx)?;
    let repo = Repo::open(global, ctx)?;
    let author = identity::author(global, ctx, repo.backend.as_ref())?;
    let review = select::review(&repo, global, ctx, args.review.id.as_deref())?;
    if review.is_pr_review() {
        return Err(super::later::pr_submit());
    }
    let verdict = match args.verdict {
        VerdictArg::Approve => Verdict::Approve,
        VerdictArg::RequestChanges => Verdict::RequestChanges,
        VerdictArg::Comment => {
            return Err(CliError::usage(
                "a local round approves or requests changes: --verdict approve or --verdict \
                 request-changes",
            ));
        }
    };
    // The round's snapshot is of the review's home worktree (PLAN §2.2, §2.3).
    let target_repo = repo.for_review(&review, ctx)?;
    let finished = ops::finish_round(
        &target_repo.store,
        &target_repo.git,
        &review.id,
        Some(verdict),
        body,
        &author,
    )?;
    let doc = ReviewSubmitDoc {
        review: repo.review_dto(&finished.review, &repo.labels()),
        round: RoundDto::from(&finished.round),
    };
    if global.json {
        output::json(ctx, &doc)?;
    } else {
        output::text(
            ctx,
            &[
                format!(
                    "round {} closed: {}",
                    doc.round.n,
                    doc.round.verdict.unwrap_or("no verdict")
                ),
                format::review_line(&doc.review),
            ],
        )?;
    }
    Ok(Exit::Ok)
}

/// `dv review close <id>`: the reviewer closes the review without approving it.
pub(crate) fn close(global: &GlobalArgs, id: &str, ctx: &mut Context) -> Result<Exit, CliError> {
    let repo = Repo::open(global, ctx)?;
    let author = identity::author(global, ctx, repo.backend.as_ref())?;
    let review = select::review(&repo, global, ctx, Some(id))?;
    let changed = ops::set_status(&repo.store, &review.id, ReviewStatus::Closed, &author, None)?;
    let review = select::load(&repo, &review.id)?;
    print_change(global, ctx, &repo, &review, changed, "closed")
}

/// `dv review delete <id>`: the reviewer deletes a review, its log, its snapshot cache and its
/// round snapshots (PLAN §7). Deleting again finishes a delete that was interrupted, and then
/// says the review isn't there.
pub(crate) fn delete(global: &GlobalArgs, id: &str, ctx: &mut Context) -> Result<Exit, CliError> {
    if let Some(agent) = identity::agent(global, ctx)? {
        return Err(CliError::new(
            Exit::Failure,
            code::NOT_ALLOWED,
            format!(
                "{} can't delete a review: only the reviewer deletes reviews",
                agent.kind.noun()
            ),
        ));
    }
    let id = select::named(global, ctx, Some(id))?
        .ok_or_else(|| CliError::usage("name the review to delete"))?;
    let repo = Repo::open(global, ctx)?;
    if !repo.store.delete(&id)? {
        return Err(CliError::not_found(format!(
            "no review {id} in {}",
            repo.root()
        )));
    }
    let doc = ReviewDeleteDoc {
        deleted: id.to_string(),
    };
    if global.json {
        output::json(ctx, &doc)?;
    } else {
        output::text(ctx, &[format!("deleted {}", doc.deleted)])?;
    }
    Ok(Exit::Ok)
}

/// Prints a status change: the review and whether anything was written.
fn print_change(
    global: &GlobalArgs,
    ctx: &mut Context,
    repo: &Repo,
    review: &dv_core::review::Review,
    changed: bool,
    what: &str,
) -> Result<Exit, CliError> {
    let doc = ReviewChangeDoc {
        changed,
        review: repo.review_dto(review, &repo.labels()),
    };
    if global.json {
        output::json(ctx, &doc)?;
    } else {
        let line = format::review_line(&doc.review);
        let lead = if changed {
            what.to_string()
        } else {
            format!("unchanged (already {})", doc.review.status)
        };
        output::text(ctx, &[format!("{lead}: {line}")])?;
    }
    Ok(Exit::Ok)
}
