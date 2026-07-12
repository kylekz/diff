//! Review data model: a [`Review`] is a draft (or submitted) pass over a
//! [`DiffSource`], carrying a flat list of [`Comment`] threads. Persisted
//! as JSON under `.git/dv/reviews/<id>.json` — see [`io`] for the
//! local/WSL I/O split and [`store`] for the [`ReviewStore`] API that ties
//! it to a repo. This module itself is pure data + validation, no I/O, so
//! it's trivially unit-testable.
//!
//! Comments anchor to `(path, blob_sha, side, start_line, end_line)`
//! (docs/architecture.md § Data model): the blob sha lets a future layer
//! detect that the anchored content has drifted and flag the thread stale,
//! rather than silently pointing at the wrong lines.

mod io;
mod store;
mod watch;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::git::DiffSource;

pub use io::resolve_local_git_dir;
pub use store::ReviewStore;
pub use watch::ReviewWatcher;

/// Schema version written by this build. A file with a higher `v` is from
/// a future dv version this build doesn't understand — refuse to load it
/// (see [`check_schema_version`]) rather than risk misinterpreting fields
/// that don't exist yet in this struct.
pub const SCHEMA_VERSION: u32 = 1;

/// One pass over a diff: metadata plus its comment threads.
///
/// `#[serde(deny_unknown_fields)]` is deliberately never used anywhere in
/// this model — a newer dv writing extra fields must stay readable by an
/// older build (forward compat), matching phase-2's schema-versioning
/// plan of only failing on a `v` bump, not on unrecognized fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    /// Schema version; always [`SCHEMA_VERSION`] on anything this build
    /// writes.
    pub v: u32,
    /// `r-<created_ms>-<4 hex>`.
    pub id: String,
    pub source: DiffSource,
    pub state: ReviewState,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub comments: Vec<Comment>,
    /// Linkage to a GitHub PR this review is attached to, if any — absent
    /// for a plain local review (working tree / staged / arbitrary range).
    /// Optional and defaulted so old on-disk JSON (written before phase 3)
    /// still loads: [`Review`] never uses `deny_unknown_fields`, and this
    /// field follows the same forward/backward-compat rule the module doc
    /// describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteRef>,
}

/// Where a [`Review`] came from / is headed on GitHub: which PR it's
/// reviewing, and — once submitted — the review `gh` created there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRef {
    /// Always `"github"` today; a field (not an assumption) so a future
    /// second provider doesn't need a schema bump.
    pub provider: String,
    /// `host/owner/repo`, matching [`crate::github::RepoSlug`]'s `Display`.
    pub slug: String,
    pub pr: u64,
    pub url: String,
    /// Set once the review is actually submitted to GitHub.
    #[serde(default)]
    pub submitted_review_id: Option<u64>,
    #[serde(default)]
    pub submitted_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Draft,
    Submitted { verdict: Verdict, at_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Comment,
    Approve,
    RequestChanges,
}

/// Which side of a diff a comment anchors to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Old,
    New,
}

/// A single-line or range comment thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    /// `c-<created_ms>-<4 hex>`.
    pub id: String,
    /// Repo-root-relative path, forward-slashed (matches [`ChangedFile`]).
    ///
    /// [`ChangedFile`]: crate::git::ChangedFile
    pub path: String,
    pub side: Side,
    /// 1-based, inclusive.
    pub start_line: u32,
    /// 1-based, inclusive; `>= start_line`.
    pub end_line: u32,
    /// The anchored side's blob sha at creation time (from
    /// [`crate::GitRepo::blob_sha`]), or `None` when the side has no blob
    /// (e.g. commenting on the new side of a to-be-added file before it's
    /// committed). Used later to detect a stale anchor.
    pub blob_sha: Option<String>,
    /// Markdown.
    pub body: String,
    pub author: String,
    pub status: CommentStatus,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub replies: Vec<Reply>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommentStatus {
    Open,
    Resolved,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    /// `p-<created_ms>-<4 hex>` (`p` for "reply", `r` and `c` already being
    /// taken by [`Review`] and [`Comment`]).
    pub id: String,
    pub body: String,
    pub author: String,
    pub created_ms: u64,
}

impl Review {
    /// A fresh draft review over `source`. Prefer
    /// [`ReviewStore::create`](store::ReviewStore::create), which also
    /// persists it — this constructor does no I/O.
    pub(crate) fn new_draft(source: DiffSource) -> Self {
        let now = now_ms();
        Self {
            v: SCHEMA_VERSION,
            id: gen_id('r', now),
            source,
            state: ReviewState::Draft,
            created_ms: now,
            updated_ms: now,
            comments: Vec::new(),
            remote: None,
        }
    }

    /// Append a new open comment. Validates `start_line >= 1`,
    /// `end_line >= start_line`, and a non-empty (after trim) `body`.
    /// Returns a reference to the stored comment.
    #[allow(clippy::too_many_arguments)]
    pub fn add_comment(
        &mut self,
        path: impl Into<String>,
        side: Side,
        start_line: u32,
        end_line: u32,
        blob_sha: Option<String>,
        body: impl Into<String>,
        author: impl Into<String>,
    ) -> Result<&Comment> {
        if start_line < 1 {
            bail!("comment start_line must be >= 1, got {start_line}");
        }
        if end_line < start_line {
            bail!("comment end_line ({end_line}) must be >= start_line ({start_line})");
        }
        let body = body.into();
        if body.trim().is_empty() {
            bail!("comment body must not be empty");
        }

        let now = now_ms();
        let id = self.fresh_id('c', now);
        let comment = Comment {
            id,
            path: path.into(),
            side,
            start_line,
            end_line,
            blob_sha,
            body,
            author: author.into(),
            status: CommentStatus::Open,
            created_ms: now,
            updated_ms: now,
            replies: Vec::new(),
        };
        self.comments.push(comment);
        self.updated_ms = now;
        Ok(self.comments.last().expect("just pushed"))
    }

    /// Append a reply to an existing comment thread. Validates a non-empty
    /// (after trim) `body`. Errors if `comment_id` doesn't exist.
    pub fn reply(
        &mut self,
        comment_id: &str,
        body: impl Into<String>,
        author: impl Into<String>,
    ) -> Result<&Reply> {
        let body = body.into();
        if body.trim().is_empty() {
            bail!("reply body must not be empty");
        }
        let now = now_ms();
        let id = self.fresh_id('p', now);

        let comment = self
            .comments
            .iter_mut()
            .find(|c| c.id == comment_id)
            .ok_or_else(|| anyhow!("no comment with id {comment_id:?} in review {}", self.id))?;
        comment.replies.push(Reply {
            id,
            body,
            author: author.into(),
            created_ms: now,
        });
        comment.updated_ms = now;
        self.updated_ms = now;
        Ok(comment.replies.last().expect("just pushed"))
    }

    /// Set a comment's open/resolved status. Errors if `comment_id`
    /// doesn't exist.
    pub fn set_status(&mut self, comment_id: &str, status: CommentStatus) -> Result<()> {
        let now = now_ms();
        let comment = self
            .comments
            .iter_mut()
            .find(|c| c.id == comment_id)
            .ok_or_else(|| anyhow!("no comment with id {comment_id:?} in review {}", self.id))?;
        comment.status = status;
        comment.updated_ms = now;
        self.updated_ms = now;
        Ok(())
    }

    /// Move the review to a new state (draft → submitted, or back to
    /// resubmit with a different verdict).
    pub fn set_state(&mut self, state: ReviewState) {
        self.state = state;
        self.updated_ms = now_ms();
    }

    /// Whether `id` is already in use by a comment or reply in this
    /// review, so [`Self::fresh_id`] can regenerate on the (astronomically
    /// unlikely) chance [`gen_id`] collides.
    fn id_in_use(&self, id: &str) -> bool {
        self.comments
            .iter()
            .any(|c| c.id == id || c.replies.iter().any(|r| r.id == id))
    }

    fn fresh_id(&self, prefix: char, now: u64) -> String {
        loop {
            let candidate = gen_id(prefix, now);
            if !self.id_in_use(&candidate) {
                return candidate;
            }
        }
    }
}

/// Milliseconds since the Unix epoch. A clock before 1970 (unreachable in
/// practice) clamps to 0 rather than panicking.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `<prefix>-<now_ms>-<4 lowercase hex>`.
///
/// The 4 hex chars are the per-process [`ID_COUNTER`]'s low 16 bits XORed
/// with a salt derived from `now_ms` and the process id. Deliberately
/// *not* a hash of `(now_ms, pid, counter)` truncated to 16 bits: hashing
/// the counter along with everything else and then truncating throws away
/// the one thing that actually guarantees uniqueness (a 64-bit avalanche
/// mix is a bijection, but a bijection on 64 bits is **not** one on its
/// low 16 bits — truncating it collides via the birthday paradox well
/// before 65536 calls, ~1000 draws already gives ~7 expected collisions).
/// Keeping the counter's low bits unmixed and only XORing in a
/// counter-independent salt (XOR by a constant *is* a bijection on 16
/// bits) means two ids collide only if two calls' counters share the same
/// low 16 bits — i.e. only after >65536 ids from this process, which
/// [`Review::fresh_id`] regenerates around as a backstop anyway.
fn gen_id(prefix: char, now_ms: u64) -> String {
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = u64::from(std::process::id());

    // Cheap avalanche mix (fmix64-style) of everything *except* the
    // counter, so the salt doesn't just look like `now_ms` shifted around.
    let mut salt =
        now_ms.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ pid.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    salt ^= salt >> 33;
    salt = salt.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    salt ^= salt >> 33;

    let value = (counter as u16) ^ (salt as u16);
    format!("{prefix}-{now_ms}-{value:04x}")
}

/// A loaded review's `v` must not exceed what this build understands —
/// otherwise it may carry fields (or field *meanings*) this version has
/// never heard of, and silently misinterpreting them is worse than a
/// loud, actionable error telling the user to upgrade dv.
pub(crate) fn check_schema_version(v: u32) -> Result<()> {
    if v > SCHEMA_VERSION {
        bail!(
            "review schema version {v} is newer than this build of dv supports (max {SCHEMA_VERSION}); upgrade dv"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Review {
        Review::new_draft(DiffSource::WorkingTree)
    }

    #[test]
    fn gen_id_shape() {
        let id = gen_id('c', 1_700_000_000_000);
        assert!(id.starts_with("c-1700000000000-"));
        let hex = id.rsplit('-').next().unwrap();
        assert_eq!(hex.len(), 4);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn gen_id_uniqueness_over_1000() {
        let now = now_ms();
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| gen_id('c', now)).collect();
        assert_eq!(ids.len(), 1000, "expected 1000 unique ids, got collisions");
    }

    #[test]
    fn schema_version_ok_and_rejects_future() {
        assert!(check_schema_version(1).is_ok());
        let err = check_schema_version(2).unwrap_err();
        assert!(
            err.to_string().contains('2'),
            "error should mention version: {err}"
        );
    }

    #[test]
    fn serde_round_trip_draft_review_with_comment_and_reply() {
        let mut review = draft();
        review
            .add_comment(
                "src/main.rs",
                Side::New,
                10,
                12,
                Some("abc123".to_string()),
                "why is this here?",
                "kyle",
            )
            .unwrap();
        let comment_id = review.comments[0].id.clone();
        review.reply(&comment_id, "good question", "agent").unwrap();

        let json = serde_json::to_string_pretty(&review).unwrap();
        let round_tripped: Review = serde_json::from_str(&json).unwrap();

        assert_eq!(round_tripped.id, review.id);
        assert_eq!(round_tripped.comments.len(), 1);
        assert_eq!(round_tripped.comments[0].replies.len(), 1);
        assert_eq!(round_tripped.state, ReviewState::Draft);
    }

    #[test]
    fn old_review_json_without_remote_field_still_loads() {
        // A pre-phase-3 review file, written before `remote` existed.
        let old_json = r#"{
            "v": 1,
            "id": "r-1700000000000-abcd",
            "source": "WorkingTree",
            "state": "draft",
            "created_ms": 1700000000000,
            "updated_ms": 1700000000000,
            "comments": []
        }"#;
        let review: Review = serde_json::from_str(old_json).unwrap();
        assert_eq!(review.id, "r-1700000000000-abcd");
        assert!(review.remote.is_none());

        // And it must not have grown a `"remote"` key on the way back out.
        let rewritten = serde_json::to_string(&review).unwrap();
        assert!(!rewritten.contains("\"remote\""));
    }

    #[test]
    fn review_with_remote_ref_round_trips() {
        let mut review = draft();
        review.remote = Some(RemoteRef {
            provider: "github".to_string(),
            slug: "github.com/kylekz/difftest".to_string(),
            pr: 7,
            url: "https://github.com/kylekz/difftest/pull/7".to_string(),
            submitted_review_id: Some(123),
            submitted_url: Some(
                "https://github.com/kylekz/difftest/pull/7#pullrequestreview-123".to_string(),
            ),
        });

        let json = serde_json::to_string_pretty(&review).unwrap();
        let round_tripped: Review = serde_json::from_str(&json).unwrap();

        let remote = round_tripped.remote.expect("remote should round-trip");
        assert_eq!(remote.provider, "github");
        assert_eq!(remote.slug, "github.com/kylekz/difftest");
        assert_eq!(remote.pr, 7);
        assert_eq!(remote.submitted_review_id, Some(123));
    }

    #[test]
    fn remote_ref_with_no_submission_yet_omits_optional_fields_but_still_parses() {
        // A review just linked to a PR, before any review has been
        // submitted through it.
        let mut review = draft();
        review.remote = Some(RemoteRef {
            provider: "github".to_string(),
            slug: "github.com/kylekz/difftest".to_string(),
            pr: 3,
            url: "https://github.com/kylekz/difftest/pull/3".to_string(),
            submitted_review_id: None,
            submitted_url: None,
        });
        let json = serde_json::to_string(&review).unwrap();
        let round_tripped: Review = serde_json::from_str(&json).unwrap();
        let remote = round_tripped.remote.unwrap();
        assert_eq!(remote.submitted_review_id, None);
        assert_eq!(remote.submitted_url, None);
    }

    #[test]
    fn state_serializes_as_documented() {
        assert_eq!(
            serde_json::to_string(&ReviewState::Draft).unwrap(),
            "\"draft\""
        );

        let submitted = ReviewState::Submitted {
            verdict: Verdict::Approve,
            at_ms: 42,
        };
        assert_eq!(
            serde_json::to_string(&submitted).unwrap(),
            r#"{"submitted":{"verdict":"approve","at_ms":42}}"#
        );

        assert_eq!(
            serde_json::to_string(&Verdict::RequestChanges).unwrap(),
            "\"request_changes\""
        );
        assert_eq!(serde_json::to_string(&Side::Old).unwrap(), "\"old\"");
        assert_eq!(serde_json::to_string(&Side::New).unwrap(), "\"new\"");
        assert_eq!(
            serde_json::to_string(&CommentStatus::Open).unwrap(),
            "\"open\""
        );
        assert_eq!(
            serde_json::to_string(&CommentStatus::Resolved).unwrap(),
            "\"resolved\""
        );
    }

    #[test]
    fn unknown_version_error_mentions_version() {
        let mut review = draft();
        review.v = 99;
        let json = serde_json::to_string(&review).unwrap();
        // Loading goes through `store::ReviewStore`, which calls
        // `check_schema_version` after deserializing; exercise the check
        // directly here since this module has no I/O of its own.
        let parsed: Review = serde_json::from_str(&json).unwrap();
        let err = check_schema_version(parsed.v).unwrap_err();
        assert!(err.to_string().contains("99"));
    }

    #[test]
    fn add_comment_validates_range_and_body() {
        let mut review = draft();
        assert!(
            review
                .add_comment("a.txt", Side::New, 0, 0, None, "body", "kyle")
                .is_err(),
            "start_line 0 must be rejected"
        );
        assert!(
            review
                .add_comment("a.txt", Side::New, 5, 4, None, "body", "kyle")
                .is_err(),
            "end_line < start_line must be rejected"
        );
        assert!(
            review
                .add_comment("a.txt", Side::New, 1, 1, None, "   ", "kyle")
                .is_err(),
            "whitespace-only body must be rejected"
        );
        assert!(review.comments.is_empty());
    }

    #[test]
    fn reply_and_set_status_error_on_unknown_comment_id() {
        let mut review = draft();
        assert!(review.reply("c-nope", "body", "kyle").is_err());
        assert!(
            review
                .set_status("c-nope", CommentStatus::Resolved)
                .is_err()
        );
    }

    #[test]
    fn set_status_and_set_state_update_timestamps() {
        let mut review = draft();
        review
            .add_comment("a.txt", Side::New, 1, 1, None, "body", "kyle")
            .unwrap();
        let comment_id = review.comments[0].id.clone();

        review
            .set_status(&comment_id, CommentStatus::Resolved)
            .unwrap();
        assert_eq!(review.comments[0].status, CommentStatus::Resolved);

        review.set_state(ReviewState::Submitted {
            verdict: Verdict::Approve,
            at_ms: now_ms(),
        });
        assert!(matches!(review.state, ReviewState::Submitted { .. }));
    }

    #[test]
    fn fresh_id_regenerates_on_collision() {
        let mut review = draft();
        review
            .add_comment("a.txt", Side::New, 1, 1, None, "body", "kyle")
            .unwrap();
        let existing_id = review.comments[0].id.clone();
        assert!(review.id_in_use(&existing_id));

        // fresh_id must never return an id already in use, even if asked
        // for the same prefix/timestamp repeatedly.
        for _ in 0..100 {
            let candidate = review.fresh_id('c', review.comments[0].created_ms);
            assert_ne!(candidate, existing_id);
        }
    }
}
