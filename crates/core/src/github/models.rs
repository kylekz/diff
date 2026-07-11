//! Data shapes exchanged with `gh`: PR metadata read from `gh pr list`/`gh
//! pr view --json ...`, and the review-submission/PR-creation request
//! bodies sent back out. Parsing lives here rather than in `client.rs` so
//! it's testable against hand-written fixture strings with no `gh` binary
//! and no network involved.

use serde::{Deserialize, Serialize};

use super::error::GhError;

/// A PR's open/closed/merged state, as `gh`'s `state` JSON field
/// (`OPEN`/`CLOSED`/`MERGED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

/// The repo's aggregate review verdict on a PR, as `gh`'s `reviewDecision`
/// field. Absent (empty string, or the field missing entirely) means no
/// reviews are required or none have been submitted — modeled as
/// `Option<ReviewDecision>` on the containing structs rather than a fourth
/// variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// Derived (never deserialized directly) rollup of a PR's `statusCheckRollup`
/// entries — see [`derive_checks_summary`] for the precedence rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksSummary {
    Passing,
    Failing,
    Pending,
    /// No checks are configured/reported at all — distinct from `Pending`
    /// (checks exist but haven't finished) so the UI can render "no CI"
    /// differently from "CI running".
    None,
}

/// One entry from `gh pr list --json number,title,author,headRefName,
/// isDraft,updatedAt`.
#[derive(Debug, Clone, Serialize)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub head_ref: String,
    pub is_draft: bool,
    pub updated_at: String,
}

/// `gh pr view <n> --json number,title,body,url,state,isDraft,baseRefName,
/// headRefName,baseRefOid,headRefOid,reviewDecision,statusCheckRollup,
/// author` — everything the PR header panel needs in one call.
#[derive(Debug, Clone, Serialize)]
pub struct PrMeta {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub state: PrState,
    pub is_draft: bool,
    pub base_ref: String,
    pub head_ref: String,
    pub base_oid: String,
    pub head_oid: String,
    pub author: String,
    pub review_decision: Option<ReviewDecision>,
    pub checks: ChecksSummary,
}

/// The lighter `gh pr view <n> --json state,isDraft,reviewDecision,
/// statusCheckRollup` used to refresh sidebar status icons without paying
/// for the full [`PrMeta`] payload.
#[derive(Debug, Clone, Serialize)]
pub struct PrStatus {
    pub state: PrState,
    pub is_draft: bool,
    pub review_decision: Option<ReviewDecision>,
    pub checks: ChecksSummary,
}

#[derive(Deserialize)]
struct RawAuthor {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPrSummary {
    number: u64,
    title: String,
    author: RawAuthor,
    head_ref_name: String,
    is_draft: bool,
    updated_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPrMeta {
    number: u64,
    title: String,
    #[serde(default)]
    body: String,
    url: String,
    state: PrState,
    is_draft: bool,
    base_ref_name: String,
    head_ref_name: String,
    base_ref_oid: String,
    head_ref_oid: String,
    author: RawAuthor,
    /// `gh`'s GraphQL-backed JSON prints `""` (not the field omitted) when
    /// there's no decision, so this is a plain string, mapped to `None` by
    /// [`parse_review_decision`] for both `""` and any value this build
    /// doesn't recognize (forward-compat: an unknown decision string is not
    /// a parse error).
    #[serde(default)]
    review_decision: String,
    /// `Option`, not a bare `Vec` — `#[serde(default)]` only covers a
    /// *missing* field, but `gh` prints an explicit JSON `null` here (not
    /// `[]`) when a PR has zero commits (e.g. right after a force-push),
    /// which would otherwise be a hard parse error. `unwrap_or_default()`
    /// at the call site folds both "missing" and "null" into an empty
    /// rollup.
    #[serde(default)]
    status_check_rollup: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPrStatus {
    state: PrState,
    is_draft: bool,
    #[serde(default)]
    review_decision: String,
    #[serde(default)]
    status_check_rollup: Option<Vec<serde_json::Value>>,
}

fn parse_review_decision(raw: &str) -> Option<ReviewDecision> {
    match raw {
        "APPROVED" => Some(ReviewDecision::Approved),
        "CHANGES_REQUESTED" => Some(ReviewDecision::ChangesRequested),
        "REVIEW_REQUIRED" => Some(ReviewDecision::ReviewRequired),
        _ => None,
    }
}

/// One check's pass/fail/pending classification, folded into an overall
/// [`ChecksSummary`] by [`derive_checks_summary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckState {
    Passing,
    Failing,
    Pending,
}

/// Classify one `statusCheckRollup` entry. GitHub's GraphQL API returns a
/// union of two shapes here, discriminated by `__typename`:
///
/// - `CheckRun` (a GitHub Actions / Checks API run): `status` is
///   `QUEUED`/`IN_PROGRESS`/`COMPLETED`; only once `COMPLETED` does
///   `conclusion` (`SUCCESS`/`FAILURE`/`NEUTRAL`/`CANCELLED`/`SKIPPED`/
///   `TIMED_OUT`/`ACTION_REQUIRED`/`STALE`/...) mean anything.
/// - `StatusContext` (a legacy commit status): `state` is
///   `SUCCESS`/`ERROR`/`FAILURE`/`PENDING`/`EXPECTED`.
///
/// Untyped `serde_json::Value` rather than a tagged enum deliberately: a
/// tagged enum errors on an unrecognized `__typename`, and GitHub adding a
/// third rollup shape someday must not turn this into a parse error —
/// treat anything unrecognized (unknown `__typename`, or fields missing
/// their expected shape) as `Pending` instead.
fn classify_check(value: &serde_json::Value) -> CheckState {
    let typename = value.get("__typename").and_then(|v| v.as_str());
    match typename {
        Some("CheckRun") => {
            let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status != "COMPLETED" {
                return CheckState::Pending;
            }
            match value.get("conclusion").and_then(|v| v.as_str()) {
                Some("SUCCESS" | "NEUTRAL" | "SKIPPED") => CheckState::Passing,
                Some(_) => CheckState::Failing,
                None => CheckState::Pending,
            }
        }
        Some("StatusContext") => match value.get("state").and_then(|v| v.as_str()) {
            Some("SUCCESS") => CheckState::Passing,
            Some("ERROR" | "FAILURE") => CheckState::Failing,
            _ => CheckState::Pending,
        },
        _ => CheckState::Pending,
    }
}

/// Fold a PR's `statusCheckRollup` array into one summary: any failing
/// check wins outright (`Failing`), else any still-pending check makes it
/// `Pending`, else (a non-empty rollup with nothing failing or pending)
/// it's `Passing`; an empty rollup is `None` (no checks configured at all).
pub fn derive_checks_summary(rollup: &[serde_json::Value]) -> ChecksSummary {
    if rollup.is_empty() {
        return ChecksSummary::None;
    }
    let mut any_pending = false;
    for entry in rollup {
        match classify_check(entry) {
            CheckState::Failing => return ChecksSummary::Failing,
            CheckState::Pending => any_pending = true,
            CheckState::Passing => {}
        }
    }
    if any_pending {
        ChecksSummary::Pending
    } else {
        ChecksSummary::Passing
    }
}

fn invalid(context: &str, err: serde_json::Error) -> GhError {
    GhError::InvalidResponse {
        detail: format!("parsing {context}: {err}"),
    }
}

impl PrSummary {
    pub(super) fn parse_list(bytes: &[u8]) -> Result<Vec<PrSummary>, GhError> {
        let raw: Vec<RawPrSummary> =
            serde_json::from_slice(bytes).map_err(|e| invalid("gh pr list output", e))?;
        Ok(raw.into_iter().map(Into::into).collect())
    }
}

impl From<RawPrSummary> for PrSummary {
    fn from(raw: RawPrSummary) -> Self {
        PrSummary {
            number: raw.number,
            title: raw.title,
            author: raw.author.login,
            head_ref: raw.head_ref_name,
            is_draft: raw.is_draft,
            updated_at: raw.updated_at,
        }
    }
}

impl PrMeta {
    pub(super) fn parse(bytes: &[u8]) -> Result<PrMeta, GhError> {
        let raw: RawPrMeta =
            serde_json::from_slice(bytes).map_err(|e| invalid("gh pr view output", e))?;
        Ok(PrMeta {
            number: raw.number,
            title: raw.title,
            body: raw.body,
            url: raw.url,
            state: raw.state,
            is_draft: raw.is_draft,
            base_ref: raw.base_ref_name,
            head_ref: raw.head_ref_name,
            base_oid: raw.base_ref_oid,
            head_oid: raw.head_ref_oid,
            author: raw.author.login,
            review_decision: parse_review_decision(&raw.review_decision),
            checks: derive_checks_summary(&raw.status_check_rollup.unwrap_or_default()),
        })
    }
}

impl PrStatus {
    pub(super) fn parse(bytes: &[u8]) -> Result<PrStatus, GhError> {
        let raw: RawPrStatus =
            serde_json::from_slice(bytes).map_err(|e| invalid("gh pr view output", e))?;
        Ok(PrStatus {
            state: raw.state,
            is_draft: raw.is_draft,
            review_decision: parse_review_decision(&raw.review_decision),
            checks: derive_checks_summary(&raw.status_check_rollup.unwrap_or_default()),
        })
    }
}

// ---------------------------------------------------------------------
// Review submission
// ---------------------------------------------------------------------

/// Which side of the diff a review comment's line number refers to, as
/// GitHub's REST API `side`/`start_side` fields (`LEFT`/`RIGHT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GhSide {
    Left,
    Right,
}

/// One inline review comment bound for
/// `POST /repos/{owner}/{repo}/pulls/{n}/reviews`'s `comments[]`.
///
/// **Single-line vs. range is encoded by `start_line`/`start_side`, not a
/// separate flag**: GitHub 422s if `start_line == line`, so a single-line
/// comment must omit them entirely (`#[serde(skip_serializing_if)]` below)
/// rather than send `start_line` equal to `line`. Prefer the
/// [`DraftComment::single_line`]/[`DraftComment::range`] constructors over
/// building this by hand — they enforce that shape.
#[derive(Debug, Clone, Serialize)]
pub struct DraftComment {
    /// Forward-slash relative path, matching [`crate::ChangedFile::path`].
    pub path: String,
    pub body: String,
    /// The diff line this comment (or, for a range, its *end*) anchors to.
    pub line: u64,
    pub side: GhSide,
    /// `Some` only for a range comment; strictly `< line` when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<GhSide>,
}

impl DraftComment {
    /// A comment anchored to a single line.
    pub fn single_line(
        path: impl Into<String>,
        body: impl Into<String>,
        line: u64,
        side: GhSide,
    ) -> Self {
        Self {
            path: path.into(),
            body: body.into(),
            line,
            side,
            start_line: None,
            start_side: None,
        }
    }

    /// A comment spanning `start_line..=line`. GitHub 422s if
    /// `start_line == line` (must be single-line instead) or if
    /// `start_line > line` (an inverted range) — rather than document that
    /// as a caller obligation, enforce it here: an equal pair collapses to
    /// [`Self::single_line`], and an inverted pair is swapped so
    /// `start_line < line` always holds in the constructed value.
    pub fn range(
        path: impl Into<String>,
        body: impl Into<String>,
        start_line: u64,
        start_side: GhSide,
        line: u64,
        side: GhSide,
    ) -> Self {
        let path = path.into();
        let body = body.into();
        if start_line == line {
            return Self::single_line(path, body, line, side);
        }
        let (start_line, start_side, line, side) = if start_line > line {
            (line, side, start_line, start_side)
        } else {
            (start_line, start_side, line, side)
        };
        Self {
            path,
            body,
            line,
            side,
            start_line: Some(start_line),
            start_side: Some(start_side),
        }
    }
}

/// The verdict half of a review submission, as GitHub's REST `event` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

/// The full request body for
/// `POST /repos/{owner}/{repo}/pulls/{n}/reviews`.
///
/// `commit_id` is required (not optional, unlike GitHub's own API where
/// it's technically optional): it must be the PR's current `headRefOid`,
/// or a comment can silently anchor to the wrong commit if the PR has
/// moved since the local review was drafted.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewSubmission {
    pub commit_id: String,
    pub body: String,
    pub event: ReviewEvent,
    pub comments: Vec<DraftComment>,
}

/// Parsed from the `{ id, html_url, state }` GitHub returns on a
/// successful review submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmittedReview {
    pub id: u64,
    pub html_url: String,
    pub state: String,
}

#[derive(Deserialize)]
struct RawSubmittedReview {
    id: u64,
    html_url: String,
    state: String,
}

impl SubmittedReview {
    pub(super) fn parse(bytes: &[u8]) -> Result<SubmittedReview, GhError> {
        let raw: RawSubmittedReview =
            serde_json::from_slice(bytes).map_err(|e| invalid("gh api reviews response", e))?;
        Ok(SubmittedReview {
            id: raw.id,
            html_url: raw.html_url,
            state: raw.state,
        })
    }
}

// ---------------------------------------------------------------------
// PR creation
// ---------------------------------------------------------------------

/// Request to `gh pr create`.
#[derive(Debug, Clone)]
pub struct CreatePr {
    pub title: String,
    pub body: String,
    /// `None` uses the repo's default branch (gh's own default when
    /// `--base` is omitted).
    pub base: Option<String>,
    pub draft: bool,
    /// `None` uses the current branch (gh's own default when `--head` is
    /// omitted); dv passes it explicitly once it already knows the
    /// caller's resolved branch name, to avoid relying on gh's cwd-based
    /// inference matching what dv thinks the current branch is.
    pub head: Option<String>,
}

/// The PR `gh pr create` just made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPr {
    pub number: u64,
    pub url: String,
}

impl CreatedPr {
    /// `gh pr create` (no `--json`) prints the created PR's URL as its
    /// last line of stdout; the number is the final `/`-separated segment.
    pub(super) fn parse_stdout(bytes: &[u8]) -> Result<CreatedPr, GhError> {
        let text = crate::command::decode_output(bytes);
        let url = text
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .ok_or_else(|| GhError::InvalidResponse {
                detail: "gh pr create printed no output".to_string(),
            })?
            .to_string();
        let number = url
            .rsplit('/')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| GhError::InvalidResponse {
                detail: format!("could not find a PR number in gh pr create output: {url:?}"),
            })?;
        Ok(CreatedPr { number, url })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_run(status: &str, conclusion: Option<&str>) -> serde_json::Value {
        let mut obj = serde_json::json!({
            "__typename": "CheckRun",
            "status": status,
        });
        if let Some(c) = conclusion {
            obj["conclusion"] = serde_json::Value::String(c.to_string());
        }
        obj
    }

    fn status_context(state: &str) -> serde_json::Value {
        serde_json::json!({ "__typename": "StatusContext", "state": state })
    }

    // --- ChecksSummary derivation ---------------------------------------

    #[test]
    fn checks_empty_is_none() {
        assert_eq!(derive_checks_summary(&[]), ChecksSummary::None);
    }

    #[test]
    fn checks_all_pass_is_passing() {
        let rollup = vec![
            check_run("COMPLETED", Some("SUCCESS")),
            status_context("SUCCESS"),
        ];
        assert_eq!(derive_checks_summary(&rollup), ChecksSummary::Passing);
    }

    #[test]
    fn checks_any_fail_is_failing() {
        let rollup = vec![
            check_run("COMPLETED", Some("SUCCESS")),
            check_run("COMPLETED", Some("FAILURE")),
        ];
        assert_eq!(derive_checks_summary(&rollup), ChecksSummary::Failing);
    }

    #[test]
    fn checks_any_pending_is_pending_when_none_failing() {
        let rollup = vec![
            check_run("COMPLETED", Some("SUCCESS")),
            check_run("IN_PROGRESS", None),
        ];
        assert_eq!(derive_checks_summary(&rollup), ChecksSummary::Pending);
    }

    #[test]
    fn checks_fail_beats_pending() {
        let rollup = vec![
            check_run("IN_PROGRESS", None),
            check_run("COMPLETED", Some("FAILURE")),
        ];
        assert_eq!(derive_checks_summary(&rollup), ChecksSummary::Failing);
    }

    #[test]
    fn checks_unknown_typename_is_pending_not_error() {
        let rollup = vec![serde_json::json!({ "__typename": "SomethingNew" })];
        assert_eq!(derive_checks_summary(&rollup), ChecksSummary::Pending);
    }

    #[test]
    fn checks_neutral_and_skipped_count_as_passing() {
        let rollup = vec![
            check_run("COMPLETED", Some("NEUTRAL")),
            check_run("COMPLETED", Some("SKIPPED")),
        ];
        assert_eq!(derive_checks_summary(&rollup), ChecksSummary::Passing);
    }

    // --- PrMeta / PrSummary / PrStatus JSON parsing ---------------------

    #[test]
    fn parses_pr_meta_fixture() {
        let fixture = r#"{
            "number": 7,
            "title": "Formatting overhaul",
            "body": "Reformats the whole project.",
            "url": "https://github.com/kylekz/difftest/pull/7",
            "state": "OPEN",
            "isDraft": false,
            "baseRefName": "main",
            "headRefName": "feature/formatting-overhaul",
            "baseRefOid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "headRefOid": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "reviewDecision": "REVIEW_REQUIRED",
            "statusCheckRollup": [
                {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"}
            ],
            "author": {"login": "kylekz"}
        }"#;
        let meta = PrMeta::parse(fixture.as_bytes()).unwrap();
        assert_eq!(meta.number, 7);
        assert_eq!(meta.title, "Formatting overhaul");
        assert_eq!(meta.state, PrState::Open);
        assert!(!meta.is_draft);
        assert_eq!(meta.base_ref, "main");
        assert_eq!(meta.head_ref, "feature/formatting-overhaul");
        assert_eq!(meta.author, "kylekz");
        assert_eq!(meta.review_decision, Some(ReviewDecision::ReviewRequired));
        assert_eq!(meta.checks, ChecksSummary::Passing);
    }

    #[test]
    fn parses_pr_meta_with_empty_review_decision_and_no_checks() {
        let fixture = r#"{
            "number": 1, "title": "t", "body": "", "url": "https://github.com/o/r/pull/1",
            "state": "MERGED", "isDraft": false,
            "baseRefName": "main", "headRefName": "feat",
            "baseRefOid": "a", "headRefOid": "b",
            "reviewDecision": "",
            "statusCheckRollup": [],
            "author": {"login": "someone"}
        }"#;
        let meta = PrMeta::parse(fixture.as_bytes()).unwrap();
        assert_eq!(meta.state, PrState::Merged);
        assert_eq!(meta.review_decision, None);
        assert_eq!(meta.checks, ChecksSummary::None);
    }

    #[test]
    fn parses_pr_meta_with_null_status_check_rollup() {
        // `gh` prints an explicit JSON `null` (not `[]`) for
        // `statusCheckRollup` when a PR has zero commits, e.g. right after
        // a force-push — must parse cleanly, not error.
        let fixture = r#"{
            "number": 2, "title": "t", "body": "", "url": "https://github.com/o/r/pull/2",
            "state": "OPEN", "isDraft": false,
            "baseRefName": "main", "headRefName": "feat",
            "baseRefOid": "a", "headRefOid": "b",
            "reviewDecision": "",
            "statusCheckRollup": null,
            "author": {"login": "someone"}
        }"#;
        let meta = PrMeta::parse(fixture.as_bytes()).unwrap();
        assert_eq!(meta.checks, ChecksSummary::None);
    }

    #[test]
    fn parses_pr_summary_list_fixture() {
        let fixture = r#"[
            {"number": 3, "title": "Add feature", "author": {"login": "kylekz"},
             "headRefName": "feature/x", "isDraft": true, "updatedAt": "2026-07-01T00:00:00Z"}
        ]"#;
        let list = PrSummary::parse_list(fixture.as_bytes()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].number, 3);
        assert_eq!(list[0].author, "kylekz");
        assert_eq!(list[0].head_ref, "feature/x");
        assert!(list[0].is_draft);
    }

    #[test]
    fn parses_pr_status_fixture() {
        let fixture = r#"{
            "state": "OPEN", "isDraft": false, "reviewDecision": "APPROVED",
            "statusCheckRollup": [{"__typename": "StatusContext", "state": "ERROR"}]
        }"#;
        let status = PrStatus::parse(fixture.as_bytes()).unwrap();
        assert_eq!(status.state, PrState::Open);
        assert_eq!(status.review_decision, Some(ReviewDecision::Approved));
        assert_eq!(status.checks, ChecksSummary::Failing);
    }

    #[test]
    fn parses_pr_status_with_null_status_check_rollup() {
        let fixture = r#"{
            "state": "OPEN", "isDraft": false, "reviewDecision": "",
            "statusCheckRollup": null
        }"#;
        let status = PrStatus::parse(fixture.as_bytes()).unwrap();
        assert_eq!(status.checks, ChecksSummary::None);
    }

    // --- ReviewSubmission serialization ----------------------------------

    #[test]
    fn single_line_comment_omits_start_fields() {
        let comment = DraftComment::single_line("src/a.rs", "hi", 10, GhSide::Right);
        let json = serde_json::to_value(&comment).unwrap();
        assert!(json.get("start_line").is_none());
        assert!(json.get("start_side").is_none());
        assert_eq!(json["line"], 10);
        assert_eq!(json["side"], "RIGHT");
    }

    #[test]
    fn range_comment_includes_start_fields() {
        let comment = DraftComment::range("src/a.rs", "hi", 5, GhSide::Left, 10, GhSide::Right);
        let json = serde_json::to_value(&comment).unwrap();
        assert_eq!(json["start_line"], 5);
        assert_eq!(json["start_side"], "LEFT");
        assert_eq!(json["line"], 10);
        assert_eq!(json["side"], "RIGHT");
    }

    #[test]
    fn range_comment_with_equal_bounds_collapses_to_single_line() {
        let comment = DraftComment::range("src/a.rs", "hi", 10, GhSide::Left, 10, GhSide::Right);
        let json = serde_json::to_value(&comment).unwrap();
        assert!(json.get("start_line").is_none());
        assert!(json.get("start_side").is_none());
        assert_eq!(json["line"], 10);
        assert_eq!(json["side"], "RIGHT");
    }

    #[test]
    fn range_comment_with_inverted_bounds_is_swapped() {
        // Caller passed (20, Left) as "start" and (5, Right) as "end" —
        // backwards. The constructor must swap them so `start_line < line`.
        let comment = DraftComment::range("src/a.rs", "hi", 20, GhSide::Left, 5, GhSide::Right);
        let json = serde_json::to_value(&comment).unwrap();
        assert_eq!(json["start_line"], 5);
        assert_eq!(json["start_side"], "RIGHT");
        assert_eq!(json["line"], 20);
        assert_eq!(json["side"], "LEFT");
    }

    #[test]
    fn review_event_serializes_as_documented() {
        assert_eq!(
            serde_json::to_string(&ReviewEvent::Comment).unwrap(),
            "\"COMMENT\""
        );
        assert_eq!(
            serde_json::to_string(&ReviewEvent::Approve).unwrap(),
            "\"APPROVE\""
        );
        assert_eq!(
            serde_json::to_string(&ReviewEvent::RequestChanges).unwrap(),
            "\"REQUEST_CHANGES\""
        );
    }

    #[test]
    fn review_submission_full_serialization_shape() {
        let submission = ReviewSubmission {
            commit_id: "deadbeef".to_string(),
            body: "Looks good overall".to_string(),
            event: ReviewEvent::RequestChanges,
            comments: vec![
                DraftComment::single_line("a.rs", "why?", 3, GhSide::Right),
                DraftComment::range("b.rs", "range comment", 1, GhSide::Left, 4, GhSide::Left),
            ],
        };
        let json = serde_json::to_value(&submission).unwrap();
        assert_eq!(json["commit_id"], "deadbeef");
        assert_eq!(json["event"], "REQUEST_CHANGES");
        assert_eq!(json["comments"].as_array().unwrap().len(), 2);
        assert!(json["comments"][0].get("start_line").is_none());
        assert_eq!(json["comments"][1]["start_line"], 1);
    }

    // --- SubmittedReview / CreatedPr parsing -----------------------------

    #[test]
    fn parses_submitted_review_fixture() {
        let fixture = r#"{"id": 123, "html_url": "https://github.com/o/r/pull/1#pullrequestreview-123", "state": "COMMENTED"}"#;
        let review = SubmittedReview::parse(fixture.as_bytes()).unwrap();
        assert_eq!(review.id, 123);
        assert_eq!(review.state, "COMMENTED");
    }

    #[test]
    fn parses_created_pr_from_stdout() {
        let stdout = b"https://github.com/kylekz/difftest/pull/9\n";
        let created = CreatedPr::parse_stdout(stdout).unwrap();
        assert_eq!(created.number, 9);
        assert_eq!(created.url, "https://github.com/kylekz/difftest/pull/9");
    }

    #[test]
    fn parses_created_pr_ignoring_trailing_blank_lines() {
        let stdout = b"https://github.com/kylekz/difftest/pull/10\n\n";
        let created = CreatedPr::parse_stdout(stdout).unwrap();
        assert_eq!(created.number, 10);
    }

    #[test]
    fn created_pr_errors_on_unparsable_output() {
        assert!(CreatedPr::parse_stdout(b"no url here\n").is_err());
        assert!(CreatedPr::parse_stdout(b"").is_err());
    }
}
