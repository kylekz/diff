//! Headless core for dv: repository access, diff model, and review state.
//!
//! Nothing UI-related belongs in this crate — it must stay buildable and
//! testable without a display. It is also the seam for the future WSL host
//! process split (see docs/phase-4-wsl.md): everything here that touches a
//! repository does so by shelling out to `git`, never via an in-process git
//! library, so the same code path can route through `wsl.exe`.

pub mod command;
pub mod conflict;
pub mod diff;
pub mod git;
pub mod github;
pub mod index;
pub mod location;
pub mod lsp;
pub mod provision;
pub mod remote;
pub mod review;

pub use command::CommandBuilder;
pub use conflict::{ConflictInfo, ConflictProbe};
pub use diff::{DiffLine, DiffOptions, FileDiff, Hunk, LineKind};
pub use git::{BlobSpec, ChangeStatus, ChangedFile, DiffSource, DiffTotals, GitRepo, parse_range};
pub use github::{
    ChecksSummary, CreatePr, CreatedPr, DraftComment, GhError, GhSide, GithubClient, Mention,
    OpenTarget, PrMeta, PrState, PrStatus, PrSummary, RemoteComment, RemoteThread, RepoSlug,
    ReviewDecision, ReviewEvent, ReviewSubmission, SubmittedReview, gh_status, parse_open_target,
};
pub use index::{
    CachedPrStatus, EntryHealth, HydrateOutcome, IndexEntry, ReviewIndex, entry_title,
    hydrate_location, repo_label,
};
pub use location::RepoLocation;
pub use provision::{
    ComponentId, ComponentReport, ComponentState, ConsentAction, ConsistencyReport, DetectError,
    NodeVtsls, consistency_check, detect_node_vtsls, install_vtsls,
};
pub use remote::{HostClient, RequestFailure};
pub use review::{
    Comment, CommentStatus, LiveBase, RemoteRef, Reply, Review, ReviewState, ReviewStore,
    ReviewWatcher, Side, Verdict, anchor_spec, format_review_as_prompt,
};

pub const APP_NAME: &str = "dv";
