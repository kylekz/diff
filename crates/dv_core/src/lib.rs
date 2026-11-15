//! dv's headless domain logic: repo locations, the `Backend` interface and its in-process
//! `LocalBackend`, review targets and their labels, the git runner, the diff pipeline, the
//! review event store, anchoring and relocation, GitHub through `gh` (docs/PLAN.md §2,
//! §5–§9), and the job object that children dying with dv join on Windows (`job`).
//!
//! gpui: **never**. This crate runs in the CLI, inside WSL distros as part of the backend, and in
//! fast headless tests; `script/check` fails when gpui appears in its dependency tree
//! (PLAN §4, §13.2).

pub mod backend;
pub mod diff;
pub mod git;
pub mod github;
pub mod job;
pub mod label;
pub mod location;
pub mod review;
pub mod target;

pub use backend::{Backend, BackendError, LocalBackend, LocalBackendOptions, RepoInfo};
pub use diff::{DiffError, FileDiff, FileEntry, TextDiff};
pub use git::{GitError, GitRepo, ObjectFormat, Oid};
pub use github::{Gh, GithubError, GithubRepo, PrRef};
pub use label::{LabelContext, side_labels, target_label};
pub use location::{Location, LocationError};
pub use review::{Review, ReviewId, ReviewStore, StoreError};
pub use target::{DiffSpec, Side, Target, TargetError};

#[cfg(test)]
pub(crate) mod testing;
