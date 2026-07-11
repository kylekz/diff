//! [`GithubClient`]: resolves the `gh` binary once, pins it to the repo's
//! `origin` remote, and wraps every GitHub call dv makes.
//!
//! Deliberately **not** built on [`crate::command::CommandBuilder`]: that
//! type routes through `wsl.exe` for a WSL-located repo, but `gh` always
//! runs on the Windows host, even when the repo it's operating on lives
//! inside a distro (see CLAUDE.md's design notes and
//! docs/phase-3-github.md). Repo identity instead comes from the *routed*
//! git layer (`GitRepo::remote_url`), so a WSL repo gets GitHub support
//! without `gh` needing to be installed in the distro at all.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::error::GhError;
use super::models::{
    CreatePr, CreatedPr, PrMeta, PrStatus, PrSummary, ReviewSubmission, SubmittedReview,
};
use super::slug::RepoSlug;
use crate::git::GitRepo;

/// A resolved `gh` binary pinned to one repo's GitHub identity.
pub struct GithubClient {
    gh_path: PathBuf,
    slug: RepoSlug,
}

impl GithubClient {
    /// Resolve `gh` and read `repo`'s `origin` remote to derive its GitHub
    /// slug. Does not itself talk to GitHub or check authentication — call
    /// [`Self::preflight`] for that.
    pub fn for_repo(repo: &GitRepo) -> Result<Self, GhError> {
        let gh_path = resolve_gh_path()?;
        let url = repo.remote_url("origin").map_err(|err| {
            let message = format!("{err:#}");
            if message.to_lowercase().contains("no such remote") {
                GhError::NoOriginRemote
            } else {
                GhError::Command { detail: message }
            }
        })?;
        let slug = RepoSlug::parse_remote_url(&url)?;
        Ok(Self { gh_path, slug })
    }

    /// Build directly from an already-known slug, bypassing `origin`
    /// lookup — used by tests, and available to callers that already have
    /// the slug from elsewhere (e.g. a `dv pr <url>` invocation that parsed
    /// it straight from the URL).
    pub fn with_slug(gh_path: PathBuf, slug: RepoSlug) -> Self {
        Self { gh_path, slug }
    }

    /// Resolve `gh` and build directly from an already-known `slug` — no
    /// [`GitRepo`] / `origin` remote read needed at all. Used by the
    /// sidebar's PR-status badge refresh, which already has the slug from a
    /// review's stored `RemoteRef` and would otherwise pay for an
    /// unnecessary repo open + `git remote get-url` just to re-derive what
    /// it already knows.
    pub fn for_slug(slug: RepoSlug) -> Result<Self, GhError> {
        let gh_path = resolve_gh_path()?;
        Ok(Self { gh_path, slug })
    }

    pub fn slug(&self) -> &RepoSlug {
        &self.slug
    }

    /// `gh --version` (proves the binary actually runs) then `gh auth
    /// status --hostname <host>` (proves we're logged in to the repo's
    /// host). Both failures map to actionable [`GhError`] variants rather
    /// than a raw gh stderr dump.
    pub fn preflight(&self) -> Result<(), GhError> {
        self.run_gh(&["--version"])?;
        match self.run_gh(&["auth", "status", "--hostname", &self.slug.host]) {
            Ok(_) => Ok(()),
            // `gh auth status`'s entire job is checking authentication, so
            // any non-zero exit from it means "not authenticated here" —
            // no need for `classify_failure`'s keyword heuristic.
            Err(_) => Err(GhError::NotAuthenticated {
                host: self.slug.host.clone(),
            }),
        }
    }

    /// `gh pr list -R <slug> --json ... --state open --limit 50`.
    pub fn list_prs(&self) -> Result<Vec<PrSummary>, GhError> {
        let repo_arg = self.slug.to_string();
        let out = self.run_gh(&[
            "pr",
            "list",
            "-R",
            &repo_arg,
            "--json",
            "number,title,author,headRefName,isDraft,updatedAt",
            "--state",
            "open",
            "--limit",
            "50",
        ])?;
        PrSummary::parse_list(&out)
    }

    /// `gh pr view <n> -R <slug> --json ...` — full metadata for the PR
    /// header panel.
    pub fn pr_meta(&self, number: u64) -> Result<PrMeta, GhError> {
        let repo_arg = self.slug.to_string();
        let number_arg = number.to_string();
        let out = self.run_gh(&[
            "pr",
            "view",
            &number_arg,
            "-R",
            &repo_arg,
            "--json",
            "number,title,body,url,state,isDraft,baseRefName,headRefName,baseRefOid,headRefOid,reviewDecision,statusCheckRollup,author",
        ])?;
        PrMeta::parse(&out)
    }

    /// The lighter-weight status query used to refresh sidebar icons.
    pub fn pr_status(&self, number: u64) -> Result<PrStatus, GhError> {
        let repo_arg = self.slug.to_string();
        let number_arg = number.to_string();
        let out = self.run_gh(&[
            "pr",
            "view",
            &number_arg,
            "-R",
            &repo_arg,
            "--json",
            "state,isDraft,reviewDecision,statusCheckRollup",
        ])?;
        PrStatus::parse(&out)
    }

    /// `gh api repos/{owner}/{repo}/pulls/{n}/reviews --method POST --input -`,
    /// with `req` written to the child's stdin as JSON. Enterprise hosts
    /// need `--hostname <host>` on `gh api` (unlike `gh pr ...`, which
    /// takes it via `-R host/owner/repo` instead).
    pub fn submit_review(
        &self,
        number: u64,
        req: &ReviewSubmission,
    ) -> Result<SubmittedReview, GhError> {
        let body = serde_json::to_vec(req).map_err(|e| GhError::InvalidResponse {
            detail: format!("serializing review submission: {e}"),
        })?;
        let endpoint = format!(
            "repos/{}/{}/pulls/{number}/reviews",
            self.slug.owner, self.slug.repo
        );
        let mut args: Vec<&str> = vec!["api", &endpoint, "--method", "POST", "--input", "-"];
        if self.slug.host != "github.com" {
            args.push("--hostname");
            args.push(&self.slug.host);
        }
        let out = self.run_gh_with_stdin(&args, &body)?;
        SubmittedReview::parse(&out)
    }

    /// `gh pr create -R <slug> --title ... --body ... [--base ...]
    /// [--draft] [--head <branch>]`. Maps gh's "no upstream for this
    /// branch" failure to [`GhError::BranchNotPushed`] — dv never
    /// auto-pushes, so the user has to do that themselves and retry.
    pub fn create_pr(&self, req: &CreatePr) -> Result<CreatedPr, GhError> {
        let repo_arg = self.slug.to_string();
        let mut args: Vec<&str> = vec![
            "pr", "create", "-R", &repo_arg, "--title", &req.title, "--body", &req.body,
        ];
        if let Some(base) = req.base.as_deref() {
            args.push("--base");
            args.push(base);
        }
        if req.draft {
            args.push("--draft");
        }
        if let Some(head) = req.head.as_deref() {
            args.push("--head");
            args.push(head);
        }

        match self.run_gh(&args) {
            Ok(out) => CreatedPr::parse_stdout(&out),
            Err(GhError::Command { detail }) if looks_like_unpushed_branch(&detail) => {
                Err(GhError::BranchNotPushed {
                    branch: req
                        .head
                        .clone()
                        .unwrap_or_else(|| "the current branch".to_string()),
                })
            }
            Err(other) => Err(other),
        }
    }

    /// `gh api user --jq .login` — the authenticated user's login, used as
    /// a submitted comment/reply's author.
    pub fn current_login(&self) -> Result<String, GhError> {
        let host = self.slug.host.clone();
        let mut args: Vec<&str> = vec!["api", "user"];
        if host != "github.com" {
            args.push("--hostname");
            args.push(&host);
        }
        args.push("--jq");
        args.push(".login");
        let out = self.run_gh(&args)?;
        Ok(crate::command::decode_output(&out).trim().to_string())
    }

    fn spawn(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.gh_path);
        cmd.args(args);
        // Never interactive: dv has no terminal for gh to prompt into, and
        // an editor/pager popping up would just hang the caller.
        cmd.env("GH_PROMPT_DISABLED", "1");
        cmd.env("GH_NO_UPDATE_NOTIFIER", "1");
        cmd.env("GH_PAGER", "cat");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(crate::command::CREATE_NO_WINDOW);
        }
        cmd
    }

    fn run_gh(&self, args: &[&str]) -> Result<Vec<u8>, GhError> {
        let output = self.spawn(args).output().map_err(|e| GhError::Command {
            detail: format!("failed to run gh {}: {e}", args.join(" ")),
        })?;
        if !output.status.success() {
            let stderr = crate::command::decode_output(&output.stderr);
            return Err(classify_failure(&stderr, "", &self.slug.host));
        }
        Ok(output.stdout)
    }

    /// Like [`Self::run_gh`], but writes `stdin_bytes` to the child's
    /// stdin first — mirrors `CommandBuilder::run_with_stdin`'s
    /// close-before-wait ordering (see its doc comment) to avoid the same
    /// full-pipe deadlock.
    fn run_gh_with_stdin(&self, args: &[&str], stdin_bytes: &[u8]) -> Result<Vec<u8>, GhError> {
        let mut cmd = self.spawn(args);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| GhError::Command {
            detail: format!("failed to run gh {}: {e}", args.join(" ")),
        })?;

        // If gh exits before draining stdin (unauthenticated, connection
        // refused, ...) and `stdin_bytes` is bigger than the pipe buffer,
        // this write fails (broken pipe). Don't return on that: stash the
        // error and let `finalize_stdin_result` decide, once the child's
        // real exit status and stderr are known — otherwise a "failed
        // writing to gh stdin" error would mask gh's actual diagnosis.
        let write_err = {
            let mut stdin = child.stdin.take().ok_or_else(|| GhError::Command {
                detail: "gh: missing stdin handle".to_string(),
            })?;
            stdin.write_all(stdin_bytes).err()
        }; // drop closes the pipe, signalling EOF to gh (or is a no-op if
        // gh already exited and closed its end first)

        let output = child.wait_with_output().map_err(|e| GhError::Command {
            detail: format!("failed waiting for gh: {e}"),
        })?;

        finalize_stdin_result(write_err, output, &self.slug.host)
    }
}

/// Turn a [`GithubClient::run_gh_with_stdin`] child's outcome into a
/// result. Non-zero exit always wins and is classified from gh's own
/// stderr/stdout — a failed stdin write is expected and uninteresting in
/// that case (gh said its piece before or instead of reading input). Only
/// when gh still somehow exited 0 despite the write failing is the write
/// error itself surfaced, since silently returning stale/empty stdout then
/// would be worse.
fn finalize_stdin_result(
    write_err: Option<std::io::Error>,
    output: std::process::Output,
    host: &str,
) -> Result<Vec<u8>, GhError> {
    if !output.status.success() {
        let stderr = crate::command::decode_output(&output.stderr);
        let stdout = crate::command::decode_output(&output.stdout);
        return Err(classify_failure(&stderr, &stdout, host));
    }
    if let Some(err) = write_err {
        return Err(GhError::Command {
            detail: format!("failed writing to gh stdin: {err}"),
        });
    }
    Ok(output.stdout)
}

/// Locate the `gh` binary: `DV_GH` env var, then a `PATH` lookup, then
/// (Windows only) the GitHub CLI installer's default install path. Cached
/// by the caller ([`GithubClient::for_repo`]) — this itself does the
/// lookup fresh every time it's called.
fn resolve_gh_path() -> Result<PathBuf, GhError> {
    if let Ok(value) = std::env::var("DV_GH")
        && !value.is_empty()
    {
        return Ok(PathBuf::from(value));
    }

    if let Some(path) = find_on_path("gh") {
        return Ok(path);
    }

    #[cfg(windows)]
    {
        let fallback = PathBuf::from(r"C:\Program Files\GitHub CLI\gh.exe");
        if fallback.is_file() {
            return Ok(fallback);
        }
    }

    Err(GhError::NotFound)
}

/// `where gh` (Windows) / `which gh` (unix). Host-side only, so a plain
/// `std::process::Command` is fine here (see the module doc) — but it
/// still needs `CREATE_NO_WINDOW` on Windows, same as every other spawn in
/// dv, or this flashes a console window.
fn find_on_path(program: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let finder = "where";
    #[cfg(not(windows))]
    let finder = "which";

    let mut cmd = Command::new(finder);
    cmd.arg(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::command::CREATE_NO_WINDOW);
    }

    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    // `where` can print more than one match (one per PATH hit); the first
    // is what would actually run.
    let text = crate::command::decode_output(&output.stdout);
    let first = text.lines().next()?.trim();
    if first.is_empty() {
        None
    } else {
        Some(PathBuf::from(first))
    }
}

/// Turn `gh`'s stderr (and, for `gh api` calls, stdout — the HTTP response
/// body, which is where the actually-useful diagnosis often lives, e.g.
/// a per-comment 422's `errors[]`) into a [`GhError`], recognizing the
/// auth-failure case by the same guidance gh itself prints (`gh auth
/// login`) rather than trying to parse exit codes, which vary by
/// subcommand.
fn classify_failure(stderr: &str, stdout: &str, host: &str) -> GhError {
    let lower = stderr.to_lowercase();
    let auth_markers = [
        "gh auth login",
        "not logged into",
        "no oauth token",
        "authentication failed",
        "requires authentication",
    ];
    if auth_markers.iter().any(|marker| lower.contains(marker)) {
        return GhError::NotAuthenticated {
            host: host.to_string(),
        };
    }

    let mut detail = stderr.trim().to_string();
    if detail.is_empty() {
        detail = "gh exited with a non-zero status and no error output".to_string();
    }
    crate::command::truncate_lossy(&mut detail, 2000);

    let stdout = stdout.trim();
    if !stdout.is_empty() {
        let mut response = stdout.to_string();
        crate::command::truncate_lossy(&mut response, 2000);
        detail.push_str("\nresponse: ");
        detail.push_str(&response);
    }

    GhError::Command { detail }
}

/// Best-effort detection of `gh pr create`'s "the branch doesn't have a
/// remote counterpart" failure, so it can be remapped to
/// [`GhError::BranchNotPushed`] instead of a raw gh error. Deliberately
/// broad: worst case (no match) just falls back to the generic
/// [`GhError::Command`], which is still correct, just less friendly.
fn looks_like_unpushed_branch(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    lower.contains("no commits between")
        || lower.contains("head sha can't be blank")
        || lower.contains("head ref must be a branch on the current repository")
        || lower.contains("must first push")
        || (lower.contains("branch") && lower.contains("does not exist"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_failure_detects_auth_markers() {
        let err = classify_failure(
            "To get started with GitHub CLI, please run: gh auth login",
            "",
            "github.com",
        );
        assert!(matches!(err, GhError::NotAuthenticated { .. }));
    }

    #[test]
    fn classify_failure_falls_back_to_command_error() {
        let err = classify_failure("HTTP 404: Not Found", "", "github.com");
        match err {
            GhError::Command { detail } => assert!(detail.contains("404")),
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn classify_failure_appends_nonempty_stdout_as_response() {
        // `gh api`'s terse stderr line doesn't say *which* comment was
        // rejected; the HTTP response body on stdout does.
        let err = classify_failure(
            "gh: 3 validation errors",
            r#"{"errors":[{"field":"line","message":"line must be part of the diff"}]}"#,
            "github.com",
        );
        match err {
            GhError::Command { detail } => {
                assert!(detail.contains("3 validation errors"));
                assert!(detail.contains("response:"));
                assert!(detail.contains("line must be part of the diff"));
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn classify_failure_omits_response_line_when_stdout_is_empty() {
        let err = classify_failure("HTTP 404: Not Found", "   \n", "github.com");
        match err {
            GhError::Command { detail } => assert!(!detail.contains("response:")),
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn looks_like_unpushed_branch_matches_common_gh_wording() {
        assert!(looks_like_unpushed_branch(
            "pull request create failed: GraphQL: No commits between main and feature (createPullRequest)"
        ));
        assert!(!looks_like_unpushed_branch("HTTP 422: Validation Failed"));
    }

    #[test]
    fn looks_like_unpushed_branch_matches_noninteractive_push_wording() {
        // gh's actual message for a non-interactive session with an
        // unpushed branch (no `--head` given).
        assert!(looks_like_unpushed_branch(
            "aborted: you must first push the current branch to a remote, or use the --head flag"
        ));
    }

    #[cfg(windows)]
    fn fake_exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }

    #[cfg(unix)]
    fn fake_exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code)
    }

    #[test]
    fn stdin_write_failure_does_not_mask_ghs_stderr_on_nonzero_exit() {
        // gh exited before draining stdin (unauthenticated, connection
        // refused, ...): the write fails (broken pipe), but gh's own
        // non-zero exit and stderr are still what must be reported.
        let write_err = Some(std::io::Error::from_raw_os_error(232));
        let output = std::process::Output {
            status: fake_exit_status(1),
            stdout: Vec::new(),
            stderr: b"gh: not logged into any GitHub hosts".to_vec(),
        };
        let err = finalize_stdin_result(write_err, output, "github.com").unwrap_err();
        assert!(matches!(err, GhError::NotAuthenticated { host } if host == "github.com"));
    }

    #[test]
    fn stdin_write_failure_is_surfaced_only_when_process_exits_zero_anyway() {
        let write_err = Some(std::io::Error::from_raw_os_error(232));
        let output = std::process::Output {
            status: fake_exit_status(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        let err = finalize_stdin_result(write_err, output, "github.com").unwrap_err();
        match err {
            GhError::Command { detail } => assert!(detail.contains("failed writing to gh stdin")),
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn no_write_error_and_success_returns_stdout() {
        let output = std::process::Output {
            status: fake_exit_status(0),
            stdout: b"ok".to_vec(),
            stderr: Vec::new(),
        };
        let out = finalize_stdin_result(None, output, "github.com").unwrap();
        assert_eq!(out, b"ok");
    }

    /// Guards the env-mutating test below: `std::env::set_var` races under
    /// the parallel test harness (every test in this file otherwise runs
    /// concurrently), so hold this for the duration of any test that
    /// touches process-global env state.
    static ENV_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn resolve_gh_path_honors_dv_gh_env_var() {
        let _guard = test_env_lock();
        // SAFETY: test-only env mutation, scoped to this process; guarded
        // by `test_env_lock` above against concurrent access from other
        // tests in this crate that touch env vars.
        unsafe {
            std::env::set_var("DV_GH", r"D:\fake\gh.exe");
        }
        let resolved = resolve_gh_path().unwrap();
        assert_eq!(resolved, PathBuf::from(r"D:\fake\gh.exe"));
        unsafe {
            std::env::remove_var("DV_GH");
        }
    }
}
