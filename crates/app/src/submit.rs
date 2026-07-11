//! Shared GitHub review-submission core: the pure comment→[`DraftComment`]
//! mapping, pre-submission validation, and the post-success local writeback
//! — reused by both `dv review submit` (`crates/app/src/cli/pr_cmd.rs`) and
//! the GUI's submit flow (`crates/app/src/workspace.rs`'s `SubmitFlow`,
//! docs/phase-3-github.md deliverable 3/5). Originally lived entirely in
//! `cli/pr_cmd.rs`; factored out here (a sibling of `pr.rs`, which already
//! holds `prepare_pr`/`PrRange`) so the GUI doesn't duplicate — or drift
//! from — the CLI's validation rules.
//!
//! `pr_cmd.rs` keeps only CLI concerns: argument parsing, `--pr`/verdict
//! resolution, `format_violations` (turns a `&[Violation]` into the CLI's
//! `"cannot submit: N problems found..."` text), and the actual `gh` calls.

use std::collections::{BTreeMap, HashMap};

use dv_core::diff::diff_blobs;
use dv_core::{
    BlobSpec, ChangeStatus, ChangedFile, Comment, CommentStatus, DiffOptions, DiffSource,
    DraftComment, GhSide, GitRepo, GithubClient, Hunk, PrMeta, PrState, RemoteRef, Review,
    ReviewEvent, ReviewState, ReviewStore, ReviewSubmission, Side, SubmittedReview, Verdict,
};

/// One comment that can't safely reach GitHub as submitted — produced by
/// [`validate_submission`]. A struct (not a pre-formatted string, unlike the
/// CLI's original `Vec<String>`) so the GUI's Blocked panel can render a
/// real list (file / lines / message) instead of scraping CLI prose; the
/// CLI's own `format_violations` just reads `.message` back out, so its
/// wire output stays byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub comment_id: String,
    pub path: String,
    /// `"12"` for a single line, `"12-15"` for a range — see [`line_range`].
    pub lines: String,
    pub kind: ViolationKind,
    /// The full human-readable sentence (identical wording to what the CLI
    /// printed before this moved).
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationKind {
    /// A recorded `blob_sha` no longer matches the anchored side's current
    /// content — the file changed (rebase/amend/force-push) since the
    /// comment was made.
    StaleAnchor,
    /// No `blob_sha` was ever recorded for this comment — unverifiable.
    Unanchored,
    /// An old-side comment on a file renamed within the PR: the anchor was
    /// recorded against the wrong (new) path and can't be verified.
    RenameUnverifiable,
    /// The comment's line range falls outside every hunk of the PR's diff.
    NotInDiff,
    /// No open comments (or, with `include_resolved`, no comments at all),
    /// no body text, and the verdict is a plain [`Verdict::Comment`] —
    /// GitHub rejects (422) an empty review, so [`build_submission`] blocks
    /// it up front instead of letting either frontend reach the API and
    /// surface a confusing raw error (review finding P2-2: the GUI used to
    /// let this through; the CLI's own pre-existing guard, outside this
    /// shared core, moved in here so both share one rule — see
    /// `cli/pr_cmd.rs::cmd_review_submit`, which keeps its original wording
    /// for this one case by recognizing this violation kind).
    NothingToSubmit,
}

fn line_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn side_word(side: Side) -> &'static str {
    match side {
        Side::Old => "old",
        Side::New => "new",
    }
}

fn violation(comment: &Comment, path: &str, kind: ViolationKind, message: String) -> Violation {
    Violation {
        comment_id: comment.id.clone(),
        path: path.to_string(),
        lines: line_range(comment.start_line, comment.end_line),
        kind,
        message,
    }
}

/// dv stores paths forward-slashed already (docs/architecture.md § Data
/// model), but a stray Windows-style path — hand-edited store JSON, a
/// future GUI path builder bug — must not silently mis-anchor a GitHub
/// comment, so this is enforced again right before it leaves the process.
pub fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// The main body, then each reply flattened underneath as a markdown
/// blockquote — GitHub review comments have no native reply thread, so this
/// is the closest single-comment-body approximation.
pub fn flatten_body(comment: &Comment) -> String {
    let mut body = comment.body.clone();
    for reply in &comment.replies {
        body.push_str("\n\n");
        body.push_str(&blockquote_reply(&reply.author, &reply.body));
    }
    body
}

/// Prefix *every* line of a reply with `> ` (a blank line gets a bare `>`),
/// with the `> **@author:** ` label folded into the first line — markdown
/// (GitHub's renderer included) treats a blank line as the end of a
/// blockquote, so prefixing only the first line let any later paragraph in
/// a multi-paragraph reply render as the top-level author's own words.
fn blockquote_reply(author: &str, body: &str) -> String {
    let mut lines = body.lines();
    let mut out = format!("> **@{author}:** ");
    if let Some(first) = lines.next() {
        out.push_str(first);
    }
    for line in lines {
        out.push('\n');
        if line.is_empty() {
            out.push('>');
        } else {
            out.push_str("> ");
            out.push_str(line);
        }
    }
    out
}

fn gh_side(side: Side) -> GhSide {
    match side {
        Side::Old => GhSide::Left,
        Side::New => GhSide::Right,
    }
}

pub fn map_comment(comment: &Comment) -> DraftComment {
    let path = normalize_path(&comment.path);
    let body = flatten_body(comment);
    let side = gh_side(comment.side);
    if comment.start_line < comment.end_line {
        DraftComment::range(
            path,
            body,
            u64::from(comment.start_line),
            side,
            u64::from(comment.end_line),
            side,
        )
    } else {
        DraftComment::single_line(path, body, u64::from(comment.end_line), side)
    }
}

pub fn verdict_to_event(verdict: Verdict) -> ReviewEvent {
    match verdict {
        Verdict::Comment => ReviewEvent::Comment,
        Verdict::Approve => ReviewEvent::Approve,
        Verdict::RequestChanges => ReviewEvent::RequestChanges,
    }
}

/// Changed-file map for `base_oid..head_oid`, keyed by each entry's
/// new-side path — `ChangedFile::path` is documented as the new-side path
/// (old side, for deletes), which is also always what a [`Comment::path`]
/// records, even for an old-side anchor on a renamed file (see the
/// docs/backlog.md caveat this fix appends). Built once per validation run
/// so the per-comment checks below are plain map reads instead of a
/// `git diff` per comment.
pub fn changed_file_map(
    repo: &GitRepo,
    base_oid: &str,
    head_oid: &str,
) -> anyhow::Result<BTreeMap<String, ChangedFile>> {
    let files = repo.changed_files(&DiffSource::Range {
        base: base_oid.to_string(),
        head: head_oid.to_string(),
        merge_base: false,
    })?;
    Ok(files.into_iter().map(|f| (f.path.clone(), f)).collect())
}

/// The file's old-side path when `file` is a rename or copy — `None`
/// (caller's own path is already correct) for every other status. Mirrors
/// `workspace.rs::compute_diff`'s `old_path` handling for the GUI diff.
fn rename_old_path(file: &ChangedFile) -> Option<&str> {
    match file.status {
        ChangeStatus::Renamed | ChangeStatus::Copied => file.old_path.as_deref(),
        _ => None,
    }
}

fn validate_comment_anchors(
    repo: &GitRepo,
    base_oid: &str,
    head_oid: &str,
    changed: &BTreeMap<String, ChangedFile>,
    comments: &[&Comment],
) -> anyhow::Result<Vec<Violation>> {
    let mut violations = Vec::new();
    // Keyed on (the path actually looked up, side), not the comment's own
    // path — collapses repeat `blob_sha` lookups for every comment sharing
    // a path/side (100 comments on WSL is otherwise 100 `git ls-tree`
    // spawns just for anchor checks). `Side` has no `Hash` impl, so the
    // side is folded in via `side_word`'s `&'static str` instead of the
    // enum itself.
    let mut blob_cache: HashMap<(String, &'static str), Option<String>> = HashMap::new();

    for &comment in comments {
        let path = normalize_path(&comment.path);
        // Old-side lookups on a file renamed/copied in this PR must read
        // the *old* path — the file never existed under its new name
        // before the rename, so looking it up there always misses.
        let rename_old = changed.get(&path).and_then(rename_old_path);
        let (rev, lookup_path) = match comment.side {
            Side::New => (head_oid, path.as_str()),
            Side::Old => (base_oid, rename_old.unwrap_or(path.as_str())),
        };

        let cache_key = (lookup_path.to_string(), side_word(comment.side));
        let actual = match blob_cache.get(&cache_key) {
            Some(cached) => cached.clone(),
            None => {
                let value = repo.blob_sha(&BlobSpec::Rev {
                    rev: rev.to_string(),
                    path: lookup_path.to_string(),
                })?;
                blob_cache.insert(cache_key, value.clone());
                value
            }
        };

        let verified = comment
            .blob_sha
            .as_deref()
            .is_some_and(|expected| actual.as_deref() == Some(expected));
        if verified {
            continue;
        }

        // Old-side comments on a renamed file were anchored by the
        // GUI/CLI against the *new* path at comment time (docs/backlog.md)
        // — so even once the lookup itself reads the right (old) path,
        // the recorded `blob_sha` may be `None` or simply wrong. Say so
        // plainly instead of the generic stale-anchor text below, which
        // would misleadingly imply the file's *content* changed rather
        // than the anchor never having been recorded against it.
        if comment.side == Side::Old
            && let Some(old_path) = rename_old
        {
            violations.push(violation(
                comment,
                &path,
                ViolationKind::RenameUnverifiable,
                format!(
                    "comment {} on {}:{} (old) is on a file renamed in the PR (previously {}) — \
                     its anchor cannot be verified and needs re-anchoring",
                    comment.id,
                    path,
                    line_range(comment.start_line, comment.end_line),
                    old_path,
                ),
            ));
            continue;
        }

        if comment.blob_sha.is_none() {
            violations.push(violation(
                comment,
                &path,
                ViolationKind::Unanchored,
                format!(
                    "comment {} on {}:{} has no recorded anchor (unverifiable) — cannot safely \
                     place it",
                    comment.id,
                    path,
                    line_range(comment.start_line, comment.end_line),
                ),
            ));
        } else {
            violations.push(violation(
                comment,
                &path,
                ViolationKind::StaleAnchor,
                format!(
                    "comment {} on {}:{} ({}) has a stale anchor — the file has changed since \
                     the comment was made",
                    comment.id,
                    path,
                    line_range(comment.start_line, comment.end_line),
                    side_word(comment.side),
                ),
            ));
        }
    }
    Ok(violations)
}

fn validate_lines_in_diff(
    repo: &GitRepo,
    base_oid: &str,
    head_oid: &str,
    changed: &BTreeMap<String, ChangedFile>,
    comments: &[&Comment],
) -> anyhow::Result<Vec<Violation>> {
    let mut violations = Vec::new();

    let mut by_path: BTreeMap<String, Vec<&Comment>> = BTreeMap::new();
    for &comment in comments {
        by_path
            .entry(normalize_path(&comment.path))
            .or_default()
            .push(comment);
    }

    for (path, path_comments) in by_path {
        // A rename/copy's real old-side content lives at `old_path` — the
        // *new* path is entirely missing at `base_oid`, which (before this
        // fix) made the whole file look newly added and let every line
        // pass regardless of whether GitHub's rename-aware diff actually
        // covers it.
        let old_side_path = changed
            .get(&path)
            .and_then(rename_old_path)
            .unwrap_or(path.as_str());

        let old_bytes = repo.blob_bytes(&BlobSpec::Rev {
            rev: base_oid.to_string(),
            path: old_side_path.to_string(),
        })?;
        let new_bytes = repo.blob_bytes(&BlobSpec::Rev {
            rev: head_oid.to_string(),
            path: path.clone(),
        })?;
        let diff = diff_blobs(
            old_bytes.as_deref(),
            new_bytes.as_deref(),
            &DiffOptions::default(),
        );

        for comment in path_comments {
            let in_diff = diff
                .hunks
                .iter()
                .any(|h| line_in_hunk_span(h, comment.side, comment.start_line, comment.end_line));
            if !in_diff {
                violations.push(violation(
                    comment,
                    &path,
                    ViolationKind::NotInDiff,
                    format!(
                        "comment {} on {}:{} ({}) is not part of the PR diff",
                        comment.id,
                        path,
                        line_range(comment.start_line, comment.end_line),
                        side_word(comment.side),
                    ),
                ));
            }
        }
    }
    Ok(violations)
}

/// Whether `start..=end` (1-based, inclusive) falls entirely within
/// `hunk`'s span on `side` — context lines included, matching unified-diff
/// convention (`new_start..new_start+new_count`, `old_start..old_start+
/// old_count`). A zero-length span (the side is wholly absent from this
/// hunk — e.g. the old side of a pure insertion) never contains anything.
fn line_in_hunk_span(hunk: &Hunk, side: Side, start: u32, end: u32) -> bool {
    let (span_start, span_len) = match side {
        Side::Old => (hunk.old_start, hunk.old_count),
        Side::New => (hunk.new_start, hunk.new_count),
    };
    if span_len == 0 {
        return false;
    }
    let span_end = span_start + span_len - 1;
    start >= span_start && end <= span_end
}

/// Validate a batch of comments against the PR's current diff before
/// submitting: every comment's anchor must still match the content it was
/// made against ([`validate_comment_anchors`]), and every commented line
/// must actually fall inside the PR's diff ([`validate_lines_in_diff`]).
/// Takes only a [`GitRepo`] and the two oids bounding the diff — no `gh`,
/// no [`PrMeta`] — so it's directly testable against a plain temp repo (see
/// the tests below); [`build_submission`] is the only caller that plugs in
/// real PR data (via the CLI's `cmd_review_submit` or the GUI's submit
/// flow).
pub fn validate_submission(
    repo: &GitRepo,
    base_oid: &str,
    head_oid: &str,
    comments: &[&Comment],
) -> anyhow::Result<Vec<Violation>> {
    let changed = changed_file_map(repo, base_oid, head_oid)?;
    let mut violations = validate_comment_anchors(repo, base_oid, head_oid, &changed, comments)?;
    violations.extend(validate_lines_in_diff(
        repo, base_oid, head_oid, &changed, comments,
    )?);
    Ok(violations)
}

/// [`build_submission`]'s result: either a ready-to-send [`ReviewSubmission`]
/// or the list of [`Violation`]s that blocked it. Not a plain
/// `Result<ReviewSubmission, Vec<Violation>>` because a *hard* I/O failure
/// (a `git` call inside validation erroring) is a third, distinct outcome —
/// that one stays in the outer `anyhow::Result` build_submission itself
/// returns, so callers can tell "GitHub/git broke" apart from "these
/// specific comments can't go out yet".
pub enum SubmissionOutcome {
    Ready(ReviewSubmission),
    Blocked(Vec<Violation>),
}

/// Filter `review`'s comments (open-only unless `include_resolved`),
/// validate them against `merge_base_oid..head_oid`, and — if clean — map
/// them onto a [`ReviewSubmission`] ready for [`GithubClient::submit_review`].
/// Shared by the CLI's `cmd_review_submit` and the GUI's submit flow so
/// neither can drift from the other's validation rules.
pub fn build_submission(
    repo: &GitRepo,
    merge_base_oid: &str,
    head_oid: &str,
    review: &Review,
    verdict: Verdict,
    body: String,
    include_resolved: bool,
) -> anyhow::Result<SubmissionOutcome> {
    let comments: Vec<&Comment> = review
        .comments
        .iter()
        .filter(|c| include_resolved || c.status == CommentStatus::Open)
        .collect();

    // Nothing to submit: checked up front, before any git I/O, so both
    // frontends refuse the same empty reviews the same way (review finding
    // P2-2).
    if comments.is_empty() && body.trim().is_empty() && verdict == Verdict::Comment {
        return Ok(SubmissionOutcome::Blocked(vec![Violation {
            comment_id: String::new(),
            path: String::new(),
            lines: String::new(),
            kind: ViolationKind::NothingToSubmit,
            message: "nothing to submit \u{2014} no open comments and no summary text".to_string(),
        }]));
    }

    let violations = validate_submission(repo, merge_base_oid, head_oid, &comments)?;
    if !violations.is_empty() {
        return Ok(SubmissionOutcome::Blocked(violations));
    }

    let draft_comments: Vec<DraftComment> = comments.into_iter().map(map_comment).collect();
    Ok(SubmissionOutcome::Ready(ReviewSubmission {
        commit_id: head_oid.to_string(),
        body,
        event: verdict_to_event(verdict),
        comments: draft_comments,
    }))
}

/// The error for when GitHub has already accepted a review submission but
/// recording that locally then failed (the review vanished from the store
/// between load and save, or the save itself errored). Factored out as a
/// pure function so both the CLI and GUI paths get identical wording, and
/// so the critical fact — GitHub already has this review, resubmitting
/// would duplicate it — can't accidentally be dropped from one call site
/// but not the other.
pub fn writeback_failure_message(
    pr_number: u64,
    submitted: &SubmittedReview,
    cause: &str,
) -> String {
    format!(
        "GitHub ACCEPTED the review: submitted review #{} on PR #{pr_number} ({}), but recording \
         it in the local review store failed: {cause}\n\
         Do NOT re-run the submission for this review — GitHub already has this review, and \
         submitting again would create a duplicate review on the PR. Fix the local issue (see \
         the cause above), then reconcile the review store by hand if needed.",
        submitted.id, submitted.html_url,
    )
}

/// Fresh-load `review_id` from `store`, mark it `Submitted`, attach
/// `remote` (the just-created GitHub review's linkage), and save — the
/// shared post-success step once `submitted` proves GitHub already
/// accepted the review. Fresh-loads before mutating like every other store
/// write (the review may have changed since the caller's own copy was
/// read). A failure here is returned as [`writeback_failure_message`]'s
/// ready-to-show text, not a raw error — the caller must never retry the
/// whole submit after seeing this, or it would create a duplicate review.
pub fn writeback_submitted_review(
    store: &ReviewStore,
    review_id: &str,
    verdict: Verdict,
    pr_number: u64,
    remote: RemoteRef,
    submitted: &SubmittedReview,
) -> Result<Review, String> {
    let mut fresh = match store.load(review_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Err(writeback_failure_message(
                pr_number,
                submitted,
                &format!("review {review_id} disappeared from the local store"),
            ));
        }
        Err(err) => {
            return Err(writeback_failure_message(
                pr_number,
                submitted,
                &format!("{err:#}"),
            ));
        }
    };
    fresh.set_state(ReviewState::Submitted {
        verdict,
        at_ms: dv_core::review::now_ms(),
    });
    fresh.remote = Some(remote);
    if let Err(err) = store.save(&fresh) {
        return Err(writeback_failure_message(
            pr_number,
            submitted,
            &format!("{err:#}"),
        ));
    }
    Ok(fresh)
}

/// Preflighted, slug-bound `GithubClient` for `repo`, matching the CLI's own
/// `github_client` helper (`cli/pr_cmd.rs`) — used by the GUI's submit flow
/// too so both paths fail the same way on a missing/unauthenticated `gh`.
pub fn github_client(repo: &GitRepo) -> anyhow::Result<GithubClient> {
    let client = GithubClient::for_repo(repo)?;
    client.preflight()?;
    Ok(client)
}

/// `meta.state != PrState::Open` as a ready-to-show message, shared so the
/// CLI and GUI say exactly the same thing about a closed/merged PR.
pub fn pr_not_open_message(pr_number: u64, meta: &PrMeta) -> String {
    format!(
        "PR #{pr_number} is {} — only an open PR can receive a review",
        match meta.state {
            PrState::Open => "open",
            PrState::Closed => "closed",
            PrState::Merged => "merged",
        }
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use dv_core::{Reply, RepoLocation};

    use super::*;

    fn comment(
        path: &str,
        side: Side,
        start: u32,
        end: u32,
        blob_sha: Option<&str>,
        body: &str,
    ) -> Comment {
        Comment {
            // Keyed on `body` (a unique per-case label in the test below),
            // not `path`/`start`/`end` — several test cases deliberately
            // share the same path/line range to isolate one violation kind
            // from another, which would otherwise collide on this id.
            id: format!("c-test-{body}"),
            path: path.to_string(),
            side,
            start_line: start,
            end_line: end,
            blob_sha: blob_sha.map(str::to_string),
            body: body.to_string(),
            author: "kyle".to_string(),
            status: CommentStatus::Open,
            created_ms: 0,
            updated_ms: 0,
            replies: Vec::new(),
        }
    }

    // --- map_comment -----------------------------------------------------

    #[test]
    fn map_comment_single_line_new_side() {
        let c = comment("src/a.rs", Side::New, 5, 5, Some("sha"), "why?");
        let draft = map_comment(&c);
        assert_eq!(draft.path, "src/a.rs");
        assert_eq!(draft.line, 5);
        assert_eq!(draft.side, GhSide::Right);
        assert!(draft.start_line.is_none());
        assert!(draft.start_side.is_none());
        assert_eq!(draft.body, "why?");
    }

    #[test]
    fn map_comment_range_old_side() {
        let c = comment("a.rs", Side::Old, 3, 7, Some("sha"), "range");
        let draft = map_comment(&c);
        assert_eq!(draft.start_line, Some(3));
        assert_eq!(draft.start_side, Some(GhSide::Left));
        assert_eq!(draft.line, 7);
        assert_eq!(draft.side, GhSide::Left);
    }

    #[test]
    fn map_comment_normalizes_backslash_paths() {
        let c = comment(r"src\windows\a.rs", Side::New, 1, 1, None, "hi");
        let draft = map_comment(&c);
        assert_eq!(draft.path, "src/windows/a.rs");
    }

    #[test]
    fn map_comment_flattens_replies_as_blockquotes() {
        let mut c = comment("a.rs", Side::New, 1, 1, None, "main body");
        c.replies.push(Reply {
            id: "p-1".to_string(),
            body: "a reply".to_string(),
            author: "agent".to_string(),
            created_ms: 0,
        });
        c.replies.push(Reply {
            id: "p-2".to_string(),
            body: "second reply".to_string(),
            author: "kyle".to_string(),
            created_ms: 0,
        });
        let draft = map_comment(&c);
        assert_eq!(
            draft.body,
            "main body\n\n> **@agent:** a reply\n\n> **@kyle:** second reply"
        );
    }

    /// A blank line inside a reply body used to end the markdown
    /// blockquote early (only the first line was ever prefixed with
    /// `> `), so a later paragraph rendered as the top-level author's own
    /// words instead of part of the quoted reply.
    #[test]
    fn map_comment_flattens_multi_paragraph_reply_with_full_blockquote() {
        let mut c = comment("a.rs", Side::New, 1, 1, None, "main body");
        c.replies.push(Reply {
            id: "p-1".to_string(),
            body: "first paragraph\n\nsecond paragraph\nstill second".to_string(),
            author: "agent".to_string(),
            created_ms: 0,
        });
        let draft = map_comment(&c);
        assert_eq!(
            draft.body,
            "main body\n\n> **@agent:** first paragraph\n>\n> second paragraph\n> still second"
        );
    }

    // --- writeback_failure_message ------------------------------------------

    /// The one message a duplicate-review bug hides behind: if this ever
    /// stops saying GitHub already has the review, a retry after a local
    /// writeback failure creates a second review on the PR.
    #[test]
    fn writeback_failure_message_names_github_success_and_warns_against_retrying() {
        let submitted = SubmittedReview {
            id: 999,
            html_url: "https://github.com/o/r/pull/42#pullrequestreview-999".to_string(),
            state: "COMMENTED".to_string(),
        };
        let msg = writeback_failure_message(42, &submitted, "disk full");

        assert!(msg.contains("ACCEPTED"), "message: {msg}");
        assert!(msg.contains("999"), "message: {msg}");
        assert!(
            msg.contains("https://github.com/o/r/pull/42#pullrequestreview-999"),
            "message: {msg}"
        );
        assert!(msg.contains("PR #42"), "message: {msg}");
        assert!(msg.contains("disk full"), "message: {msg}");
        assert!(
            msg.to_lowercase().contains("duplicate"),
            "message must warn about duplicating the review: {msg}"
        );
        assert!(
            msg.contains("Do NOT re-run"),
            "message must tell the caller not to retry: {msg}"
        );
    }

    // --- validate_submission: real temp repo ----------------------------

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestRepo {
        dir: PathBuf,
    }

    impl TestRepo {
        fn new(name: &str) -> Self {
            let pid = std::process::id();
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("dv-submit-test-{pid}-{n}-{name}"));
            std::fs::create_dir_all(&dir).expect("create test repo dir");
            let repo = Self { dir };
            repo.git(&["-c", "init.defaultBranch=main", "init"]);
            repo
        }

        fn path(&self) -> &Path {
            &self.dir
        }

        fn git(&self, args: &[&str]) -> String {
            let dir_str = self.dir.to_str().expect("temp dir path is not valid UTF-8");
            let mut full_args = vec![
                "-C",
                dir_str,
                "-c",
                "user.name=dv-test",
                "-c",
                "user.email=dv@test",
                "-c",
                "core.autocrlf=false",
                "-c",
                "commit.gpgsign=false",
            ];
            full_args.extend_from_slice(args);
            let output = Command::new("git")
                .args(&full_args)
                .output()
                .expect("failed to spawn git");
            if !output.status.success() {
                panic!(
                    "git {:?} failed:\n{}",
                    args,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string()
        }

        fn write(&self, rel_path: &str, bytes: &[u8]) {
            let full = self.dir.join(rel_path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("create parent dir");
            }
            std::fs::write(full, bytes).expect("write fixture file");
        }

        fn commit(&self, msg: &str) -> String {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-m", msg]);
            self.git(&["rev-parse", "HEAD"])
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Exercises every violation kind `validate_submission` is meant to
    /// catch, plus the ok-case passing cleanly, in one temp repo — also
    /// covers rename-awareness (Phase-3 round-2 review): a file renamed
    /// *and* edited between base and head, which used to defeat both the
    /// anchor check (old-side blob looked up under the wrong — new —
    /// path) and the line-in-diff check (diffing "missing at base" vs
    /// "full content at head" made every line of the new file look
    /// added, so a comment far outside the real edited hunk used to
    /// false-pass).
    #[test]
    fn validate_submission_catches_every_violation_kind_and_passes_ok_case() {
        let repo = TestRepo::new("validate");
        let base_lines: Vec<String> = (1..=12).map(|i| format!("line {i}\n")).collect();
        repo.write("a.rs", base_lines.concat().as_bytes());
        repo.write("untouched.rs", b"same on both sides\n");
        let renamed_base_lines: Vec<String> =
            (1..=12).map(|i| format!("renamed line {i}\n")).collect();
        repo.write("renamed_file.rs", renamed_base_lines.concat().as_bytes());
        let base_sha = repo.commit("base");

        let mut head_lines = base_lines.clone();
        head_lines[5] = "line 6 CHANGED\n".to_string(); // 1-based line 6
        repo.write("a.rs", head_lines.concat().as_bytes());
        let mut renamed_head_lines = renamed_base_lines.clone();
        renamed_head_lines[1] = "renamed line 2 CHANGED\n".to_string(); // 1-based line 2
        std::fs::remove_file(repo.path().join("renamed_file.rs")).expect("remove old name");
        repo.write(
            "renamed_file_new.rs",
            renamed_head_lines.concat().as_bytes(),
        );
        let head_sha = repo.commit("head");

        let git_repo =
            GitRepo::open(RepoLocation::Local(repo.path().to_path_buf())).expect("open repo");

        let correct_sha = git_repo
            .blob_sha(&BlobSpec::Rev {
                rev: head_sha.clone(),
                path: "a.rs".to_string(),
            })
            .expect("blob_sha")
            .expect("a.rs exists at head");
        let untouched_sha = git_repo
            .blob_sha(&BlobSpec::Rev {
                rev: base_sha.clone(),
                path: "untouched.rs".to_string(),
            })
            .expect("blob_sha")
            .expect("untouched.rs exists at base");
        let renamed_old_sha = git_repo
            .blob_sha(&BlobSpec::Rev {
                rev: base_sha.clone(),
                path: "renamed_file.rs".to_string(),
            })
            .expect("blob_sha")
            .expect("renamed_file.rs exists at base under its old name");
        let renamed_new_sha = git_repo
            .blob_sha(&BlobSpec::Rev {
                rev: head_sha.clone(),
                path: "renamed_file_new.rs".to_string(),
            })
            .expect("blob_sha")
            .expect("renamed_file_new.rs exists at head");

        // ok: correctly anchored, line 6 is within the diff's hunk.
        let ok = comment("a.rs", Side::New, 6, 6, Some(&correct_sha), "ok");
        // stale: blob_sha doesn't match the real content anymore.
        let stale = comment(
            "a.rs",
            Side::New,
            6,
            6,
            Some("0000000000000000000000000000000000000000"),
            "stale",
        );
        // unanchored: no blob_sha recorded at all.
        let unanchored = comment("untouched.rs", Side::Old, 1, 1, None, "unanchored");
        // missing file: path doesn't exist at either oid.
        let missing = comment(
            "does-not-exist.rs",
            Side::New,
            1,
            1,
            Some("deadbeef"),
            "missing",
        );
        // out-of-hunk: correctly anchored, but this file has no diff at
        // all, so none of its lines are part of the PR.
        let out_of_hunk = comment(
            "untouched.rs",
            Side::Old,
            1,
            1,
            Some(&untouched_sha),
            "oohunk",
        );
        // (a) new-side comment on a renamed file, inside the real edited
        // hunk (line 2) — must pass.
        let rename_new_in_hunk = comment(
            "renamed_file_new.rs",
            Side::New,
            2,
            2,
            Some(&renamed_new_sha),
            "rename-new-in-hunk",
        );
        // (b) new-side comment on a renamed file, outside the real edited
        // hunk (line 10 is untouched by the edit) — before this fix, the
        // rename made the whole new file look added, so this used to
        // false-pass; must now be flagged as not-in-diff.
        let rename_new_out_of_hunk = comment(
            "renamed_file_new.rs",
            Side::New,
            10,
            10,
            Some(&renamed_new_sha),
            "rename-new-out-of-hunk",
        );
        // (c) old-side comment on the renamed file, anchored (via
        // `old_path`) to the correct pre-rename blob — must pass.
        let rename_old_correct = comment(
            "renamed_file_new.rs",
            Side::Old,
            2,
            2,
            Some(&renamed_old_sha),
            "rename-old-correct",
        );

        let comments = vec![
            &ok,
            &stale,
            &unanchored,
            &missing,
            &out_of_hunk,
            &rename_new_in_hunk,
            &rename_new_out_of_hunk,
            &rename_old_correct,
        ];
        let violations = validate_submission(&git_repo, &base_sha, &head_sha, &comments)
            .expect("validation should run without error");

        assert!(
            !violations.iter().any(|v| v.comment_id == ok.id),
            "ok case must not be flagged: {violations:#?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.comment_id == stale.id && v.kind == ViolationKind::StaleAnchor),
            "{violations:#?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.comment_id == unanchored.id && v.kind == ViolationKind::Unanchored),
            "{violations:#?}"
        );
        assert!(
            violations.iter().any(|v| v.comment_id == missing.id),
            "{violations:#?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.comment_id == out_of_hunk.id && v.kind == ViolationKind::NotInDiff),
            "{violations:#?}"
        );
        assert!(
            !violations
                .iter()
                .any(|v| v.comment_id == rename_new_in_hunk.id),
            "rename (a) in-hunk new-side comment must not be flagged: {violations:#?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.comment_id == rename_new_out_of_hunk.id
                    && v.kind == ViolationKind::NotInDiff),
            "rename (b) out-of-hunk new-side comment must be flagged: {violations:#?}"
        );
        assert!(
            !violations
                .iter()
                .any(|v| v.comment_id == rename_old_correct.id),
            "rename (c) correctly old_path-anchored comment must not be flagged: {violations:#?}"
        );
    }

    // --- build_submission -------------------------------------------------

    /// A bare draft [`Review`] for `build_submission` tests — every field
    /// is `pub`, but `Review::new_draft` itself is `pub(crate)` to dv-core
    /// (only [`ReviewStore::create`] is meant to mint one outside the
    /// crate), so tests here build the struct literal directly instead.
    fn draft_review() -> Review {
        Review {
            v: dv_core::review::SCHEMA_VERSION,
            id: "r-test-0000000000000-0000".to_string(),
            source: DiffSource::WorkingTree,
            state: ReviewState::Draft,
            created_ms: 0,
            updated_ms: 0,
            comments: Vec::new(),
            remote: None,
        }
    }

    #[test]
    fn build_submission_blocks_on_violations_and_reports_them() {
        let repo = TestRepo::new("build-submission-blocked");
        repo.write("a.rs", b"one\ntwo\nthree\n");
        let base_sha = repo.commit("base");
        repo.write("a.rs", b"one\ntwo\nCHANGED\n");
        let head_sha = repo.commit("head");

        let git_repo =
            GitRepo::open(RepoLocation::Local(repo.path().to_path_buf())).expect("open repo");

        let mut review = draft_review();
        review
            .add_comment(
                "a.rs",
                Side::New,
                3,
                3,
                Some("0000000000000000000000000000000000000000".to_string()),
                "stale",
                "kyle",
            )
            .expect("add comment");

        let outcome = build_submission(
            &git_repo,
            &base_sha,
            &head_sha,
            &review,
            Verdict::Comment,
            String::new(),
            false,
        )
        .expect("build_submission should not hard-error");
        match outcome {
            SubmissionOutcome::Blocked(violations) => {
                assert_eq!(violations.len(), 1);
                assert_eq!(violations[0].kind, ViolationKind::StaleAnchor);
            }
            SubmissionOutcome::Ready(_) => panic!("expected Blocked, got Ready"),
        }
    }

    #[test]
    fn build_submission_ready_case_maps_open_comments_only() {
        let repo = TestRepo::new("build-submission-ready");
        repo.write("a.rs", b"one\ntwo\nthree\n");
        let base_sha = repo.commit("base");
        repo.write("a.rs", b"one\ntwo\nCHANGED\n");
        let head_sha = repo.commit("head");

        let git_repo =
            GitRepo::open(RepoLocation::Local(repo.path().to_path_buf())).expect("open repo");
        let sha = git_repo
            .blob_sha(&BlobSpec::Rev {
                rev: head_sha.clone(),
                path: "a.rs".to_string(),
            })
            .expect("blob_sha")
            .expect("a.rs exists at head");

        let mut review = draft_review();
        let id = review
            .add_comment("a.rs", Side::New, 3, 3, Some(sha), "looks off", "kyle")
            .expect("add comment")
            .id
            .clone();
        review
            .set_status(&id, CommentStatus::Resolved)
            .expect("resolve");

        let outcome = build_submission(
            &git_repo,
            &base_sha,
            &head_sha,
            &review,
            Verdict::Approve,
            "ship it".to_string(),
            false,
        )
        .expect("build_submission should not hard-error");
        match outcome {
            SubmissionOutcome::Ready(submission) => {
                assert_eq!(submission.comments.len(), 0, "resolved comment excluded");
                assert_eq!(submission.body, "ship it");
                assert_eq!(submission.event, ReviewEvent::Approve);
                assert_eq!(submission.commit_id, head_sha);
            }
            SubmissionOutcome::Blocked(violations) => {
                panic!("expected Ready, got Blocked: {violations:#?}")
            }
        }
    }

    // --- build_submission: nothing-to-submit guard (review finding P2-2) --
    //
    // The CLI used to enforce this rule itself, outside this shared core
    // (`cli/pr_cmd.rs::cmd_review_submit`, pre-move); the GUI's submit flow
    // let it through to a live GitHub 422. Covered here once so neither
    // frontend can drift: a "GUI-shaped" call (no comments at all, empty
    // `body` — matching `workspace.rs::start_submit_validation`'s always-
    // `String::new()` body) and a "CLI-shaped" call (a comment that only
    // becomes "nothing" once resolved-and-filtered, a whitespace-only
    // `--body` — matching `cmd_review_submit`'s own `open_comment_count`/
    // `body_text` reasoning) must both block identically.

    #[test]
    fn build_submission_blocks_nothing_to_submit_gui_shaped() {
        let repo = TestRepo::new("nothing-to-submit-gui");
        repo.write("a.rs", b"one\ntwo\nthree\n");
        let base_sha = repo.commit("base");
        repo.write("a.rs", b"one\ntwo\nCHANGED\n");
        let head_sha = repo.commit("head");
        let git_repo =
            GitRepo::open(RepoLocation::Local(repo.path().to_path_buf())).expect("open repo");

        // No comments at all, empty body (the GUI always passes
        // `String::new()`), plain Comment verdict.
        let review = draft_review();
        let outcome = build_submission(
            &git_repo,
            &base_sha,
            &head_sha,
            &review,
            Verdict::Comment,
            String::new(),
            false,
        )
        .expect("build_submission should not hard-error");
        match outcome {
            SubmissionOutcome::Blocked(violations) => {
                assert_eq!(violations.len(), 1, "{violations:#?}");
                assert_eq!(violations[0].kind, ViolationKind::NothingToSubmit);
                assert!(
                    violations[0].message.contains("nothing to submit"),
                    "message: {}",
                    violations[0].message
                );
            }
            SubmissionOutcome::Ready(_) => panic!("expected Blocked, got Ready"),
        }
    }

    #[test]
    fn build_submission_blocks_nothing_to_submit_cli_shaped() {
        let repo = TestRepo::new("nothing-to-submit-cli");
        repo.write("a.rs", b"one\ntwo\nthree\n");
        let base_sha = repo.commit("base");
        repo.write("a.rs", b"one\ntwo\nCHANGED\n");
        let head_sha = repo.commit("head");
        let git_repo =
            GitRepo::open(RepoLocation::Local(repo.path().to_path_buf())).expect("open repo");

        // One comment, still Open, plus a whitespace-only `--body`
        // (trimmed to empty by the guard) — must NOT block, since an open
        // comment is something to submit.
        let mut review = draft_review();
        let sha = git_repo
            .blob_sha(&BlobSpec::Rev {
                rev: head_sha.clone(),
                path: "a.rs".to_string(),
            })
            .expect("blob_sha")
            .expect("a.rs exists at head");
        review
            .add_comment("a.rs", Side::New, 3, 3, Some(sha), "nit", "kyle")
            .expect("add comment");
        let outcome = build_submission(
            &git_repo,
            &base_sha,
            &head_sha,
            &review,
            Verdict::Comment,
            "   ".to_string(),
            false,
        )
        .expect("build_submission should not hard-error");
        assert!(
            matches!(outcome, SubmissionOutcome::Ready(_)),
            "an open comment must not trip the nothing-to-submit guard"
        );

        // Now resolve it (without `include_resolved`) — filtered down to
        // zero comments, matching the CLI's own pre-move
        // `open_comment_count` reasoning — and it must block.
        let id = review.comments[0].id.clone();
        review
            .set_status(&id, CommentStatus::Resolved)
            .expect("resolve");
        let outcome = build_submission(
            &git_repo,
            &base_sha,
            &head_sha,
            &review,
            Verdict::Comment,
            "   ".to_string(),
            false,
        )
        .expect("build_submission should not hard-error");
        match outcome {
            SubmissionOutcome::Blocked(violations) => {
                assert_eq!(violations.len(), 1, "{violations:#?}");
                assert_eq!(violations[0].kind, ViolationKind::NothingToSubmit);
            }
            SubmissionOutcome::Ready(_) => panic!("expected Blocked, got Ready"),
        }
    }

    #[test]
    fn build_submission_allows_approve_with_no_comments_and_no_body() {
        // A plain "Looks good, nothing to add" Approve (or RequestChanges) with
        // no comments and no body is a legitimate review — only the bare
        // `Comment` verdict is meaningless with nothing attached to it.
        let repo = TestRepo::new("approve-empty-is-fine");
        repo.write("a.rs", b"one\ntwo\nthree\n");
        let base_sha = repo.commit("base");
        repo.write("a.rs", b"one\ntwo\nCHANGED\n");
        let head_sha = repo.commit("head");
        let git_repo =
            GitRepo::open(RepoLocation::Local(repo.path().to_path_buf())).expect("open repo");

        let review = draft_review();
        let outcome = build_submission(
            &git_repo,
            &base_sha,
            &head_sha,
            &review,
            Verdict::Approve,
            String::new(),
            false,
        )
        .expect("build_submission should not hard-error");
        assert!(
            matches!(outcome, SubmissionOutcome::Ready(_)),
            "Approve with no comments/body must not trip the nothing-to-submit guard"
        );
    }

    #[test]
    fn build_submission_allows_plain_comment_with_body_text_and_no_comments() {
        let repo = TestRepo::new("comment-with-body-is-fine");
        repo.write("a.rs", b"one\ntwo\nthree\n");
        let base_sha = repo.commit("base");
        repo.write("a.rs", b"one\ntwo\nCHANGED\n");
        let head_sha = repo.commit("head");
        let git_repo =
            GitRepo::open(RepoLocation::Local(repo.path().to_path_buf())).expect("open repo");

        let review = draft_review();
        let outcome = build_submission(
            &git_repo,
            &base_sha,
            &head_sha,
            &review,
            Verdict::Comment,
            "just a general note, no inline comments".to_string(),
            false,
        )
        .expect("build_submission should not hard-error");
        assert!(
            matches!(outcome, SubmissionOutcome::Ready(_)),
            "a plain Comment with summary body text must not trip the nothing-to-submit guard"
        );
    }
}
