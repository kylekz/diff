//! The PR mirror (docs/PLAN.md §9.4): what GitHub holds about one pull request, as one login
//! sees it, kept as a cache beside the reviews.
//!
//! What GitHub holds is GitHub's and never enters a review's log (PLAN §7). A refresh reads it
//! all again and rewrites the mirror whole; every review of the pull request, from every
//! worktree, reads the same file. Nothing in it is authority for dv's own review state.
//!
//! The file is `{"v": 1, …}` with the fields of [`Mirror`] in snake_case. Unknown fields are
//! ignored, so an optional addition needs no version bump; anything else changes
//! [`MIRROR_VERSION`], and a file of another version is fetched again
//! ([`store::MirrorRead`]).

pub mod store;

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use super::repo::{GithubRepo, hash_folded};
use super::spelling::spell;
use crate::git::Oid;
use crate::review::AnchorSide;

/// The mirror's format version.
pub const MIRROR_VERSION: u32 = 1;

/// Which mirror: a pull request as one login sees it. Per login, because Viewed state, the
/// pending review and what the viewer may resolve are the login's, and the app's `gh` and a
/// distro's may be signed in as different users (PLAN §9.2).
///
/// GitHub's hosts, names and logins have no case, so equality and the hash fold it; the
/// fields keep the spelling they were given.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MirrorKey {
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    /// The viewer: the login `gh` is signed in as on `host`.
    pub login: String,
}

impl MirrorKey {
    pub fn new(repo: &GithubRepo, number: u64, login: &str) -> Self {
        Self {
            host: repo.host.clone(),
            owner: repo.owner.clone(),
            repo: repo.repo.clone(),
            number,
            login: login.to_string(),
        }
    }

    /// The mirror's file, relative to the store root (`<git-common-dir>/dv/`):
    /// `github/<host>/<owner>/<repo>/pr-<n>/<login>.json`, each name spelled as
    /// [`spell`] spells it.
    pub fn path(&self) -> String {
        format!(
            "{}/{}.json",
            pr_dir(&self.host, &self.owner, &self.repo, self.number),
            spell(&self.login)
        )
    }
}

impl PartialEq for MirrorKey {
    fn eq(&self, other: &Self) -> bool {
        self.number == other.number
            && self.host.eq_ignore_ascii_case(&other.host)
            && self.owner.eq_ignore_ascii_case(&other.owner)
            && self.repo.eq_ignore_ascii_case(&other.repo)
            && self.login.eq_ignore_ascii_case(&other.login)
    }
}

impl Eq for MirrorKey {}

impl Hash for MirrorKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for name in [&self.host, &self.owner, &self.repo, &self.login] {
            hash_folded(name, state);
        }
        self.number.hash(state);
    }
}

/// The store directory holding every login's mirror of one pull request, relative to the
/// store root: `github/<host>/<owner>/<repo>/pr-<n>`.
fn pr_dir(host: &str, owner: &str, repo: &str, number: u64) -> String {
    format!(
        "github/{}/{}/{}/pr-{number}",
        spell(host),
        spell(owner),
        spell(repo)
    )
}

/// [`pr_dir`] of a pull request whose owner and repository are already spelled, as its refs
/// under `refs/dv/pr/` name them.
pub(super) fn spelled_pr_dir(host: &str, owner: &str, repo: &str, number: u64) -> String {
    format!("github/{}/{owner}/{repo}/pr-{number}", spell(host))
}

/// Whether `path`, a store path, is a mirror of pull request `number` of `owner/repo`, on
/// whichever host and for whichever login: what a review of that pull request follows in
/// the store's change events (PLAN §9.4).
pub fn is_mirror_of(path: &str, owner: &str, repo: &str, number: u64) -> bool {
    let mut parts = path.split('/');
    let dir = [
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ];
    matches!(
        dir,
        [Some("github"), Some(_), Some(o), Some(r), Some(pr)]
            if o == spell(owner) && r == spell(repo) && pr == format!("pr-{number}")
    ) && parts.next().is_some_and(|file| file.ends_with(".json"))
        && parts.next().is_none()
}

/// Everything GitHub holds about a pull request that dv shows or checks against (PLAN §9.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mirror {
    pub key: MirrorKey,
    /// When it was read, in ms since the Unix epoch.
    pub read_at: u64,
    pub pr: PullRequest,
    /// The old side of the pull request's diff at [`PullRequest::head_oid`]: the merge base
    /// of that head and the base branch as the refresh fetched them (PLAN §9.3 step 3), which
    /// a thread on the old side at that head is read in (PLAN §9.6). `None` in a mirror that
    /// doesn't say.
    #[serde(default)]
    pub merge_base_oid: Option<Oid>,
    pub checks: Checks,
    /// Every review thread with every comment (PLAN §9.6), the viewer's own pending comments
    /// included.
    pub threads: Vec<GithubThread>,
    /// The viewer's pending review, when there is one (PLAN §9.8).
    pub pending_review: Option<PendingReview>,
    /// The files the pull request changes at [`PullRequest::head_oid`], in GitHub's order
    /// (its first 3,000), each with GitHub's own patch.
    pub files: Vec<FilePatch>,
    /// The viewer's Viewed state of each file, by path (PLAN §9.7).
    pub viewed: BTreeMap<String, ViewedFile>,
}

impl Mirror {
    /// The comments of the viewer's pending review, with their threads: the ones whose review
    /// it is.
    pub fn pending_comments(&self) -> impl Iterator<Item = (&GithubThread, &GithubComment)> {
        let pending = self.pending_review.as_ref().map(|review| &review.id);
        self.threads
            .iter()
            .flat_map(|thread| thread.comments.iter().map(move |comment| (thread, comment)))
            .filter(move |(_, comment)| {
                pending.is_some() && comment.review.as_ref().map(|review| &review.id) == pending
            })
    }
}

/// A pull request's metadata (PLAN §9.3). Its number is the mirror's key's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// GitHub's node id, which mutations name it by.
    pub id: String,
    pub title: String,
    /// Markdown.
    pub body: String,
    pub url: String,
    pub state: PrState,
    /// The author's login; `ghost` for a deleted account.
    pub author: String,
    /// The base branch's name.
    pub base_ref: String,
    /// The head branch's name.
    pub head_ref: String,
    /// The head commit, which everything in the mirror was read at.
    pub head_oid: Oid,
    pub head_repo: HeadRepo,
    /// When the pull request last changed, in ms since the Unix epoch.
    pub updated_at: u64,
    /// `None` when no review is required and none was given.
    pub review_decision: Option<ReviewDecision>,
    pub changed_files: u32,
    pub additions: u32,
    pub deletions: u32,
}

/// A pull request's state as dv shows it (PLAN §9.9). A draft is an open one; a closed or
/// merged pull request is no longer a draft to anyone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

/// Where a pull request's head branch lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadRepo {
    /// The base repository itself.
    Same,
    /// Another repository; `owner` is its owner's login, `None` once the fork is deleted.
    Fork { owner: Option<String> },
}

/// What the pull request's reviews add up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// The checks on the head commit, summarised. All zero when it has none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checks {
    pub passing: u32,
    pub failing: u32,
    pub pending: u32,
}

/// One of GitHub's review threads (PLAN §9.6): shown in dv, never stored in a review's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubThread {
    /// GitHub's node id.
    pub id: String,
    pub path: String,
    pub resolved: bool,
    /// GitHub's own word that the code under the thread has changed since.
    pub outdated: bool,
    pub subject: ThreadSubject,
    pub viewer_can_resolve: bool,
    pub viewer_can_unresolve: bool,
    pub viewer_can_reply: bool,
    /// In GitHub's order, the thread's first comment first.
    pub comments: Vec<GithubComment>,
}

impl GithubThread {
    /// Whether the thread is the viewer's own, not yet submitted: its first comment belongs
    /// to a pending review, which only its author sees (PLAN §9.6).
    pub fn is_pending(&self) -> bool {
        self.comments.first().is_some_and(GithubComment::is_pending)
    }

    /// The thread on GitHub: its first comment's URL.
    pub fn url(&self) -> Option<&str> {
        self.comments.first().map(|comment| comment.url.as_str())
    }

    /// [`GithubThread::url`] when it is a page of `host` over https, the only kind of link
    /// dv opens, prints or hands on for a thread (PLAN §9.6): `https://<authority>/…` whose
    /// authority is `host` itself (so no user before it, and a port only when the mirror's
    /// host carries one), with no whitespace and nothing that moves or hides text
    /// (`crate::text::is_unprintable`) anywhere. What the mirror holds is GitHub's answer,
    /// not dv's own text.
    pub fn url_on(&self, host: &str) -> Option<&str> {
        let url = self.url()?;
        let (authority, _) = url.strip_prefix("https://")?.split_once('/')?;
        (authority.eq_ignore_ascii_case(host)
            && !url.contains(|c: char| c.is_whitespace() || crate::text::is_unprintable(c)))
        .then_some(url)
    }

    /// Whether the thread is on the whole file.
    pub fn is_file_level(&self) -> bool {
        self.subject == ThreadSubject::File
    }
}

/// What a thread is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadSubject {
    /// The whole file.
    File,
    /// Lines of one side of the file's diff.
    Lines(ThreadLines),
}

/// Where a line thread is, as GitHub gives it. Lines are 1-based, on the old side's file or
/// the new side's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadLines {
    /// GitHub's `diffSide`: `LEFT` is the old side, `RIGHT` the new.
    pub side: AnchorSide,
    /// The thread's last line in its comments' commit; `None` once GitHub calls the thread
    /// outdated.
    pub line: Option<u32>,
    /// The same line in the commit the thread was made on.
    pub original_line: Option<u32>,
    /// A range's first line, and its side; `None` for a thread on one line.
    pub start: Option<ThreadStart>,
}

/// The first line of a thread on a range of lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadStart {
    /// GitHub's `startDiffSide`.
    pub side: AnchorSide,
    pub line: Option<u32>,
    pub original_line: Option<u32>,
}

/// One comment of a GitHub review thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubComment {
    /// GitHub's node id.
    pub id: String,
    /// GitHub's database id (`fullDatabaseId`), which REST and the comment's URL use.
    pub database_id: u64,
    /// The author's login; `ghost` for a deleted account.
    pub author: String,
    /// Markdown.
    pub body: String,
    /// In ms since the Unix epoch.
    pub created_at: u64,
    pub url: String,
    /// The diff hunk GitHub shows above the comment: the code it was made on.
    pub diff_hunk: String,
    pub outdated: bool,
    /// The commit the comment's `line` is in.
    pub commit: Option<Oid>,
    /// The commit the comment was made on.
    pub original_commit: Option<Oid>,
    /// The node id of the comment this one replies to.
    pub reply_to: Option<String>,
    /// The review the comment belongs to.
    pub review: Option<CommentReview>,
}

impl GithubComment {
    /// Whether the comment belongs to a review that isn't submitted yet: the viewer's own.
    pub fn is_pending(&self) -> bool {
        self.review
            .as_ref()
            .is_some_and(|review| review.state == ReviewState::Pending)
    }
}

/// The review a comment was posted with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentReview {
    /// GitHub's node id.
    pub id: String,
    /// GitHub's database id (`fullDatabaseId`), which a submission is recorded by (PLAN §7).
    pub database_id: u64,
    pub state: ReviewState,
}

/// A GitHub review's state. Another user's `Pending` review is never visible; the viewer's
/// own is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Pending,
    Commented,
    Approved,
    ChangesRequested,
    Dismissed,
}

/// The viewer's pending review on GitHub: dv's own while a submission is under way, or one
/// started elsewhere (PLAN §9.8). Its comments are among the mirror's threads
/// ([`Mirror::pending_comments`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReview {
    /// GitHub's node id.
    pub id: String,
    pub database_id: u64,
    /// Its summary so far, which holds dv's marker when dv made it.
    pub body: String,
    /// The commit GitHub fixed it to.
    pub commit: Option<Oid>,
}

/// One file the pull request changes, with GitHub's own patch for it (PLAN §9.4): the hunks
/// a line comment is validated against (PLAN §9.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePatch {
    /// The file's path at the head; a removed file's path at the base.
    pub path: String,
    /// The path it was renamed or copied from.
    pub previous_path: Option<String>,
    pub status: FileStatus,
    /// GitHub's unified diff of the file, without a file header. `None` for a binary file, a
    /// rename without changes, or a diff too large for GitHub to give: such a file takes file
    /// comments only.
    pub patch: Option<String>,
}

/// How a pull request changes a file, as GitHub's REST API names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Removed,
    Modified,
    Renamed,
    Copied,
    Changed,
    Unchanged,
}

/// A file's Viewed state (PLAN §9.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewedFile {
    /// What GitHub holds for the viewer.
    pub state: ViewedState,
    /// Whether the file was viewed when GitHub's state and dv's own mark last agreed: the
    /// base of the next three-way sync. `None` before the first sync.
    pub synced: Option<bool>,
}

/// GitHub's `viewerViewedState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewedState {
    Viewed,
    Unviewed,
    /// Viewed, then the file changed.
    Dismissed,
}
