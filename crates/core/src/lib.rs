//! Headless core for dv: repository access, diff model, and review state.
//!
//! Nothing UI-related belongs in this crate — it must stay buildable and
//! testable without a display. It is also the seam for the future WSL host
//! process split (see docs/phase-4-wsl.md): everything here that touches a
//! repository does so by shelling out to `git`, never via an in-process git
//! library, so the same code path can route through `wsl.exe`.

pub mod command;
pub mod diff;
pub mod git;
pub mod location;
pub mod review;

pub use command::CommandBuilder;
pub use diff::{DiffLine, DiffOptions, FileDiff, Hunk, LineKind};
pub use git::{BlobSpec, ChangeStatus, ChangedFile, DiffSource, GitRepo};
pub use location::RepoLocation;
pub use review::{Comment, CommentStatus, Reply, Review, ReviewState, ReviewStore, Side, Verdict};

pub const APP_NAME: &str = "dv";
