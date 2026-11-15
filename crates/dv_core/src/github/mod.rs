//! GitHub, through `gh` alone (docs/PLAN.md D7, §9): dv stores no token and talks to no
//! GitHub endpoint itself.
//!
//! - [`repo`]: which GitHub repository a clone is, from its remotes (PLAN §9.1).
//! - [`gh`]: running `gh` — the program, its environment, the request on stdin, timeouts,
//!   every result as its JSON or a [`GithubError`] kind, lists read whole, the pace
//!   (PLAN §9.2).
//! - [`pr_ref`]: how a pull request is named (PLAN §9.3).
//! - [`mirror`]: what GitHub holds about a pull request, cached beside the reviews
//!   (PLAN §9.4).
//! - [`spelling`]: a GitHub name as a component of a store path or of a ref (PLAN §9.3,
//!   §9.4).
//! - [`time`]: GitHub's timestamps in dv's milliseconds.
//! - [`error`]: why any of it failed, [`GithubError`].
//!
//! `gh` runs on the machine dv runs on, whatever the repository's location: GitHub calls need
//! the repository's name, not its files. Only git runs where the repository lives, through
//! its [`crate::Backend`]: reading the remotes here, fetching a pull request's commits.

pub mod error;
pub mod gh;
pub mod mirror;
pub mod pr_ref;
pub mod repo;
pub mod spelling;
pub mod time;

pub use error::{Elsewhere, GhFailure, GithubError, KEPT_BYTES};
pub use gh::{
    AuthStatus, Connection, GH_VAR, Gh, GhOptions, MAX_PAGES, Mutation, Nested, PAGE_SIZE, Paged,
    Truncated, program, says_signed_out,
};
pub use mirror::store::{MirrorError, MirrorRead, MirrorStore, Refetch};
pub use mirror::{
    Checks, CommentReview, FilePatch, FileStatus, GithubComment, GithubThread, HeadRepo,
    MIRROR_VERSION, Mirror, MirrorKey, PendingReview, PrState, PullRequest, ReviewDecision,
    ReviewState, ThreadLines, ThreadStart, ThreadSubject, ViewedFile, ViewedState,
};
pub use pr_ref::{InvalidPrRef, PrRef};
pub use repo::{ConfiguredRemote, GITHUB_COM, GithubRepo, Place, Remote, RepoUrl};
pub use spelling::spell;
pub use time::parse_timestamp;

#[cfg(test)]
mod tests;
