//! `dv review wait [<id>] [--timeout <s>]` (docs/PLAN.md §10.1): blocks until the reviewer
//! finishes a round, so an agent never polls.
//!
//! It waits on the review's *state*, not on a change: it returns at once when the status is
//! one the agent acts on — `changes_requested` (its turn), `approved` or `closed` (finished;
//! `submitted` for a PR review) — and otherwise blocks until it becomes one. No wakeup is lost:
//! the review was loaded before the store watch is registered, so it is loaded again after,
//! and every batch of the watch that touches the review's log (or may have lost events) loads
//! it once more. A re-check every [`RECHECK`] covers a watch that misses something — due
//! whatever other batches arrive meanwhile ([`until_reload`]) — and a repository the OS can't
//! watch is polled every [`POLL`] instead. A review deleted meanwhile
//! is exit 1; `--timeout` running out is exit 3.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use dv_core::backend::{WatchEvent, WatchSpec};
use dv_core::review::{AuthorKind, Review, ReviewStatus, StoreError, ThreadState};

use super::format;
use crate::args::{GlobalArgs, ReviewWaitArgs};
use crate::context::Context;
use crate::dto::{RoundDto, ThreadCountsDto, WaitDoc, WaitTimeoutDoc};
use crate::error::{CliError, Exit};
use crate::output;
use crate::repo::Repo;
use crate::select;

/// How often a wait loads the review again although its watch reported nothing.
pub(crate) const RECHECK: Duration = Duration::from_secs(5);

/// How often a wait loads the review when the store can't be watched.
pub(crate) const POLL: Duration = Duration::from_secs(1);

/// Where a wait is, for tests that change the review at exactly that point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// The review was loaded and must be waited for; the watch isn't registered yet.
    BeforeWatch,
    /// The watch is registered; the review hasn't been loaded again yet.
    AfterWatch,
}

/// How a wait ended.
#[derive(Debug)]
pub(crate) enum Waited {
    /// The review reached a status the agent acts on.
    Woke(Review),
    /// The timeout ran out first; the review as last loaded.
    TimedOut(Review),
}

pub(crate) fn wait(
    global: &GlobalArgs,
    args: &ReviewWaitArgs,
    ctx: &mut Context,
) -> Result<Exit, CliError> {
    let repo = Repo::open(global, ctx)?;
    let review = select::review(&repo, global, ctx, args.review.id.as_deref())?;
    let waited = wait_for(&repo, review, args.timeout, &mut |_| {})?;
    let labels = repo.labels();
    match waited {
        Waited::Woke(review) => {
            let doc = WaitDoc {
                outcome: review.status.as_str(),
                review: repo.review_dto(&review, &labels),
                round: review.last_round().map(RoundDto::from),
                threads: ThreadCountsDto::of(&review),
            };
            if global.json {
                output::json(ctx, &doc)?;
            } else {
                output::text(ctx, &woke_text(&doc, waiting_questions(&review)))?;
            }
            Ok(Exit::Ok)
        }
        Waited::TimedOut(review) => {
            if global.json {
                output::json(ctx, &WaitTimeoutDoc { outcome: "timeout" })?;
            } else {
                output::text(
                    ctx,
                    &[format!(
                        "timed out: review {id} is still {}; run `dv review wait {id}` again to \
                         keep waiting",
                        review.status,
                        id = review.id,
                    )],
                )?;
            }
            Ok(Exit::Timeout)
        }
    }
}

/// Whether the agent acts on a review in `status`: its turn, or nothing more to do.
pub(crate) fn wakes(status: ReviewStatus) -> bool {
    matches!(
        status,
        ReviewStatus::ChangesRequested
            | ReviewStatus::Approved
            | ReviewStatus::Closed
            | ReviewStatus::Submitted
    )
}

/// Waits until `review` (as just loaded) reaches a status that [`wakes`] the agent, or
/// `timeout` runs out (module docs). `stage` runs at each [`Stage`], for tests.
pub(crate) fn wait_for(
    repo: &Repo,
    review: Review,
    timeout: Option<Duration>,
    stage: &mut dyn FnMut(Stage),
) -> Result<Waited, CliError> {
    if wakes(review.status) {
        return Ok(Waited::Woke(review));
    }
    let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
    let log = format!("reviews/{}.jsonl", review.id);
    stage(Stage::BeforeWatch);
    let (sender, events) = mpsc::channel::<WatchEvent>();
    let watch = repo
        .backend
        .watch(
            WatchSpec::STORE,
            Box::new(move |event| {
                // The receiver is gone once the wait returns.
                let _ = sender.send(event);
            }),
        )
        .map_err(|error| {
            log::warn!(
                "review {}: can't watch the review store ({error}); checking every {}s",
                review.id,
                POLL.as_secs()
            );
        })
        .ok();
    let mut watching = watch.is_some();
    stage(Stage::AfterWatch);

    let mut review = review;
    loop {
        review = reload(repo, &review)?;
        if wakes(review.status) {
            return Ok(Waited::Woke(review));
        }
        let next_check = Instant::now() + if watching { RECHECK } else { POLL };
        match until_reload(watching.then_some(&events), &log, deadline, next_check) {
            Until::Reload => {}
            Until::Timeout => return Ok(Waited::TimedOut(review)),
            Until::Unwatched => {
                log::warn!(
                    "review {}: the store watch stopped; checking every {}s",
                    review.id,
                    POLL.as_secs()
                );
                watching = false;
            }
        }
    }
}

/// Why [`until_reload`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Until {
    /// The log changed, the watch may have lost events, or the re-check is due: load the
    /// review again.
    Reload,
    /// The deadline passed.
    Timeout,
    /// The watch stopped; poll from now on.
    Unwatched,
}

/// Blocks until a batch of `events` touches `log` (or is a rescan), `next_check` comes (the
/// re-check, which batches about anything else never put off) or `deadline` passes. Without a
/// watch it sleeps until `next_check`.
pub(crate) fn until_reload(
    events: Option<&mpsc::Receiver<WatchEvent>>,
    log: &str,
    deadline: Option<Instant>,
    next_check: Instant,
) -> Until {
    loop {
        let now = Instant::now();
        if deadline.is_some_and(|deadline| deadline <= now) {
            return Until::Timeout;
        }
        if next_check <= now {
            return Until::Reload;
        }
        let wake = deadline.map_or(next_check, |deadline| deadline.min(next_check));
        let Some(events) = events else {
            std::thread::sleep(wake - now);
            continue;
        };
        match events.recv_timeout(wake - now) {
            Ok(event) if event.rescan || event.store_paths.iter().any(|path| path == log) => {
                return Until::Reload;
            }
            // Another review's log, a snapshot cache, a temp index: wait on.
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Until::Unwatched,
        }
    }
}

/// `review` loaded again; deleted meanwhile, it is `not_found` (exit 1).
fn reload(repo: &Repo, review: &Review) -> Result<Review, CliError> {
    match repo.store.load(&review.id) {
        Ok(loaded) => Ok(loaded.review),
        Err(StoreError::UnknownReview { .. }) => Err(CliError::not_found(format!(
            "review {} was deleted while waiting for it",
            review.id
        ))),
        Err(error) => Err(error.into()),
    }
}

/// The open threads an agent opened (`dv comment add`: a question for the reviewer) that the
/// reviewer hasn't replied to: they wait on the reviewer, so the hint doesn't count them among
/// the threads to fix and address (PLAN §10.5).
fn waiting_questions(review: &Review) -> u32 {
    let count = review
        .live_threads()
        .filter(|thread| thread.state == ThreadState::Open)
        .filter(|thread| thread.author.kind == AuthorKind::Agent)
        .filter(|thread| {
            !thread
                .replies
                .iter()
                .any(|reply| !reply.is_deleted() && reply.author.kind == AuthorKind::Reviewer)
        })
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// What a woken wait says: the review's line, the last closed round with its summary in full,
/// and what to do next; `questions` of the open threads are agents' questions still waiting for
/// the reviewer ([`waiting_questions`]).
fn woke_text(doc: &WaitDoc, questions: u32) -> Vec<String> {
    let review = &doc.review;
    let mut lines = vec![format::review_line(review)];
    if let Some(round) = &doc.round {
        lines.extend(format::round_summary(round));
    }
    let summary = doc.round.as_ref().is_some_and(|round| round.body.is_some());
    // The hand-back names the review and the round worked on, as the skill's step 4 does.
    let done = match &doc.round {
        Some(round) => format!("`dv review done {} --round {}`", review.id, round.n),
        None => format!("`dv review done {}`", review.id),
    };
    lines.push(match review.status {
        "changes_requested" => {
            let open = doc.threads.open.saturating_sub(questions);
            let threads = if open == 1 { "thread" } else { "threads" };
            let hint = match (open, summary) {
                // A reviewer may request changes in the round's summary alone (PLAN §10.5).
                (0, true) => format!(
                    "changes requested, 0 open threads: do what the round's summary asks, then \
                     {done}"
                ),
                (0, false) => format!(
                    "changes requested, 0 open threads: read the reviewer's replies (`dv \
                     comment list`), act on them, then {done}"
                ),
                (_, true) => format!(
                    "changes requested, {open} open {threads}: do what the round's summary \
                     asks, run `dv comment list --status open`, fix, reply and address each, \
                     then {done}"
                ),
                (_, false) => format!(
                    "changes requested, {open} open {threads}: run `dv comment list --status \
                     open`, fix, reply and address each, then {done}"
                ),
            };
            match questions {
                0 => hint,
                1 => format!(
                    "{hint}; 1 more open thread is an agent's question still waiting for the \
                     reviewer: leave it open"
                ),
                n => format!(
                    "{hint}; {n} more open threads are agents' questions still waiting for the \
                     reviewer: leave them open"
                ),
            }
        }
        "approved" => "approved: nothing more to address".to_string(),
        "closed" => "closed: the reviewer closed the review".to_string(),
        other => format!("{other}: the review is finished"),
    });
    lines
}
