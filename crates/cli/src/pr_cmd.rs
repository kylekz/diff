//! `dv pr <list|view|create|fetch>` and `dv review submit` — the
//! GitHub-backed extensions of the headless CLI (see the crate root's
//! (`lib.rs`) module doc and docs/phase-3-github.md). A child module of the
//! crate root purely so it can reuse the root's private plumbing
//! (`CliError`, `resolve_repo`, `print_json`, `REVIEW_USAGE`, ...) via
//! `super::` — Rust visibility already allows a descendant module to see
//! its ancestor's private items, so none of that needed to become
//! `pub(crate)`.
//!
//! The comment→[`dv_core::DraftComment`] mapping and pre-submission
//! validation this module used to own outright now live in `crate::submit`
//! (a sibling of this module, not a descendant) — the GUI's submit flow
//! (`crates/app/src/workspace.rs`, which reaches it via `dv_cli::submit`)
//! needs the exact same rules, so they moved out to where both can reach
//! them (docs/phase-3-github.md deliverable 3/5; the crate-boundary move
//! itself is Phase 8 S8b). This module keeps only CLI concerns: argument
//! parsing, `--pr`/verdict resolution, and formatting
//! `crate::submit::Violation`s into the CLI's text output.

use dv_core::{
    ChecksSummary, CreatePr, CreatedPr, GhError, GitRepo, GithubClient, PrMeta, PrState, PrSummary,
    RemoteRef, RepoLocation, Review, ReviewDecision, ReviewEvent, ReviewState, ReviewStore,
    Verdict,
};
use serde_json::json;

use crate::submit::{self, SubmissionOutcome, Violation};

use super::{CliError, REVIEW_USAGE, op_err, print_json, resolve_repo, usage_err};

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
    submit::github_client(repo).map_err(op_err)
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

/// The `"cannot submit: N problems found — nothing was sent to GitHub:\n  -
/// ..."` framing, shared by every early-refusal case in `cmd_review_submit`
/// so they all read as one consistent style — not just
/// [`format_violations`]'s validation failures, but also the closed/merged-
/// PR short-circuit below (review finding P3-4: a refactor had that one
/// case print `pr_not_open_message` bare, unwrapped, breaking the
/// consistency this helper restores).
fn format_problem_messages(messages: &[String]) -> String {
    let mut msg = format!(
        "cannot submit: {} problem{} found — nothing was sent to GitHub:\n",
        messages.len(),
        if messages.len() == 1 { "" } else { "s" }
    );
    for m in messages {
        msg.push_str("  - ");
        msg.push_str(m);
        msg.push('\n');
    }
    msg
}

/// Turn `crate::submit::validate_submission`'s structured [`Violation`]s
/// into the CLI's `"cannot submit: N problems found..."` text — the wire
/// output stays byte-for-byte what it was before the validation logic
/// moved out to `crate::submit`, since each `Violation::message` already
/// carries the exact same sentence this used to build inline.
fn format_violations(violations: &[Violation]) -> String {
    format_problem_messages(
        &violations
            .iter()
            .map(|v| v.message.clone())
            .collect::<Vec<_>>(),
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
    let body_text = parsed.body.clone().unwrap_or_default();

    let client = github_client(&repo)?;
    let meta = client.pr_meta(pr_number).map_err(gh_err)?;
    if meta.state != PrState::Open {
        // Kept as an early short-circuit (no point building a submission
        // against a PR that can't receive one), but formatted through the
        // same framing every other early refusal here uses (review finding
        // P3-4).
        return Err(CliError::Op(format_problem_messages(&[
            submit::pr_not_open_message(pr_number, &meta),
        ])));
    }

    let pr_range = crate::pr::prepare_pr(&repo, &meta).map_err(op_err)?;
    let outcome = submit::build_submission(
        &repo,
        &pr_range.merge_base,
        &pr_range.head_oid,
        &review,
        verdict,
        body_text,
        parsed.include_resolved,
    )
    .map_err(op_err)?;
    let submission = match outcome {
        // The nothing-to-submit check moved into `build_submission` itself
        // (review finding P2-2, shared with the GUI's validation), but this
        // one case keeps its own original wording/framing (no other
        // callers depend on `Violation::message`'s exact text, but this
        // one's `--json`/exit-code output predates the move and must not
        // change).
        SubmissionOutcome::Blocked(violations)
            if matches!(
                violations.as_slice(),
                [v] if v.kind == submit::ViolationKind::NothingToSubmit
            ) =>
        {
            return Err(CliError::Op(
                "nothing to submit: no comments (after filtering), no --body, and the verdict is \
                 a plain comment"
                    .to_string(),
            ));
        }
        SubmissionOutcome::Blocked(violations) => {
            return Err(CliError::Op(format_violations(&violations)));
        }
        SubmissionOutcome::Ready(submission) => submission,
    };
    let comment_count = submission.comments.len();
    let event = submission.event;

    let submitted = client
        .submit_review(pr_number, &submission)
        .map_err(|err| {
            CliError::Op(format!(
                "{err}\n(hint: the PR may have been force-pushed since validation — re-run \
             `dv review submit` to re-check and try again)"
            ))
        })?;

    // Fresh-load before mutating (inside `writeback_submitted_review`): the
    // review may have been edited (by the GUI, another agent invocation,
    // ...) between our load above and now — the same discipline
    // `workspace.rs`'s store writes follow. GitHub has already accepted the
    // review at this point, so any failure from here on must say so loudly
    // — the natural instinct on a CLI error is "just re-run it", which here
    // would submit a second, duplicate review.
    let remote = RemoteRef {
        provider: "github".to_string(),
        slug: client.slug().to_string(),
        pr: pr_number,
        url: meta.url.clone(),
        submitted_review_id: Some(submitted.id),
        submitted_url: Some(submitted.html_url.clone()),
    };
    // Fresh live-base handle for the sidebar's offline conflict probe
    // (docs/backlog.md "Stored-but-never-reopened PR range reviews...") —
    // `prepare_pr` above just fetched `meta.base_oid`, and this is the only
    // write path a CLI-created, never-GUI-opened review ever crosses.
    let live_base = dv_core::LiveBase {
        ref_name: Some(format!("refs/remotes/origin/{}", meta.base_ref)),
        oid: Some(meta.base_oid.clone()),
    };
    let fresh = submit::writeback_submitted_review(
        &store,
        &review.id,
        verdict,
        pr_number,
        remote,
        Some(live_base),
        &submitted,
    )
    .map_err(CliError::Op)?;

    print_submission(
        &fresh.id,
        pr_number,
        event,
        comment_count,
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

    use dv_core::{DiffSource, RepoLocation};

    use super::*;

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

    /// A minimal [`Violation`] carrying only the message text —
    /// `format_violations` only ever reads `.message` back out, so the rest
    /// of the fields don't matter for these tests.
    fn violation(message: &str) -> Violation {
        Violation {
            comment_id: String::new(),
            path: String::new(),
            lines: String::new(),
            kind: crate::submit::ViolationKind::StaleAnchor,
            message: message.to_string(),
        }
    }

    #[test]
    fn format_violations_lists_each_one() {
        let msg = format_violations(&[violation("a"), violation("b")]);
        assert!(msg.contains("2 problems"), "message: {msg}");
        assert!(msg.contains("- a"));
        assert!(msg.contains("- b"));
    }

    #[test]
    fn format_violations_singular_wording() {
        let msg = format_violations(&[violation("only one")]);
        assert!(msg.contains("1 problem "), "message: {msg}");
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

    // --- shared real-repo test fixture -----------------------------------

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
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    // `validate_submission`'s own coverage (every violation kind, plus the
    // rename-awareness edge cases) now lives with the function itself in
    // `crate::submit`'s test module — this file only keeps `TestRepo` (used
    // below by `target_review_for_submit`/`cmd_review_submit` tests, which
    // never touch validation directly).

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
            other => panic!("expected an operation error, got: {other:?}"),
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
            other => panic!("expected an operation error, got: {other:?}"),
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
            other => {
                panic!("expected a usage error (exit 2) like missing --verdict, got: {other:?}")
            }
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
            other => panic!("expected an operation error, got: {other:?}"),
        }
    }
}
