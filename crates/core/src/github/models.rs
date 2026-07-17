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
// Review threads (read-only GitHub sync, docs/phase-6-review-navigator.md
// deliverable 6). Doc-deviation #1: sourced from `gh api graphql`, not the
// two REST endpoints (`.../pulls/{n}/comments` + `.../reviews`) the phase
// doc names — REST doesn't expose thread-level `isResolved` at all; only
// GraphQL's `reviewThreads` connection does.
// ---------------------------------------------------------------------

/// One review thread as GitHub's GraphQL `reviewThreads` connection reports
/// it — read-only, rendered alongside dv's own local threads. Modeled with
/// its own `Raw*` structs below rather than reusing `PrState`/
/// `ReviewDecision`/`ChecksSummary`/etc.: GraphQL is a third wire
/// convention in this file (camelCase field names, and `diffSide` is
/// `SCREAMING_SNAKE_CASE` like [`GhSide`] but under a different field name
/// than the REST `side`/`start_side` this file already wraps).
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteThread {
    /// GraphQL node id — opaque identity, for keying/debugging only. NOT
    /// the mapping key back to a dv-submitted review; see
    /// `review_database_id`.
    pub id: String,
    pub is_resolved: bool,
    /// Forward-slash relative path, matching [`crate::ChangedFile::path`].
    pub path: String,
    /// `None` for a thread GitHub can no longer place on the current diff
    /// (its side went outdated).
    pub line: Option<u32>,
    pub side: GhSide,
    /// The opening comment first, any replies after, oldest first — same
    /// order GitHub returns them in.
    pub comments: Vec<RemoteComment>,
    /// The opening comment's `PullRequestReview.fullDatabaseId` — the
    /// REST-equivalent numeric id, i.e. exactly what
    /// [`crate::review::RemoteRef::submitted_review_id`] stores. NOT the
    /// GraphQL node id above. A thread's later replies can belong to a
    /// different review (or none — a plain conversation reply), so only
    /// the opening comment's review identifies "this is our submitted
    /// review's thread". `None` when the opening comment isn't part of any
    /// review.
    pub review_database_id: Option<u64>,
}

/// One comment inside a [`RemoteThread`] — read-only.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteComment {
    pub author: String,
    pub body: String,
    pub created_ms: u64,
}

/// `gh api graphql`'s response envelope: GraphQL always answers HTTP 200
/// (so `run_gh`'s exit-code check alone can't catch a query error) and
/// reports failures via a top-level `errors` array instead, with `data`
/// left null or partially populated — both must be checked explicitly
/// rather than just unwrapping `data`.
#[derive(Deserialize)]
struct RawGraphQlEnvelope {
    #[serde(default)]
    data: Option<RawGraphQlData>,
    #[serde(default)]
    errors: Option<Vec<RawGraphQlError>>,
}

#[derive(Deserialize)]
struct RawGraphQlError {
    message: String,
}

#[derive(Deserialize)]
struct RawGraphQlData {
    repository: Option<RawThreadsRepository>,
}

#[derive(Deserialize)]
struct RawThreadsRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<RawThreadsPullRequest>,
}

#[derive(Deserialize)]
struct RawThreadsPullRequest {
    #[serde(rename = "reviewThreads")]
    review_threads: RawReviewThreadConnection,
}

#[derive(Deserialize)]
struct RawReviewThreadConnection {
    nodes: Vec<RawReviewThread>,
}

#[derive(Deserialize)]
struct RawReviewThread {
    id: String,
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(rename = "diffSide", default)]
    diff_side: Option<String>,
    comments: RawThreadCommentConnection,
}

#[derive(Deserialize)]
struct RawThreadCommentConnection {
    nodes: Vec<RawThreadComment>,
}

#[derive(Deserialize)]
struct RawThreadComment {
    /// `null` for a deleted GitHub account — falls back to `"ghost"`
    /// (GitHub's own placeholder login for this case) in [`From`] below.
    #[serde(default)]
    author: Option<RawAuthor>,
    body: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "pullRequestReview", default)]
    pull_request_review: Option<RawThreadReviewRef>,
}

#[derive(Deserialize)]
struct RawThreadReviewRef {
    /// A JSON *string* on the wire, not a number — live-verified against
    /// `kylekz/difftest`: `fullDatabaseId` prints quoted (it can exceed the
    /// safe-integer range other GraphQL `Int` ids stay under). Parsed to
    /// `u64` in the [`From`] impl below rather than here, so a value that
    /// doesn't parse degrades to `None` instead of failing the whole fetch.
    #[serde(rename = "fullDatabaseId", default)]
    full_database_id: Option<String>,
}

impl RemoteThread {
    /// Parse `gh api graphql`'s response to the query
    /// [`super::client::GithubClient::pr_review_threads`] sends.
    pub(super) fn parse_graphql(bytes: &[u8]) -> Result<Vec<RemoteThread>, GhError> {
        let envelope: RawGraphQlEnvelope = serde_json::from_slice(bytes)
            .map_err(|e| invalid("gh api graphql review-threads response", e))?;
        if let Some(errors) = envelope.errors.filter(|errors| !errors.is_empty()) {
            let detail = errors
                .into_iter()
                .map(|e| e.message)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::InvalidResponse {
                detail: format!("GraphQL error: {detail}"),
            });
        }
        let pull_request = envelope
            .data
            .and_then(|d| d.repository)
            .and_then(|r| r.pull_request)
            .ok_or_else(|| GhError::InvalidResponse {
                detail: "GraphQL review-threads response had no repository/pull request \
                          (wrong owner/repo/number, or no access)"
                    .to_string(),
            })?;
        Ok(pull_request
            .review_threads
            .nodes
            .into_iter()
            .map(Into::into)
            .collect())
    }
}

impl From<RawReviewThread> for RemoteThread {
    fn from(raw: RawReviewThread) -> Self {
        // `diffSide` is nullable in principle (an outdated thread); default
        // to `Right` rather than fail the whole fetch over one field this
        // build has no better guess for.
        let side = match raw.diff_side.as_deref() {
            Some("LEFT") => GhSide::Left,
            _ => GhSide::Right,
        };
        let review_database_id = raw
            .comments
            .nodes
            .first()
            .and_then(|c| c.pull_request_review.as_ref())
            .and_then(|r| r.full_database_id.as_deref())
            .and_then(|s| s.parse::<u64>().ok());
        let comments = raw
            .comments
            .nodes
            .into_iter()
            .map(|c| RemoteComment {
                author: c
                    .author
                    .map(|a| a.login)
                    .unwrap_or_else(|| "ghost".to_string()),
                body: c.body,
                created_ms: parse_github_datetime_ms(&c.created_at).unwrap_or(0),
            })
            .collect();
        RemoteThread {
            id: raw.id,
            is_resolved: raw.is_resolved,
            path: raw.path,
            line: raw.line,
            side,
            comments,
            review_database_id,
        }
    }
}

/// Parse a UTC RFC3339 timestamp the way GitHub's GraphQL `DateTime` scalar
/// always prints it (`"2026-07-01T20:09:31Z"` — no fractional seconds, no
/// offset besides `Z`) into epoch milliseconds. Hand-rolled rather than
/// pulling in `chrono` for this one field: dv-core has no other date-
/// parsing need anywhere else in the crate. `None` on anything that doesn't
/// match; the caller falls back to `0` rather than failing the whole fetch
/// over one malformed comment timestamp.
fn parse_github_datetime_ms(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    // Seconds may carry fractional digits ("31.123") — GitHub doesn't emit
    // these today, but tolerate them rather than fail the whole parse.
    let second: i64 = time_parts.next()?.split('.').next()?.parse().ok()?;
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Days since the Unix epoch via the standard civil-calendar algorithm
    // (Howard Hinnant's `days_from_civil`).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (month + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days = era * 146097 + doe - 719468;

    let secs = days * 86400 + hour * 3600 + minute * 60 + second;
    (secs >= 0).then_some(secs as u64 * 1000)
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

/// One @-mentionable user (R3 item 2) — from the
/// GraphQL `mentionableUsers` connection, the same set GitHub's own
/// comment box autocompletes from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Mention {
    pub login: String,
    /// Display name; often absent.
    #[serde(default)]
    pub name: Option<String>,
}

/// One page of the `mentionableUsers` connection: the page's users plus
/// the cursor for the next page (`None` on the last page). Pagination
/// lives in `GithubClient::mentionable_users`; this is just the parse,
/// split out for unit tests (same pattern as [`RemoteThread::parse_graphql`]).
pub(super) fn parse_mentionable_page(
    bytes: &[u8],
) -> Result<(Vec<Mention>, Option<String>), GhError> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        data: Option<Data>,
        #[serde(default)]
        errors: Option<Vec<RawGraphQlError>>,
    }
    #[derive(Deserialize)]
    struct Data {
        #[serde(default)]
        repository: Option<Repository>,
    }
    #[derive(Deserialize)]
    struct Repository {
        #[serde(rename = "mentionableUsers")]
        mentionable_users: Connection,
    }
    #[derive(Deserialize)]
    struct Connection {
        nodes: Vec<Mention>,
        #[serde(rename = "pageInfo")]
        page_info: PageInfo,
    }
    #[derive(Deserialize)]
    struct PageInfo {
        #[serde(rename = "hasNextPage")]
        has_next_page: bool,
        #[serde(rename = "endCursor")]
        end_cursor: Option<String>,
    }

    let envelope: Envelope = serde_json::from_slice(bytes)
        .map_err(|e| invalid("gh api graphql mentionableUsers response", e))?;
    if let Some(errors) = envelope.errors.filter(|errors| !errors.is_empty()) {
        let detail = errors
            .into_iter()
            .map(|e| e.message)
            .collect::<Vec<_>>()
            .join("; ");
        return Err(GhError::InvalidResponse {
            detail: format!("GraphQL error: {detail}"),
        });
    }
    let connection = envelope
        .data
        .and_then(|d| d.repository)
        .ok_or_else(|| GhError::InvalidResponse {
            detail: "GraphQL mentionableUsers response had no repository (wrong owner/repo, or \
                     no access)"
                .to_string(),
        })?
        .mentionable_users;
    let next = if connection.page_info.has_next_page {
        connection.page_info.end_cursor
    } else {
        None
    };
    Ok((connection.nodes, next))
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

    // --- RemoteThread / GraphQL parsing ----------------------------------

    #[test]
    fn parses_review_threads_graphql_fixture() {
        let fixture = r#"{
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [
                                {
                                    "id": "PRRT_1",
                                    "isResolved": true,
                                    "path": "src/a.rs",
                                    "line": 10,
                                    "diffSide": "RIGHT",
                                    "comments": {
                                        "nodes": [
                                            {
                                                "author": {"login": "kylekz"},
                                                "body": "why this way?",
                                                "createdAt": "2026-07-01T12:00:00Z",
                                                "pullRequestReview": {"fullDatabaseId": "555"}
                                            },
                                            {
                                                "author": {"login": "reviewer2"},
                                                "body": "agreed, resolving",
                                                "createdAt": "2026-07-01T12:05:00Z",
                                                "pullRequestReview": null
                                            }
                                        ]
                                    }
                                },
                                {
                                    "id": "PRRT_2",
                                    "isResolved": false,
                                    "path": "src/b.rs",
                                    "line": null,
                                    "diffSide": "LEFT",
                                    "comments": {
                                        "nodes": [
                                            {
                                                "author": null,
                                                "body": "old thread",
                                                "createdAt": "2026-01-01T00:00:00Z",
                                                "pullRequestReview": null
                                            }
                                        ]
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        }"#;
        let threads = RemoteThread::parse_graphql(fixture.as_bytes()).unwrap();
        assert_eq!(threads.len(), 2);

        assert_eq!(threads[0].id, "PRRT_1");
        assert!(threads[0].is_resolved);
        assert_eq!(threads[0].path, "src/a.rs");
        assert_eq!(threads[0].line, Some(10));
        assert_eq!(threads[0].side, GhSide::Right);
        assert_eq!(threads[0].comments.len(), 2);
        assert_eq!(threads[0].comments[0].author, "kylekz");
        assert_eq!(threads[0].review_database_id, Some(555));

        assert!(!threads[1].is_resolved);
        assert_eq!(threads[1].line, None);
        assert_eq!(threads[1].side, GhSide::Left);
        assert_eq!(threads[1].comments[0].author, "ghost");
        assert_eq!(threads[1].review_database_id, None);
    }

    #[test]
    fn review_threads_graphql_errors_array_becomes_invalid_response() {
        let fixture =
            r#"{"data": null, "errors": [{"message": "Could not resolve to a Repository"}]}"#;
        let err = RemoteThread::parse_graphql(fixture.as_bytes()).unwrap_err();
        match err {
            GhError::InvalidResponse { detail } => {
                assert!(detail.contains("Could not resolve to a Repository"))
            }
            other => panic!("expected InvalidResponse, got {other:?}"),
        }
    }

    #[test]
    fn review_threads_graphql_missing_pull_request_is_invalid_response() {
        let fixture = r#"{"data": {"repository": {"pullRequest": null}}}"#;
        assert!(RemoteThread::parse_graphql(fixture.as_bytes()).is_err());
    }

    #[test]
    fn review_threads_graphql_empty_nodes_is_empty_vec() {
        let fixture =
            r#"{"data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": []}}}}}"#;
        let threads = RemoteThread::parse_graphql(fixture.as_bytes()).unwrap();
        assert!(threads.is_empty());
    }

    // --- parse_mentionable_page (R3 item 2) -------------------------------

    #[test]
    fn mentionable_page_parses_users_and_next_cursor() {
        let fixture = r#"{"data": {"repository": {"mentionableUsers": {
            "nodes": [{"login": "kylekz", "name": "Kyle"}, {"login": "octocat", "name": null}],
            "pageInfo": {"hasNextPage": true, "endCursor": "abc123"}}}}}"#;
        let (users, next) = parse_mentionable_page(fixture.as_bytes()).unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].login, "kylekz");
        assert_eq!(users[0].name.as_deref(), Some("Kyle"));
        assert_eq!(users[1].login, "octocat");
        assert!(users[1].name.is_none());
        assert_eq!(next.as_deref(), Some("abc123"));
    }

    #[test]
    fn mentionable_page_last_page_has_no_cursor() {
        // GitHub still populates endCursor on the last page — hasNextPage
        // is the authority, so `next` must come back None regardless.
        let fixture = r#"{"data": {"repository": {"mentionableUsers": {
            "nodes": [{"login": "kylekz"}],
            "pageInfo": {"hasNextPage": false, "endCursor": "zzz"}}}}}"#;
        let (users, next) = parse_mentionable_page(fixture.as_bytes()).unwrap();
        assert_eq!(users.len(), 1);
        assert!(next.is_none());
    }

    #[test]
    fn mentionable_page_graphql_errors_become_invalid_response() {
        let fixture = r#"{"data": null, "errors": [{"message": "no access"}]}"#;
        let err = parse_mentionable_page(fixture.as_bytes()).unwrap_err();
        match err {
            GhError::InvalidResponse { detail } => assert!(detail.contains("no access")),
            other => panic!("expected InvalidResponse, got {other:?}"),
        }
    }

    #[test]
    fn mentionable_page_missing_repository_is_invalid_response() {
        let fixture = r#"{"data": {"repository": null}}"#;
        assert!(parse_mentionable_page(fixture.as_bytes()).is_err());
    }

    #[test]
    fn parse_github_datetime_ms_epoch() {
        assert_eq!(parse_github_datetime_ms("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn parse_github_datetime_ms_known_value() {
        // 2000-01-01T00:00:00Z is 946684800 seconds after the epoch.
        assert_eq!(
            parse_github_datetime_ms("2000-01-01T00:00:00Z"),
            Some(946_684_800_000)
        );
    }

    #[test]
    fn parse_github_datetime_ms_tolerates_fractional_seconds() {
        assert_eq!(
            parse_github_datetime_ms("1970-01-01T00:00:01.500Z"),
            Some(1000)
        );
    }

    #[test]
    fn parse_github_datetime_ms_rejects_non_utc_or_malformed() {
        assert_eq!(parse_github_datetime_ms("not a date"), None);
        assert_eq!(parse_github_datetime_ms("2026-07-01T12:00:00+01:00"), None);
    }
}
