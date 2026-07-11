//! `dv pr <list|view|create|fetch>` and `dv review submit` — the
//! GitHub-backed extensions of the headless CLI (see `super`'s module doc
//! and docs/phase-3-github.md). A child module of `cli` purely so it can
//! reuse `cli`'s private plumbing (`CliError`, `resolve_repo`, `line_range`,
//! `side_word`, `print_json`, `REVIEW_USAGE`, ...) via `super::` — Rust
//! visibility already allows a descendant module to see its ancestor's
//! private items, so none of that needed to become `pub(crate)`.

use std::collections::{BTreeMap, HashMap};

use dv_core::diff::diff_blobs;
use dv_core::{
    BlobSpec, ChangeStatus, ChangedFile, ChecksSummary, Comment, CommentStatus, CreatePr,
    CreatedPr, DiffOptions, DiffSource, DraftComment, GhError, GhSide, GitRepo, GithubClient, Hunk,
    PrMeta, PrState, PrSummary, RemoteRef, RepoLocation, Review, ReviewDecision, ReviewEvent,
    ReviewState, ReviewStore, ReviewSubmission, Side, SubmittedReview, Verdict,
};
use serde_json::json;

use super::{
    CliError, REVIEW_USAGE, line_range, op_err, print_json, resolve_repo, side_word, usage_err,
};

pub(super) const PR_USAGE: &str = "\
usage: dv pr <list|view|create|fetch> [options]

  list                          open PRs (gh pr list)
  view <number>                 PR metadata: state, branch, oids, review, checks
  create --title <t> [--body <b>] [--base <ref>] [--draft]
                                 open a PR from the current branch
  fetch <number>                 fetch the PR's head/base and print its diff range

global options (may appear anywhere after `pr`):
  --repo <path>                local path or \\\\wsl.localhost\\<distro>\\<path> (default: .)
  --wsl <distro>:<posix-path>
  --json                       machine-readable output on stdout";

pub(super) fn pr_router(
    sub: &str,
    args: &[String],
    json: bool,
    location: Option<RepoLocation>,
) -> Result<(), CliError> {
    match sub {
        "list" => cmd_pr_list(args, json, location),
        "view" => cmd_pr_view(args, json, location),
        "create" => cmd_pr_create(args, json, location),
        "fetch" => cmd_pr_fetch(args, json, location),
        other => Err(usage_err(
            format!("unknown pr subcommand: {other}"),
            PR_USAGE,
        )),
    }
}

fn github_client(repo: &GitRepo) -> Result<GithubClient, CliError> {
    let client = GithubClient::for_repo(repo).map_err(gh_err)?;
    client.preflight().map_err(gh_err)?;
    Ok(client)
}

fn gh_err(err: GhError) -> CliError {
    CliError::Op(err.to_string())
}

// ---------------------------------------------------------------------
// pr list
// ---------------------------------------------------------------------

fn cmd_pr_list(
    args: &[String],
    json: bool,
    location: Option<RepoLocation>,
) -> Result<(), CliError> {
    if !args.is_empty() {
        return Err(usage_err("pr list takes no arguments", PR_USAGE));
    }
    let repo = resolve_repo(location)?;
    let client = github_client(&repo)?;
    let prs = client.list_prs().map_err(gh_err)?;
    print_pr_list(&prs, json);
    Ok(())
}

fn print_pr_list(prs: &[PrSummary], json: bool) {
    if json {
        print_json(&json!({ "prs": prs }));
        return;
    }
    if prs.is_empty() {
        println!("no open PRs");
        return;
    }
    for pr in prs {
        println!(
            "#{:<5} {}{} ({}, by {})",
            pr.number,
            if pr.is_draft { "[draft] " } else { "" },
            pr.title,
            pr.head_ref,
            pr.author,
        );
    }
}

// ---------------------------------------------------------------------
// pr view
// ---------------------------------------------------------------------

fn parse_pr_number(args: &[String], what: &str) -> Result<u64, CliError> {
    match args {
        [n] if !n.starts_with("--") => n
            .parse::<u64>()
            .map_err(|_| usage_err(format!("{what}: expected a PR number, got {n:?}"), PR_USAGE)),
        [] => Err(usage_err(format!("{what} requires <number>"), PR_USAGE)),
        [extra, ..] => Err(usage_err(
            format!("{what}: unexpected argument {extra:?}"),
            PR_USAGE,
        )),
    }
}

fn cmd_pr_view(
    args: &[String],
    json: bool,
    location: Option<RepoLocation>,
) -> Result<(), CliError> {
    let number = parse_pr_number(args, "pr view")?;
    let repo = resolve_repo(location)?;
    let client = github_client(&repo)?;
    let meta = client.pr_meta(number).map_err(gh_err)?;
    print_pr_meta(&meta, json);
    Ok(())
}

fn print_pr_meta(meta: &PrMeta, json: bool) {
    if json {
        print_json(&json!({ "pr": meta }));
        return;
    }
    println!(
        "#{} {}{}",
        meta.number,
        meta.title,
        if meta.is_draft { " [draft]" } else { "" }
    );
    println!("  state:   {}", pr_state_word(meta.state));
    println!("  branch:  {} <- {}", meta.base_ref, meta.head_ref);
    println!(
        "  oids:    {} <- {}",
        short_oid(&meta.base_oid),
        short_oid(&meta.head_oid)
    );
    println!(
        "  review:  {}",
        meta.review_decision
            .map(review_decision_word)
            .unwrap_or("none")
    );
    println!("  checks:  {}", checks_word(meta.checks));
    println!("  url:     {}", meta.url);
    if !meta.body.trim().is_empty() {
        println!();
        println!("{}", meta.body);
    }
}

fn short_oid(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

fn pr_state_word(state: PrState) -> &'static str {
    match state {
        PrState::Open => "open",
        PrState::Closed => "closed",
        PrState::Merged => "merged",
    }
}

fn review_decision_word(decision: ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::Approved => "approved",
        ReviewDecision::ChangesRequested => "changes requested",
        ReviewDecision::ReviewRequired => "review required",
    }
}

fn checks_word(checks: ChecksSummary) -> &'static str {
    match checks {
        ChecksSummary::Passing => "passing",
        ChecksSummary::Failing => "failing",
        ChecksSummary::Pending => "pending",
        ChecksSummary::None => "none",
    }
}

// ---------------------------------------------------------------------
// pr create
// ---------------------------------------------------------------------

struct PrCreateArgs {
    title: String,
    body: Option<String>,
    base: Option<String>,
    draft: bool,
}

fn parse_pr_create(args: &[String]) -> Result<PrCreateArgs, String> {
    let mut title: Option<String> = None;
    let mut body: Option<String> = None;
    let mut base: Option<String> = None;
    let mut draft = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--title" => title = Some(iter.next().ok_or("--title requires text")?.clone()),
            "--body" => body = Some(iter.next().ok_or("--body requires text")?.clone()),
            "--base" => base = Some(iter.next().ok_or("--base requires a ref")?.clone()),
            "--draft" => draft = true,
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    let title = title.ok_or("--title is required")?;
    Ok(PrCreateArgs {
        title,
        body,
        base,
        draft,
    })
}

fn cmd_pr_create(
    args: &[String],
    json: bool,
    location: Option<RepoLocation>,
) -> Result<(), CliError> {
    let parsed = parse_pr_create(args).map_err(|reason| usage_err(reason, PR_USAGE))?;
    let repo = resolve_repo(location)?;

    let branch = match repo.current_branch() {
        Ok(Some(branch)) => branch,
        Ok(None) => {
            return Err(CliError::Op(
                "cannot create a PR from a detached HEAD — check out a branch first".to_string(),
            ));
        }
        // `git rev-parse --abbrev-ref HEAD` (what `current_branch` shells
        // out to) fails with this raw git text — rather than a clean
        // `Ok(None)` — specifically when `HEAD` doesn't resolve at all: a
        // brand-new repo with no commits yet. Worth naming plainly instead
        // of surfacing "fatal: ambiguous argument 'HEAD': unknown
        // revision..." straight from git.
        Err(err) if is_unborn_head_error(&err) => {
            return Err(CliError::Op(
                "repository has no commits yet — create an initial commit before opening a PR"
                    .to_string(),
            ));
        }
        Err(err) => return Err(op_err(err)),
    };

    // Refuse comparing the current branch against itself: `--base` if the
    // caller gave one, else the remote's default branch (best-effort — if
    // neither is known, skip the check and let `gh` be the final word).
    let base_for_check = parsed.base.clone().or_else(|| repo.default_branch());
    if let Some(base) = &base_for_check
        && &branch == base
    {
        return Err(CliError::Op(format!(
            "current branch ({branch}) is the base branch ({base}) — nothing to open a PR \
             from; check out a feature branch first"
        )));
    }

    let client = github_client(&repo)?;
    let req = CreatePr {
        title: parsed.title,
        body: parsed.body.unwrap_or_default(),
        base: parsed.base,
        draft: parsed.draft,
        head: Some(branch),
    };
    let created = client.create_pr(&req).map_err(gh_err)?;
    print_created_pr(&created, json);
    Ok(())
}

/// Whether `err` is `current_branch`'s failure mode for a repo with no
/// commits yet — git's `rev-parse --abbrev-ref HEAD` can't resolve `HEAD`
/// to anything and prints "fatal: ambiguous argument 'HEAD': unknown
/// revision or path not in the working tree." (exit 128) rather than a
/// clean empty/`None` result.
fn is_unborn_head_error(err: &anyhow::Error) -> bool {
    err.to_string().contains("unknown revision")
}

fn print_created_pr(created: &CreatedPr, json: bool) {
    if json {
        print_json(&json!({ "pr": { "number": created.number, "url": created.url } }));
        return;
    }
    println!("created PR #{}: {}", created.number, created.url);
}

// ---------------------------------------------------------------------
// pr fetch
// ---------------------------------------------------------------------

fn cmd_pr_fetch(
    args: &[String],
    json: bool,
    location: Option<RepoLocation>,
) -> Result<(), CliError> {
    let number = parse_pr_number(args, "pr fetch")?;
    let repo = resolve_repo(location)?;
    let client = github_client(&repo)?;
    let meta = client.pr_meta(number).map_err(gh_err)?;
    let range = crate::pr::prepare_pr(&repo, &meta).map_err(op_err)?;
    print_pr_range(&range, json);
    Ok(())
}

fn print_pr_range(range: &crate::pr::PrRange, json: bool) {
    if json {
        print_json(&json!({
            "pr_range": {
                "number": range.number,
                "base_oid": range.base_oid,
                "head_oid": range.head_oid,
                "merge_base": range.merge_base,
                "range": range.range,
            }
        }));
        return;
    }
    println!("PR #{}: {}", range.number, range.range);
    println!("  base_oid:   {}", range.base_oid);
    println!("  head_oid:   {}", range.head_oid);
    println!("  merge_base: {}", range.merge_base);
}

// ---------------------------------------------------------------------
// review submit
// ---------------------------------------------------------------------

struct ReviewSubmitArgs {
    review_id: Option<String>,
    pr: Option<u64>,
    verdict: Option<Verdict>,
    body: Option<String>,
    include_resolved: bool,
}

fn parse_verdict(value: &str) -> Result<Verdict, String> {
    match value {
        "comment" => Ok(Verdict::Comment),
        "approve" => Ok(Verdict::Approve),
        "request-changes" => Ok(Verdict::RequestChanges),
        other => Err(format!(
            "--verdict must be comment|approve|request-changes, got {other}"
        )),
    }
}

fn parse_review_submit(args: &[String]) -> Result<ReviewSubmitArgs, String> {
    let mut review_id: Option<String> = None;
    let mut pr: Option<u64> = None;
    let mut verdict: Option<Verdict> = None;
    let mut body: Option<String> = None;
    let mut include_resolved = false;

    let mut rest = args;
    if let Some((first, tail)) = rest.split_first()
        && !first.starts_with("--")
    {
        review_id = Some(first.clone());
        rest = tail;
    }

    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--pr" => {
                let value = iter.next().ok_or("--pr requires a PR number")?;
                pr = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("--pr expects a number, got {value:?}"))?,
                );
            }
            "--verdict" => {
                let value = iter
                    .next()
                    .ok_or("--verdict requires comment|approve|request-changes")?;
                verdict = Some(parse_verdict(value)?);
            }
            "--body" => body = Some(iter.next().ok_or("--body requires text")?.clone()),
            "--include-resolved" => include_resolved = true,
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(ReviewSubmitArgs {
        review_id,
        pr,
        verdict,
        body,
        include_resolved,
    })
}

/// `--verdict` wins; else the review's own locally-recorded verdict (if it
/// was already marked `Submitted` locally, e.g. by the GUI's finish-review
/// flow, before ever reaching GitHub); else an error naming the choices.
fn resolve_verdict(explicit: Option<Verdict>, state: &ReviewState) -> Result<Verdict, String> {
    if let Some(v) = explicit {
        return Ok(v);
    }
    match state {
        ReviewState::Submitted { verdict, .. } => Ok(*verdict),
        ReviewState::Draft => {
            Err("no verdict given: pass --verdict comment|approve|request-changes".to_string())
        }
    }
}

fn verdict_to_event(verdict: Verdict) -> ReviewEvent {
    match verdict {
        Verdict::Comment => ReviewEvent::Comment,
        Verdict::Approve => ReviewEvent::Approve,
        Verdict::RequestChanges => ReviewEvent::RequestChanges,
    }
}

fn event_word(event: ReviewEvent) -> &'static str {
    match event {
        ReviewEvent::Comment => "comment",
        ReviewEvent::Approve => "approve",
        ReviewEvent::RequestChanges => "request_changes",
    }
}

/// Explicit id, else the most recent local draft (same rule
/// `target_review_for_add` uses for `comment add`, minus the auto-create —
/// submitting nothing into existence would be nonsensical), else the most
/// recent review already marked `Submitted` locally but never actually sent
/// to GitHub (`remote` unset, or set but `submitted_review_id` still
/// `None`) — the natural state after the GUI's finish-review flow, which
/// marks a review Submitted without itself talking to GitHub. [`ReviewStore::list`]
/// already sorts newest-`created_ms`-first, so each `.find` below picks the
/// most recent match.
fn target_review_for_submit(
    store: &ReviewStore,
    review_id: Option<&str>,
) -> Result<Review, CliError> {
    if let Some(id) = review_id {
        return store
            .load(id)
            .map_err(op_err)?
            .ok_or_else(|| CliError::Op(format!("no review with id {id:?}")));
    }
    let reviews = store.list().map_err(op_err)?;
    if let Some(draft) = reviews
        .iter()
        .find(|r| matches!(r.state, ReviewState::Draft))
    {
        return Ok(draft.clone());
    }
    if let Some(unsent) = reviews.iter().find(|r| {
        matches!(r.state, ReviewState::Submitted { .. })
            && r.remote
                .as_ref()
                .is_none_or(|remote| remote.submitted_review_id.is_none())
    }) {
        return Ok(unsent.clone());
    }
    Err(CliError::Op(
        "no review ready to submit: no draft, and no locally-finished review still awaiting \
         GitHub — finish a review in the GUI, or start one with `dv comment add`, then pass its \
         <review-id> explicitly if this still doesn't find it"
            .to_string(),
    ))
}

fn gh_side(side: Side) -> GhSide {
    match side {
        Side::Old => GhSide::Left,
        Side::New => GhSide::Right,
    }
}

/// dv stores paths forward-slashed already (docs/architecture.md § Data
/// model), but a stray Windows-style path — hand-edited store JSON, a
/// future GUI path builder bug — must not silently mis-anchor a GitHub
/// comment, so this is enforced again right before it leaves the process.
fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// The main body, then each reply flattened underneath as a markdown
/// blockquote — GitHub review comments have no native reply thread, so this
/// is the closest single-comment-body approximation.
fn flatten_body(comment: &Comment) -> String {
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

fn map_comment(comment: &Comment) -> DraftComment {
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

/// Validate a batch of comments against the PR's current diff before
/// submitting: every comment's anchor must still match the content it was
/// made against ([`validate_comment_anchors`]), and every commented line
/// must actually fall inside the PR's diff ([`validate_lines_in_diff`]).
/// Takes only a [`GitRepo`] and the two oids bounding the diff — no `gh`,
/// no [`PrMeta`] — so it's directly testable against a plain temp repo (see
/// the tests below); `cmd_review_submit` is the only caller that plugs in
/// real PR data.
pub(super) fn validate_submission(
    repo: &GitRepo,
    base_oid: &str,
    head_oid: &str,
    comments: &[&Comment],
) -> anyhow::Result<Vec<String>> {
    let changed = changed_file_map(repo, base_oid, head_oid)?;
    let mut violations = validate_comment_anchors(repo, base_oid, head_oid, &changed, comments)?;
    violations.extend(validate_lines_in_diff(
        repo, base_oid, head_oid, &changed, comments,
    )?);
    Ok(violations)
}

/// Changed-file map for `base_oid..head_oid`, keyed by each entry's
/// new-side path — `ChangedFile::path` is documented as the new-side path
/// (old side, for deletes), which is also always what a [`Comment::path`]
/// records, even for an old-side anchor on a renamed file (see the
/// docs/backlog.md caveat this fix appends). Built once per validation run
/// so the per-comment checks below are plain map reads instead of a
/// `git diff` per comment.
fn changed_file_map(
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
) -> anyhow::Result<Vec<String>> {
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
            violations.push(format!(
                "comment {} on {}:{} (old) is on a file renamed in the PR (previously {}) — its \
                 anchor cannot be verified and needs re-anchoring",
                comment.id,
                path,
                line_range(comment.start_line, comment.end_line),
                old_path,
            ));
            continue;
        }

        if comment.blob_sha.is_none() {
            violations.push(format!(
                "comment {} on {}:{} has no recorded anchor (unverifiable) — cannot safely \
                 place it",
                comment.id,
                path,
                line_range(comment.start_line, comment.end_line),
            ));
        } else {
            violations.push(format!(
                "comment {} on {}:{} ({}) has a stale anchor — the file has changed since the \
                 comment was made",
                comment.id,
                path,
                line_range(comment.start_line, comment.end_line),
                side_word(comment.side),
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
) -> anyhow::Result<Vec<String>> {
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
                violations.push(format!(
                    "comment {} on {}:{} ({}) is not part of the PR diff",
                    comment.id,
                    path,
                    line_range(comment.start_line, comment.end_line),
                    side_word(comment.side),
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

fn format_violations(violations: &[String]) -> String {
    let mut msg = format!(
        "cannot submit: {} problem{} found — nothing was sent to GitHub:\n",
        violations.len(),
        if violations.len() == 1 { "" } else { "s" }
    );
    for v in violations {
        msg.push_str("  - ");
        msg.push_str(v);
        msg.push('\n');
    }
    msg
}

/// The error for when GitHub has already accepted a review submission but
/// recording that locally then failed (the review vanished from the store
/// between load and save, or the save itself errored). Factored out as a
/// pure function so both the human and `--json` paths (which share
/// `CliError::Op`'s single message, per `report_error`) get identical
/// wording, and so the critical fact — GitHub already has this review,
/// re-running `submit` would duplicate it — can't accidentally be dropped
/// from one call site but not the other.
fn writeback_failure_message(pr_number: u64, submitted: &SubmittedReview, cause: &str) -> String {
    format!(
        "GitHub ACCEPTED the review: submitted review #{} on PR #{pr_number} ({}), but recording \
         it in the local review store failed: {cause}\n\
         Do NOT re-run `dv review submit` for this — GitHub already has this review, and \
         submitting again would create a duplicate review on the PR. Fix the local issue (see \
         the cause above), then reconcile the review store by hand if needed.",
        submitted.id, submitted.html_url,
    )
}

fn print_submission(
    review_id: &str,
    pr: u64,
    event: ReviewEvent,
    comment_count: usize,
    url: &str,
    json: bool,
) {
    if json {
        print_json(&json!({
            "submission": {
                "review_id": review_id,
                "pr": pr,
                "event": event_word(event),
                "comments": comment_count,
                "url": url,
            }
        }));
        return;
    }
    println!(
        "submitted {} to PR #{pr} with {comment_count} comment{}: {url}",
        event_word(event),
        if comment_count == 1 { "" } else { "s" },
    );
}

pub(super) fn cmd_review_submit(
    args: &[String],
    json: bool,
    location: Option<RepoLocation>,
) -> Result<(), CliError> {
    let parsed = parse_review_submit(args).map_err(|reason| usage_err(reason, REVIEW_USAGE))?;

    let repo = resolve_repo(location)?;
    let store = ReviewStore::open(repo.location().clone());
    let review = target_review_for_submit(&store, parsed.review_id.as_deref())?;

    if let Some(remote) = &review.remote
        && remote.submitted_review_id.is_some()
    {
        return Err(CliError::Op(format!(
            "review {} was already submitted to GitHub ({}) — submitting again would create \
             a duplicate review on the PR",
            review.id,
            remote.submitted_url.as_deref().unwrap_or(&remote.url),
        )));
    }

    let linked_pr = review.remote.as_ref().map(|r| r.pr);
    let pr_number = match (parsed.pr, linked_pr) {
        (Some(given), Some(linked)) if given != linked => {
            return Err(CliError::Op(format!(
                "review is linked to PR {linked} but --pr {given} was given; pass --pr \
                 {linked} or create a new review for PR {given}"
            )));
        }
        (Some(given), _) => given,
        (None, Some(linked)) => linked,
        (None, None) => {
            return Err(usage_err(
                "no PR specified: pass --pr <number>, or link the review to a PR first",
                REVIEW_USAGE,
            ));
        }
    };

    let verdict = resolve_verdict(parsed.verdict, &review.state)
        .map_err(|reason| usage_err(reason, REVIEW_USAGE))?;
    let event = verdict_to_event(verdict);

    let comments: Vec<&Comment> = review
        .comments
        .iter()
        .filter(|c| parsed.include_resolved || c.status == CommentStatus::Open)
        .collect();

    let body_text = parsed.body.clone().unwrap_or_default();
    if comments.is_empty() && body_text.trim().is_empty() && event == ReviewEvent::Comment {
        return Err(CliError::Op(
            "nothing to submit: no comments (after filtering), no --body, and the verdict is \
             a plain comment"
                .to_string(),
        ));
    }

    let client = github_client(&repo)?;
    let meta = client.pr_meta(pr_number).map_err(gh_err)?;

    let mut violations = Vec::new();
    if meta.state != PrState::Open {
        violations.push(format!(
            "PR #{pr_number} is {} — only an open PR can receive a review",
            pr_state_word(meta.state)
        ));
    }

    let pr_range = crate::pr::prepare_pr(&repo, &meta).map_err(op_err)?;
    violations.extend(
        validate_submission(&repo, &pr_range.merge_base, &pr_range.head_oid, &comments)
            .map_err(op_err)?,
    );

    if !violations.is_empty() {
        return Err(CliError::Op(format_violations(&violations)));
    }

    let draft_comments: Vec<DraftComment> = comments.iter().copied().map(map_comment).collect();
    let submission = ReviewSubmission {
        commit_id: pr_range.head_oid.clone(),
        body: body_text,
        event,
        comments: draft_comments,
    };

    let submitted = client
        .submit_review(pr_number, &submission)
        .map_err(|err| {
            CliError::Op(format!(
                "{err}\n(hint: the PR may have been force-pushed since validation — re-run \
             `dv review submit` to re-check and try again)"
            ))
        })?;

    // Fresh-load before mutating: the review may have been edited (by the
    // GUI, another agent invocation, ...) between our load above and now —
    // the same discipline `workspace.rs`'s store writes follow. GitHub has
    // already accepted the review at this point, so any failure from here
    // on must say so loudly — the natural instinct on a CLI error is "just
    // re-run it", which here would submit a second, duplicate review.
    let mut fresh = match store.load(&review.id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Err(CliError::Op(writeback_failure_message(
                pr_number,
                &submitted,
                &format!("review {} disappeared from the local store", review.id),
            )));
        }
        Err(err) => {
            return Err(CliError::Op(writeback_failure_message(
                pr_number,
                &submitted,
                &format!("{err:#}"),
            )));
        }
    };
    fresh.set_state(ReviewState::Submitted {
        verdict,
        at_ms: dv_core::review::now_ms(),
    });
    fresh.remote = Some(RemoteRef {
        provider: "github".to_string(),
        slug: client.slug().to_string(),
        pr: pr_number,
        url: meta.url.clone(),
        submitted_review_id: Some(submitted.id),
        submitted_url: Some(submitted.html_url.clone()),
    });
    if let Err(err) = store.save(&fresh) {
        return Err(CliError::Op(writeback_failure_message(
            pr_number,
            &submitted,
            &format!("{err:#}"),
        )));
    }

    print_submission(
        &fresh.id,
        pr_number,
        event,
        comments.len(),
        &submitted.html_url,
        json,
    );
    Ok(())
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

    // --- resolve_verdict ---------------------------------------------------

    #[test]
    fn resolve_verdict_explicit_wins() {
        let state = ReviewState::Submitted {
            verdict: Verdict::Approve,
            at_ms: 0,
        };
        assert_eq!(
            resolve_verdict(Some(Verdict::Comment), &state),
            Ok(Verdict::Comment)
        );
    }

    #[test]
    fn resolve_verdict_falls_back_to_local_submitted_state() {
        let state = ReviewState::Submitted {
            verdict: Verdict::RequestChanges,
            at_ms: 0,
        };
        assert_eq!(resolve_verdict(None, &state), Ok(Verdict::RequestChanges));
    }

    #[test]
    fn resolve_verdict_errors_when_draft_and_no_explicit_verdict() {
        assert!(resolve_verdict(None, &ReviewState::Draft).is_err());
    }

    // --- format_violations --------------------------------------------------

    #[test]
    fn format_violations_lists_each_one() {
        let msg = format_violations(&["a".to_string(), "b".to_string()]);
        assert!(msg.contains("2 problems"), "message: {msg}");
        assert!(msg.contains("- a"));
        assert!(msg.contains("- b"));
    }

    #[test]
    fn format_violations_singular_wording() {
        let msg = format_violations(&["only one".to_string()]);
        assert!(msg.contains("1 problem "), "message: {msg}");
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

    // --- arg parsing ---------------------------------------------------

    #[test]
    fn parse_review_submit_minimal() {
        let args: Vec<String> = ["--pr", "7"].iter().map(|s| s.to_string()).collect();
        let parsed = parse_review_submit(&args).expect("should parse");
        assert!(parsed.review_id.is_none());
        assert_eq!(parsed.pr, Some(7));
        assert!(parsed.verdict.is_none());
        assert!(!parsed.include_resolved);
    }

    #[test]
    fn parse_review_submit_with_leading_id_and_flags() {
        let args: Vec<String> = [
            "r-123-abcd",
            "--pr",
            "9",
            "--verdict",
            "approve",
            "--body",
            "ship it",
            "--include-resolved",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let parsed = parse_review_submit(&args).expect("should parse");
        assert_eq!(parsed.review_id.as_deref(), Some("r-123-abcd"));
        assert_eq!(parsed.pr, Some(9));
        assert_eq!(parsed.verdict, Some(Verdict::Approve));
        assert_eq!(parsed.body.as_deref(), Some("ship it"));
        assert!(parsed.include_resolved);
    }

    #[test]
    fn parse_review_submit_bad_verdict_is_error() {
        let args: Vec<String> = ["--verdict", "nope"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_review_submit(&args).is_err());
    }

    #[test]
    fn parse_pr_create_requires_title() {
        let args: Vec<String> = ["--body", "x"].iter().map(|s| s.to_string()).collect();
        assert!(parse_pr_create(&args).is_err());
    }

    #[test]
    fn parse_pr_create_all_flags() {
        let args: Vec<String> = ["--title", "t", "--body", "b", "--base", "main", "--draft"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parsed = parse_pr_create(&args).expect("should parse");
        assert_eq!(parsed.title, "t");
        assert_eq!(parsed.body.as_deref(), Some("b"));
        assert_eq!(parsed.base.as_deref(), Some("main"));
        assert!(parsed.draft);
    }

    #[test]
    fn parse_pr_number_rejects_non_numeric() {
        let args: Vec<String> = vec!["abc".to_string()];
        assert!(parse_pr_number(&args, "pr view").is_err());
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
            let dir = std::env::temp_dir().join(format!("dv-pr-cmd-test-{pid}-{n}-{name}"));
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
            !violations.iter().any(|v| v.contains(&ok.id)),
            "ok case must not be flagged: {violations:#?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains(&stale.id) && v.contains("stale")),
            "{violations:#?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains(&unanchored.id) && v.contains("unverifiable")),
            "{violations:#?}"
        );
        assert!(
            violations.iter().any(|v| v.contains(&missing.id)),
            "{violations:#?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains(&out_of_hunk.id) && v.contains("not part of the PR diff")),
            "{violations:#?}"
        );
        assert!(
            !violations
                .iter()
                .any(|v| v.contains(&rename_new_in_hunk.id)),
            "rename (a) in-hunk new-side comment must not be flagged: {violations:#?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.contains(&rename_new_out_of_hunk.id)
                    && v.contains("not part of the PR diff")),
            "rename (b) out-of-hunk new-side comment must be flagged: {violations:#?}"
        );
        assert!(
            !violations
                .iter()
                .any(|v| v.contains(&rename_old_correct.id)),
            "rename (c) correctly old_path-anchored comment must not be flagged: {violations:#?}"
        );
    }

    // --- target_review_for_submit --------------------------------------

    #[test]
    fn target_review_for_submit_falls_back_to_unsent_local_submission() {
        let repo = TestRepo::new("submit-fallback");
        let store = ReviewStore::open(RepoLocation::Local(repo.path().to_path_buf()));

        let mut review = store.create(DiffSource::WorkingTree).expect("create draft");
        review.set_state(ReviewState::Submitted {
            verdict: Verdict::Approve,
            at_ms: 0,
        });
        review.remote = Some(RemoteRef {
            provider: "github".to_string(),
            slug: "github.com/o/r".to_string(),
            pr: 5,
            url: "https://example.invalid/pull/5".to_string(),
            submitted_review_id: None,
            submitted_url: None,
        });
        store.save(&review).expect("save");

        let found = target_review_for_submit(&store, None)
            .unwrap_or_else(|_| panic!("a locally-finished, not-yet-sent review should be found"));
        assert_eq!(found.id, review.id);
    }

    #[test]
    fn target_review_for_submit_prefers_draft_over_unsent_submission() {
        let repo = TestRepo::new("submit-fallback-prefers-draft");
        let store = ReviewStore::open(RepoLocation::Local(repo.path().to_path_buf()));

        let mut old_submitted = store.create(DiffSource::WorkingTree).expect("create");
        old_submitted.set_state(ReviewState::Submitted {
            verdict: Verdict::Comment,
            at_ms: 0,
        });
        store.save(&old_submitted).expect("save");

        let draft = store.create(DiffSource::WorkingTree).expect("create draft");

        let found = target_review_for_submit(&store, None)
            .unwrap_or_else(|_| panic!("should find the draft"));
        assert_eq!(found.id, draft.id);
    }

    #[test]
    fn target_review_for_submit_errors_with_gui_and_comment_add_hint_when_nothing_found() {
        let repo = TestRepo::new("submit-fallback-empty");
        let store = ReviewStore::open(RepoLocation::Local(repo.path().to_path_buf()));

        let err =
            target_review_for_submit(&store, None).expect_err("no reviews at all must be an error");
        match err {
            CliError::Op(msg) => {
                assert!(msg.contains("GUI"), "message: {msg}");
                assert!(msg.contains("dv comment add"), "message: {msg}");
                assert!(
                    !msg.contains("dv review create"),
                    "no draft exists to create-then-submit here, so this advice is wrong: {msg}"
                );
            }
            CliError::Usage { .. } => panic!("expected an operation error, got a usage error"),
        }
    }

    // --- cmd_review_submit: --pr / linkage handling -------------------------

    #[test]
    fn cmd_review_submit_errors_when_pr_flag_disagrees_with_linked_pr() {
        let repo = TestRepo::new("pr-mismatch");
        let location = RepoLocation::Local(repo.path().to_path_buf());
        let git_repo = GitRepo::open(location.clone()).expect("open repo");
        let store = ReviewStore::open(git_repo.location().clone());

        let mut review = store.create(DiffSource::WorkingTree).expect("create draft");
        review.remote = Some(RemoteRef {
            provider: "github".to_string(),
            slug: "github.com/o/r".to_string(),
            pr: 5,
            url: "https://example.invalid/pull/5".to_string(),
            submitted_review_id: None,
            submitted_url: None,
        });
        store.save(&review).expect("save");

        let args: Vec<String> = [review.id.as_str(), "--pr", "9"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = cmd_review_submit(&args, false, Some(location))
            .expect_err("a --pr disagreeing with the linked PR must error");
        match err {
            CliError::Op(msg) => {
                assert!(msg.contains("linked to PR 5"), "message: {msg}");
                assert!(msg.contains("--pr 9"), "message: {msg}");
            }
            CliError::Usage { .. } => panic!("expected an operation error, got a usage error"),
        }
    }

    #[test]
    fn cmd_review_submit_errors_as_usage_when_no_pr_given_and_no_linkage() {
        let repo = TestRepo::new("no-pr-no-link");
        let location = RepoLocation::Local(repo.path().to_path_buf());
        let git_repo = GitRepo::open(location.clone()).expect("open repo");
        let store = ReviewStore::open(git_repo.location().clone());
        let review = store.create(DiffSource::WorkingTree).expect("create draft");

        let args: Vec<String> = [review.id.as_str()].iter().map(|s| s.to_string()).collect();
        let err = cmd_review_submit(&args, false, Some(location))
            .expect_err("missing --pr with no linkage must error");
        match err {
            CliError::Usage { .. } => {}
            CliError::Op(msg) => panic!(
                "expected a usage error (exit 2) like missing --verdict, got an operation \
                 error: {msg}"
            ),
        }
    }

    // --- cmd_pr_create: repo with no commits --------------------------------

    #[test]
    fn cmd_pr_create_reports_clear_error_for_repo_with_no_commits() {
        let repo = TestRepo::new("no-commits");
        let location = RepoLocation::Local(repo.path().to_path_buf());
        let args: Vec<String> = ["--title", "t"].iter().map(|s| s.to_string()).collect();

        let err = cmd_pr_create(&args, false, Some(location))
            .expect_err("a repo with no commits must not reach `gh`");
        match err {
            CliError::Op(msg) => assert!(
                msg.contains("no commits yet"),
                "message should name the real problem, not surface raw git text: {msg}"
            ),
            CliError::Usage { .. } => panic!("expected an operation error, got a usage error"),
        }
    }
}
