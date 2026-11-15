//! Reviews as append-only event logs (docs/PLAN.md D5, §2, §7).
//!
//! - [`id`]: `r-`/`t-`/`c-`/`e-` ULID ids, strictly validated before any path is built.
//! - [`event`]: the event enum, one JSON line each, and the forgiving log reader.
//! - [`model`]: the folded state — target, status, rounds, viewed files, threads, PR link.
//! - [`fold`]: events → state, with the status and thread-state rules (PLAN §2.4, §2.5).
//! - [`store`]: the logs, their snapshot cache and refs, over a [`crate::Backend`].
//! - [`snapshot`]: a round's new side kept as a commit under `refs/dv/` (PLAN §2.3).
//! - [`anchor`]: anchors built from a selection of a side's lines (PLAN §2.5).
//! - [`relocate`]: relocation and placement — where a thread shows once its file moved on
//!   (PLAN §8).
//! - [`history`]: a thread's round history: its file followed since its round, and where its
//!   code went (PLAN §8).
//! - [`excerpt`]: the rows of the diff around a thread's lines now, as the CLI prints them
//!   (PLAN §10.3).
//! - [`identity`]: the reviewer's name (PLAN §2.5).
//! - [`key`]: how a review's threads are named where GitHub's show beside its own
//!   (PLAN §9.6).
//! - [`home`]: the worktree a review's target resolves in, whichever worktree reads it
//!   (PLAN §2.2, §2.4).
//! - [`pins`]: the commits a PR review shows, and every review's two sides (PLAN §9.5).
//! - [`ops`]: checked changes — what the UI and the CLI call.
//!
//! Many writers (the GUI, the CLI, any number of agents, in Windows and inside WSL) append to
//! one log at once without a lock: each event is one line written in one append (PLAN D5), and
//! every reader folds the lines in file order, so no update is ever lost.

pub mod anchor;
pub mod event;
pub mod excerpt;
pub mod fold;
pub mod history;
pub mod home;
pub mod id;
pub mod identity;
pub mod key;
pub mod model;
pub mod ops;
pub mod pins;
pub mod relocate;
pub mod snapshot;
pub mod store;

pub use anchor::{ANCHOR_CONTEXT, AnchorError, AnchorPlace, anchor_selection, side_lines};
pub use event::{EVENT_VERSION, Event, EventBody, LogWarning, ParsedLog, parse_log};
pub use excerpt::{ExcerptGap, ExcerptRow, excerpt, excerpt_lines};
pub use fold::{FoldError, Folded, Outcome, Rejection, fold};
pub use history::{
    FileHistory, History, MAX_PAIR_READS, PAIR_MIN_SHARED_CHARS, PAIR_MIN_SHARED_LINES,
    PAIR_THRESHOLD, RoundHistory, SUBSTANTIVE_CHARS, Trail, Went, content_match,
    content_similarity, is_substantive, read_round_history, trace,
};
pub use home::{Home, HomeError, Worktrees, home, same_worktree};
pub use id::{CommentId, EventId, InvalidId, ItemId, ReviewId, ThreadId};
pub use identity::reviewer_identity;
pub use key::{GITHUB_PREFIX, ThreadKey};
pub use model::{
    Anchor, AnchorSide, Author, AuthorKind, Comment, PrLink, Review, ReviewStatus, ReviewSummary,
    Round, StateChange, StatusChange, Submission, Thread, ThreadCounts, ThreadState, Verdict,
};
pub use ops::{FinishedRound, OpError, Opened};
pub use pins::Pins;
pub use relocate::{
    FuzzyLines, MoveKind, Placement, Relocation, SideLines, SideText, ThreadFile, anchored_entry,
    edited_since, entry_for, has_side, is_gone, moved_since, place_anchor, place_thread,
    place_unlisted, place_unlisted_anchor, relocate, relocate_with, relocated_anchor, shows_edited,
    shows_moved, thread_file, unlisted_path,
};
pub use snapshot::SnapshotError;
pub use store::{ListEntry, LoadedReview, ReviewStore, StoreError};

#[cfg(test)]
mod tests;
