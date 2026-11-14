//! Checked review operations (docs/PLAN.md §2.4, §2.5): each loads the review, checks the
//! change against its current state and who is asking, and only then appends the event.
//!
//! The checks the log can carry are the fold's own ([`super::fold`]), run on the loaded state
//! before anything is written, so an operation never appends an event the fold would skip given
//! that state. On top of them come the checks that need to know who is asking, which some
//! events don't record (`body_edited`, `deleted`) or record only optionally (`status`): agents
//! set only `in_progress` and `ready`, the reviewer the verdict statuses, and only an item's
//! author edits or deletes it. A finished local review (`approved`, `closed`) takes no more
//! threads, replies, state changes, edits or viewed marks ([`OpError::Finished`]), as the GUI
//! shows it read-only (PLAN §2.4); a submitted PR review refuses every later event in the fold
//! itself.
//!
//! Writers don't lock (PLAN D5), so another writer may append between the load and the append;
//! the fold then applies both in file order under the same rules, and an event that no longer
//! fits is skipped there with a warning (module docs of [`super::fold`]).
//!
//! An operation's load uses a fresh snapshot cache but never writes one
//! (`ReviewStore::load_for_update`): its own append would make it stale at once (PLAN §7).

use super::event::{Event, EventBody};
use super::fold::{Folder, Outcome, Rejection, fold};
use super::home::{HomeError, Worktrees};
use super::id::{CommentId, ItemId, ReviewId, ThreadId};
use super::model::{
    Anchor, Author, AuthorKind, PrLink, Review, ReviewStatus, ReviewSummary, Round, ThreadState,
    Verdict,
};
use super::snapshot::{self, SnapshotError};
use super::store::{ListEntry, ReviewStore, StoreError};
use crate::git::{GitError, GitRepo, Oid};
use crate::target::{Side, Target, TargetError};

/// Why an operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The change doesn't apply to the review as it stands.
    #[error(transparent)]
    Rejected(#[from] Rejection),
    /// The change is valid, but not for this author (PLAN §2.4, §2.5).
    #[error("{} can't {action}", actor.noun())]
    NotAllowed { actor: AuthorKind, action: String },
    #[error("the {what} is empty")]
    Empty { what: &'static str },
    /// The review is `approved`, `closed` or `submitted`: nothing more happens to it.
    #[error("the review is {status}")]
    Finished { status: ReviewStatus },
    /// An agent's hand-back ([`hand_back`]) would hand back a round the reviewer finished
    /// while the agent was still working on the one before (PLAN §2.4): it reads round
    /// `round` first, then acknowledges it.
    #[error("the reviewer finished round {round} while the agent was working")]
    UnseenRound { round: u32 },
    /// A hand-back acknowledged a round that isn't the last closed one: `closed` rounds have
    /// closed.
    #[error("round {round} isn't the last closed round ({})", match closed {
        0 => "no round has closed yet".to_string(),
        n => format!("round {n} is"),
    })]
    NotLastRound { round: u32, closed: u32 },
    /// The review's target no longer resolves (a branch that was deleted, say).
    #[error(transparent)]
    Target(#[from] TargetError),
    #[error("couldn't snapshot the round: {0}")]
    Snapshot(#[from] SnapshotError),
    #[error(transparent)]
    Git(#[from] GitError),
}

/// What [`finish_round`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedRound {
    /// The round that closed: its number, its two sides, its verdict and summary.
    pub round: Round,
    /// The review after the round closed and its status followed the verdict.
    pub review: Review,
}

/// What [`open_review`] found or made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub review: Review,
    /// `false` when an open review of the target already existed and is returned instead.
    pub created: bool,
}

/// Opens the review of `target` in the store's worktree: the open review of that target when
/// there is one ("at most one open review per target", PLAN §2.4), else a new one by `author`.
///
/// A `working` or `staged` target belongs to its worktree, so another worktree's review of
/// one doesn't count; the other targets name the same commits from every worktree. A PR target
/// matches by pull request, whatever head it was fetched at. When several open reviews match
/// (two openers raced), the most recently updated one wins. Logs that fail to load are passed
/// over.
pub fn open_review(store: &ReviewStore, target: Target, author: Author) -> Result<Opened, OpError> {
    open_review_as(store, target, || author)
}

/// [`open_review`], asking `author` who opens it only when it creates the review: resuming one
/// then reads nothing more (the app's open, where who the reviewer is costs a git call).
pub fn open_review_as(
    store: &ReviewStore,
    target: Target,
    author: impl FnOnce() -> Author,
) -> Result<Opened, OpError> {
    let worktree = store.worktree();
    let existing = store
        .list()?
        .into_iter()
        .filter_map(|entry| match entry {
            ListEntry::Ok(summary) => Some(*summary),
            ListEntry::Err { .. } => None,
        })
        .filter(|summary| summary.status.is_open() && is_review_of(summary, &target, worktree))
        .max_by_key(|summary| summary.updated_at);
    if let Some(summary) = existing {
        let review = store.load(&summary.id)?.review;
        return Ok(Opened {
            review,
            created: false,
        });
    }
    Ok(Opened {
        review: store.create(target, author())?,
        created: true,
    })
}

/// Adds a thread anchored at `anchor` with its opening comment; returns its id.
pub fn add_thread(
    store: &ReviewStore,
    review: &ReviewId,
    anchor: Anchor,
    body: &str,
    author: Author,
) -> Result<ThreadId, OpError> {
    check_body(body, "comment")?;
    let id = ThreadId::generate();
    commit(
        store,
        review,
        EventBody::ThreadAdded {
            id: id.clone(),
            anchor,
            body: body.to_string(),
            author,
            annotation: false,
        },
        check_not_finished,
    )?;
    Ok(id)
}

/// Adds an agent's annotation anchored at `anchor` (PLAN §19): a thread in state
/// `annotation`, waiting on no one; returns its id. Only an agent annotates, and never a PR
/// review, which is the reviewer's (PLAN §2.4).
pub fn annotate(
    store: &ReviewStore,
    review: &ReviewId,
    anchor: Anchor,
    body: &str,
    author: Author,
) -> Result<ThreadId, OpError> {
    if author.kind != AuthorKind::Agent {
        return Err(OpError::NotAllowed {
            actor: author.kind,
            action: "annotate a change: annotations are an agent's".to_string(),
        });
    }
    check_body(body, "annotation")?;
    let id = ThreadId::generate();
    let actor = author.kind;
    commit(
        store,
        review,
        EventBody::ThreadAdded {
            id: id.clone(),
            anchor,
            body: body.to_string(),
            author,
            annotation: true,
        },
        |state| {
            check_not_finished(state)?;
            if state.is_pr_review() {
                return Err(OpError::NotAllowed {
                    actor,
                    action: "annotate a PR review: PR reviews are the reviewer's".to_string(),
                });
            }
            Ok(())
        },
    )?;
    Ok(id)
}

/// Replies to a thread; returns the reply's id. `relocated`: the thread's anchor where
/// relocation found its text now, when it moved or its file was renamed
/// (`super::relocate::relocated_anchor`), which the reply records (PLAN §8); `None` otherwise,
/// or when nobody relocated it.
pub fn reply(
    store: &ReviewStore,
    review: &ReviewId,
    thread: &ThreadId,
    body: &str,
    author: Author,
    relocated: Option<Anchor>,
) -> Result<CommentId, OpError> {
    check_body(body, "reply")?;
    let id = CommentId::generate();
    commit(
        store,
        review,
        EventBody::ReplyAdded {
            thread: thread.clone(),
            id: id.clone(),
            body: body.to_string(),
            author,
            anchor: relocated,
        },
        check_not_finished,
    )?;
    Ok(id)
}

/// Moves a thread to `state` with an optional note (PLAN §2.5: agents address, only the
/// reviewer resolves and reopens; [`super::fold::check_thread_state`]), recording `relocated`
/// as [`reply`] does. `false` when the thread was already in that state and nothing was
/// written (the relocated anchor included).
///
/// An annotation is [`Rejection::Annotation`] first, whoever asks (PLAN §19.1: its state
/// doesn't change, a reply asks about it). Otherwise an agent asking to resolve or reopen is
/// [`OpError::NotAllowed`] before the thread's state is looked at, so it is told that those are
/// the reviewer's, whatever state the thread is in.
pub fn set_thread_state(
    store: &ReviewStore,
    review: &ReviewId,
    thread: &ThreadId,
    state: ThreadState,
    note: Option<&str>,
    author: Author,
    relocated: Option<Anchor>,
) -> Result<bool, OpError> {
    let actor = author.kind;
    let note = note.map(str::trim).filter(|note| !note.is_empty());
    let outcome = commit(
        store,
        review,
        EventBody::ThreadState {
            thread: thread.clone(),
            state,
            note: note.map(str::to_string),
            author,
            anchor: relocated,
        },
        |review| {
            if review
                .thread(thread)
                .is_some_and(|found| found.state == ThreadState::Annotation)
            {
                return Err(Rejection::Annotation {
                    thread: thread.clone(),
                }
                .into());
            }
            if actor == AuthorKind::Agent {
                let action = match state {
                    ThreadState::Resolved => {
                        Some("resolve a thread: only the reviewer resolves threads")
                    }
                    ThreadState::Open => Some("reopen a thread: only the reviewer reopens threads"),
                    ThreadState::Addressed | ThreadState::Annotation => None,
                };
                if let Some(action) = action {
                    return Err(OpError::NotAllowed {
                        actor,
                        action: action.to_string(),
                    });
                }
            }
            check_not_finished(review)
        },
    )?;
    Ok(outcome == Outcome::Changed)
}

/// Marks `path` viewed (`on`) or not viewed in the round in progress. `false` when it already
/// was and nothing was written.
pub fn mark_viewed(
    store: &ReviewStore,
    review: &ReviewId,
    path: &str,
    on: bool,
) -> Result<bool, OpError> {
    let loaded = store.load_for_update(review)?;
    check_not_finished(&loaded.review)?;
    let round = loaded.review.current_round();
    let outcome = apply_and_append(
        store,
        review,
        loaded.review,
        EventBody::Viewed {
            round,
            path: path.to_string(),
            on,
        },
    )?;
    Ok(outcome == Outcome::Changed)
}

/// Sets the review's status on behalf of `author` with an optional `note` (Markdown, PLAN
/// §2.4): an agent sets `in_progress` and `ready` (`dv review done --note`, which is
/// [`hand_back`]); the reviewer sets `changes_requested`, `approved` and `closed`; GitHub sets
/// nothing. The move itself must be allowed ([`super::fold::check_status`]); a finished local
/// review (`approved`, `closed`) moving anywhere else is [`OpError::Finished`], as every other
/// write to it is, so an agent's `dv review done` racing the reviewer's approval hears the
/// review is finished (a submitted PR review is the fold's, [`Rejection::Submitted`]).
/// `false` when nothing was written: the review already had that status, and the note (if
/// any) is the one it already carries.
pub fn set_status(
    store: &ReviewStore,
    review: &ReviewId,
    status: ReviewStatus,
    author: &Author,
    note: Option<&str>,
) -> Result<bool, OpError> {
    use ReviewStatus::{Approved, ChangesRequested, Closed, InProgress, Ready};
    let allowed = match author.kind {
        AuthorKind::Agent => matches!(status, InProgress | Ready),
        AuthorKind::Reviewer => matches!(status, ChangesRequested | Approved | Closed),
        AuthorKind::Github => false,
    };
    if !allowed {
        return Err(OpError::NotAllowed {
            actor: author.kind,
            action: format!("set a review's status to {status}"),
        });
    }
    if status == Ready {
        return hand_back(store, review, author, note, None);
    }
    let note = note.map(str::trim).filter(|note| !note.is_empty());
    let outcome = commit(
        store,
        review,
        EventBody::Status {
            status,
            note: note.map(str::to_string),
            author: Some(author.clone()),
            if_status: None,
            if_round: None,
        },
        |state| match state.status {
            from if from != status => check_not_finished(state),
            _ => Ok(()),
        },
    )?;
    Ok(outcome == Outcome::Changed)
}

/// An agent hands the review back (`dv review done`, PLAN §2.4, §3 workflow A step 4): status
/// `ready`, with an optional `note` (Markdown) for the reviewer.
///
/// It never hands back a round the agent hasn't seen. `seen_round` is the round the agent
/// worked on (`dv review done --round <n>`: the round `dv review wait` returned with), which
/// must be the last closed one: an older one is [`OpError::UnseenRound`] for the newest, one
/// that hasn't closed [`OpError::NotLastRound`]. Without it, a round the reviewer finished
/// while the agent was still `in_progress` on the one before ([`Review::unseen_round`]) is
/// [`OpError::UnseenRound`]: the agent's `done` would carry a note about the old round and
/// leave the new one unread, and nothing in the log tells whether its writes since were about
/// the new round. The event written is conditional on the closed rounds it was checked
/// against (`if_round`), so a round that closes between this load and the append wins: the log
/// is read again afterwards, and a hand-back the fold skipped there is [`OpError::Finished`]
/// when the round finished the review, else [`OpError::UnseenRound`]. A finished review is
/// [`OpError::Finished`], as for [`set_status`]. `false` when nothing was written (already
/// `ready` with that note).
pub fn hand_back(
    store: &ReviewStore,
    review: &ReviewId,
    author: &Author,
    note: Option<&str>,
    seen_round: Option<u32>,
) -> Result<bool, OpError> {
    if author.kind != AuthorKind::Agent {
        return Err(OpError::NotAllowed {
            actor: author.kind,
            action: format!("set a review's status to {}", ReviewStatus::Ready),
        });
    }
    let loaded = store.load_for_update(review)?;
    hand_back_on(store, review, loaded.review, author, note, seen_round)
}

/// [`hand_back`]'s work on `state`, the review as it was read (a test lands a round between
/// the two).
pub(crate) fn hand_back_on(
    store: &ReviewStore,
    review: &ReviewId,
    state: Review,
    author: &Author,
    note: Option<&str>,
    seen_round: Option<u32>,
) -> Result<bool, OpError> {
    if state.status != ReviewStatus::Ready {
        check_not_finished(&state)?;
    }
    let closed = state.closed_rounds();
    match seen_round {
        Some(round) if round < closed => return Err(OpError::UnseenRound { round: closed }),
        Some(round) if round != closed => return Err(OpError::NotLastRound { round, closed }),
        Some(_) => {}
        None => {
            if let Some(round) = state.unseen_round() {
                return Err(OpError::UnseenRound { round });
            }
        }
    }
    let note = note.map(str::trim).filter(|note| !note.is_empty());
    let event = Event::new(EventBody::Status {
        status: ReviewStatus::Ready,
        note: note.map(str::to_string),
        author: Some(author.clone()),
        if_status: None,
        if_round: Some(closed),
    });
    if Folder::from_review(state).apply(&event)? == Outcome::Unchanged {
        return Ok(false);
    }
    store.append_event(review, &event)?;
    // Read back: a round that closed between the load and the append makes the event a no-op
    // where it landed, and a verdict that finished the review meanwhile refuses it.
    let events = store.events(review)?.events;
    let at = events
        .iter()
        .position(|logged| logged.eid == event.eid)
        .unwrap_or(events.len());
    let Ok(before) = fold(review, &events[..at]) else {
        return Ok(true);
    };
    // A verdict that finished the review is the stop signal an agent must hear, before any
    // round it missed (PLAN §2.4, §10.1).
    check_not_finished(&before.review)?;
    if before.review.closed_rounds() != closed {
        let now = fold(review, &events).map_or(before.review.closed_rounds(), |folded| {
            folded.review.closed_rounds()
        });
        return Err(OpError::UnseenRound { round: now });
    }
    Ok(true)
}

/// Moves a `changes_requested` review to `in_progress` for `author`, an agent: PLAN §2.4's
/// follow-up to an agent's first write after changes were requested, so the reviewer sees the
/// agent at work. Nothing is written unless the review is `changes_requested` as it stands now
/// (not as the caller last loaded it), and the event written is conditional on it
/// (`if_status`): a status that lands between this load and the append — the agent's own `dv
/// review done` run beside the write, the reviewer's verdict — wins, the fold leaving the review
/// as that status put it. `false` when nothing was written.
pub fn start_work(
    store: &ReviewStore,
    review: &ReviewId,
    author: &Author,
) -> Result<bool, OpError> {
    if author.kind != AuthorKind::Agent {
        return Ok(false);
    }
    let loaded = store.load_for_update(review)?;
    start_work_on(store, review, loaded.review, author)
}

/// [`start_work`]'s write on `loaded`, the review as it was read (a test interleaves another
/// write between the two).
pub(crate) fn start_work_on(
    store: &ReviewStore,
    review: &ReviewId,
    loaded: Review,
    author: &Author,
) -> Result<bool, OpError> {
    if loaded.status != ReviewStatus::ChangesRequested {
        return Ok(false);
    }
    apply_and_append(
        store,
        review,
        loaded,
        EventBody::Status {
            status: ReviewStatus::InProgress,
            note: None,
            author: Some(author.clone()),
            if_status: Some(ReviewStatus::ChangesRequested),
            if_round: None,
        },
    )?;
    Ok(true)
}

/// Links the review to a pull request (a PR target is already linked to its own). `false` when
/// it already was.
pub fn link_pr(store: &ReviewStore, review: &ReviewId, pr: PrLink) -> Result<bool, OpError> {
    let outcome = commit(
        store,
        review,
        EventBody::PrLinked {
            owner: pr.owner,
            repo: pr.repo,
            number: pr.number,
        },
        no_extra_check,
    )?;
    Ok(outcome == Outcome::Changed)
}

/// Records that a draft PR review was posted to GitHub (PLAN §9.3). The review is read-only
/// from then on.
pub fn record_submission(
    store: &ReviewStore,
    review: &ReviewId,
    github_review_id: u64,
    url: &str,
    event: Verdict,
) -> Result<(), OpError> {
    commit(
        store,
        review,
        EventBody::Submitted {
            github_review_id,
            url: url.to_string(),
            event,
        },
        no_extra_check,
    )?;
    Ok(())
}

/// Closes the round in progress at these two sides (PLAN §2.3); returns its number. Taking the
/// snapshot the new side names is the caller's job.
pub fn close_round(
    store: &ReviewStore,
    review: &ReviewId,
    old_oid: Oid,
    new_oid: Oid,
    verdict: Option<Verdict>,
) -> Result<u32, OpError> {
    let loaded = store.load_for_update(review)?;
    let n = loaded.review.current_round();
    apply_and_append(
        store,
        review,
        loaded.review,
        EventBody::RoundClosed {
            n,
            new_oid,
            old_oid,
            verdict,
            body: None,
        },
    )?;
    Ok(n)
}

/// Finishes the round in progress of a local review (PLAN §2.3, §3 workflow A step 3; the
/// "Finish round" sheet, DESIGN §4.2): resolves the target as it stands now, snapshots its new
/// side at `refs/dv/reviews/<id>/<n>` ([`snapshot::take`]), appends `round_closed` with the
/// resolved old side, the snapshot, the verdict and the summary `body`, then moves the status
/// by the verdict: `request_changes` to `changes_requested`, `approve` to `approved`, none
/// leaves it as it is.
///
/// Only the reviewer finishes a round, never of a finished review and never of a PR review,
/// whose rounds close when it is submitted to GitHub (PLAN §2.3, §9.3); a local round's verdict
/// approves or requests changes, never `comment`. Both events are checked before either is
/// written, and the snapshot is taken before both, so a failed snapshot leaves the log as it
/// was. Retrying after a failed append finds the round's ref already there: it is reused when
/// the content is unchanged, and replaced otherwise, also when it holds the commit an earlier
/// round closed at (a branch that hadn't moved), since the round it belongs to never closed
/// ([`snapshot::take`]).
///
/// The verdict's status event is written even when the review already had that status as it
/// was loaded: the snapshot takes a while, and a status that lands meanwhile (the agent's `dv
/// review done`) would otherwise stand after the round closed, the verdict lost. Written
/// after it, the verdict wins; with nothing between, the fold reads it as the no-op it is.
pub fn finish_round(
    store: &ReviewStore,
    repo: &GitRepo,
    review: &ReviewId,
    verdict: Option<Verdict>,
    body: Option<String>,
    author: &Author,
) -> Result<FinishedRound, OpError> {
    let not_allowed = |action: &str| OpError::NotAllowed {
        actor: author.kind,
        action: action.to_string(),
    };
    if author.kind != AuthorKind::Reviewer {
        return Err(not_allowed("finish a round"));
    }
    if verdict == Some(Verdict::Comment) {
        return Err(not_allowed(
            "finish a local round with a comment verdict (approve or request changes)",
        ));
    }
    let loaded = store.load_for_update(review)?;
    finish_round_on(store, repo, review, loaded.review, verdict, body, author)
}

/// [`finish_round`]'s work on `state`, the review as it was read (a test lands another write
/// between the two), once the author and the verdict are known to be allowed.
pub(crate) fn finish_round_on(
    store: &ReviewStore,
    repo: &GitRepo,
    review: &ReviewId,
    state: Review,
    verdict: Option<Verdict>,
    body: Option<String>,
    author: &Author,
) -> Result<FinishedRound, OpError> {
    let not_allowed = |action: &str| OpError::NotAllowed {
        actor: author.kind,
        action: action.to_string(),
    };
    if state.status.is_final() {
        return Err(OpError::Finished {
            status: state.status,
        });
    }
    if state.is_pr_review() {
        return Err(not_allowed(
            "finish a PR review's round here (it closes when submitted to GitHub)",
        ));
    }

    let spec = state.target.resolve(repo)?;
    let old_oid = match &spec.old {
        Side::Commit(oid) => oid.clone(),
        Side::EmptyTree => repo.empty_tree(),
        // Targets resolve their old side to a tree; the snapshot refuses anything else.
        Side::Index | Side::Worktree => {
            return Err(SnapshotError::UnsupportedSide {
                side: spec.old.to_string(),
            }
            .into());
        }
    };
    // The round in progress never closed, so its ref holds at most the snapshot of an attempt
    // whose events didn't land.
    let n = state.current_round();
    let new_oid = snapshot::take(repo, review, n, &spec, None)?;

    let body = body
        .map(|body| body.trim().to_string())
        .filter(|body| !body.is_empty());
    let round_closed = Event::new(EventBody::RoundClosed {
        n,
        new_oid: new_oid.clone(),
        old_oid,
        verdict,
        body,
    });
    let status = match verdict {
        Some(Verdict::RequestChanges) => Some(ReviewStatus::ChangesRequested),
        Some(Verdict::Approve) => Some(ReviewStatus::Approved),
        Some(Verdict::Comment) | None => None,
    };
    let status = status.map(|status| {
        Event::new(EventBody::Status {
            status,
            note: None,
            author: Some(author.clone()),
            if_status: None,
            if_round: None,
        })
    });
    let mut folder = Folder::from_review(state);
    folder.apply(&round_closed)?;
    // Checked like the round, and written whatever it changes in the state read (above).
    if let Some(event) = &status {
        folder.apply(event)?;
    }
    if let Err(error) = store.append_event(review, &round_closed) {
        // Deleted while the snapshot was taken: the delete may have listed the review's refs
        // before this one existed, so it goes here, or it would pin the snapshot for good.
        if matches!(error, StoreError::UnknownReview { .. }) {
            snapshot::remove_ref(repo, review, n, &new_oid);
        }
        return Err(error.into());
    }
    if let Some(event) = &status {
        store.append_event(review, event)?;
    }
    let review = folder.into_review();
    let round = review
        .rounds
        .last()
        .cloned()
        .expect("applying round_closed appended the round");
    Ok(FinishedRound { round, review })
}

/// Replaces a thread's opening comment or a reply. Only its author may.
pub fn edit_body(
    store: &ReviewStore,
    review: &ReviewId,
    item: &ItemId,
    body: &str,
    editor: &Author,
) -> Result<(), OpError> {
    check_body(body, "comment")?;
    commit(
        store,
        review,
        EventBody::BodyEdited {
            target_id: item.clone(),
            body: body.to_string(),
        },
        |state| {
            check_not_finished(state)?;
            check_item_author(state, item, editor, "edit")
        },
    )?;
    Ok(())
}

/// Deletes a thread or a reply (a tombstone). Only its author may.
pub fn delete_item(
    store: &ReviewStore,
    review: &ReviewId,
    item: &ItemId,
    by: &Author,
) -> Result<(), OpError> {
    commit(
        store,
        review,
        EventBody::Deleted {
            target_id: item.clone(),
        },
        |state| {
            check_not_finished(state)?;
            check_item_author(state, item, by, "delete")
        },
    )?;
    Ok(())
}

/// Loads the review, runs `extra` on it, then checks and appends `body`.
fn commit(
    store: &ReviewStore,
    review: &ReviewId,
    body: EventBody,
    extra: impl FnOnce(&Review) -> Result<(), OpError>,
) -> Result<Outcome, OpError> {
    let loaded = store.load_for_update(review)?;
    extra(&loaded.review)?;
    apply_and_append(store, review, loaded.review, body)
}

/// Checks `body` against `state` with the fold's own rules and appends it when it changes
/// something.
fn apply_and_append(
    store: &ReviewStore,
    review: &ReviewId,
    state: Review,
    body: EventBody,
) -> Result<Outcome, OpError> {
    let event = Event::new(body);
    let outcome = Folder::from_review(state).apply(&event)?;
    if outcome == Outcome::Changed {
        store.append_event(review, &event)?;
    }
    Ok(outcome)
}

fn no_extra_check(_: &Review) -> Result<(), OpError> {
    Ok(())
}

/// A finished local review (`approved`, `closed`) takes no more writes on its threads or its
/// files (module docs). A submitted PR review is left to the fold, which refuses every later
/// event (`Rejection::Submitted`).
fn check_not_finished(state: &Review) -> Result<(), OpError> {
    match state.status {
        ReviewStatus::Approved | ReviewStatus::Closed => Err(OpError::Finished {
            status: state.status,
        }),
        _ => Ok(()),
    }
}

/// Only an item's author edits or deletes it; GitHub's items are read-only (PLAN §9.2). An item
/// that doesn't exist is left to the fold's check, which says so.
fn check_item_author(
    state: &Review,
    item: &ItemId,
    actor: &Author,
    verb: &str,
) -> Result<(), OpError> {
    match state.item_author(item) {
        Some(author) if !author.is(actor) => Err(OpError::NotAllowed {
            actor: actor.kind,
            action: format!("{verb} {item}, written by {} {}", author.kind, author.name),
        }),
        _ => Ok(()),
    }
}

fn check_body(body: &str, what: &'static str) -> Result<(), OpError> {
    if body.trim().is_empty() {
        Err(OpError::Empty { what })
    } else {
        Ok(())
    }
}

/// The open reviews a command run in `repo`'s worktree means when it names none (PLAN §10.2
/// step 3), newest first: the ones whose home is this worktree ([`in_context`]). Logs that
/// fail to load are passed over. A command acts on the review when exactly one comes back.
pub fn reviews_in_context(
    store: &ReviewStore,
    repo: &GitRepo,
) -> Result<Vec<ReviewSummary>, OpError> {
    let open: Vec<ReviewSummary> = store
        .list()?
        .into_iter()
        .filter_map(|entry| match entry {
            ListEntry::Ok(summary) => Some(*summary),
            ListEntry::Err { .. } => None,
        })
        .filter(|summary| summary.status.is_open())
        .collect();
    Ok(in_context(open.iter(), repo)?
        .into_iter()
        .cloned()
        .collect())
}

/// Those of `summaries` (open or not) that are the work in progress of `repo`'s worktree: the
/// reviews whose home is this worktree (`super::home`, PLAN §2.2) — a `working` or `staged`
/// review made here, or a `branch` review of the branch checked out here, however its name is
/// spelled (`feat`, `heads/feat`, `refs/heads/feat`). A review whose worktree is gone is
/// nobody's context. The rule the GUI and the CLI resolve a review's target by, so a review in
/// context here is resolved here. Blocking: `git worktree list` and a ref lookup per `branch`
/// review.
pub fn in_context<'s>(
    summaries: impl IntoIterator<Item = &'s ReviewSummary>,
    repo: &GitRepo,
) -> Result<Vec<&'s ReviewSummary>, OpError> {
    let worktrees = Worktrees::read(repo)?;
    let (location, here) = (&repo.info().location, &repo.info().toplevel);
    let mut found = Vec::new();
    for summary in summaries {
        match worktrees.home(repo, &summary.target, &summary.worktree) {
            Ok(home) if home.is(location, here) => found.push(summary),
            Ok(_) | Err(HomeError::Gone { .. }) => {}
            Err(HomeError::Git(error)) => return Err(error.into()),
        }
    }
    Ok(found)
}

/// Whether `summary` reviews `target` as opened from `worktree` (the worktree root, as
/// `ReviewStore::worktree` names it), open or not: the rule [`open_review`] resumes by, which
/// the switcher also uses to show a target's open review beside it.
pub fn is_review_of(summary: &ReviewSummary, target: &Target, worktree: &str) -> bool {
    same_target(&summary.target, target)
        && (!is_worktree_bound(target) || summary.worktree == worktree)
}

/// Whether two targets are the same for "at most one open review per target": the same spec,
/// except that a PR is the same PR whatever commits it was fetched at.
fn same_target(a: &Target, b: &Target) -> bool {
    match (a, b) {
        (
            Target::Pr {
                owner: owner_a,
                repo: repo_a,
                number: number_a,
                ..
            },
            Target::Pr {
                owner: owner_b,
                repo: repo_b,
                number: number_b,
                ..
            },
        ) => {
            number_a == number_b
                && owner_a.eq_ignore_ascii_case(owner_b)
                && repo_a.eq_ignore_ascii_case(repo_b)
        }
        _ => a == b,
    }
}

/// Whether a target means different changes in different worktrees of one repo.
fn is_worktree_bound(target: &Target) -> bool {
    matches!(target, Target::Working | Target::Staged)
}
