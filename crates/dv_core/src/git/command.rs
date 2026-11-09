//! The one place dv builds and runs a `git` process: the invocation policy (docs/PLAN.md §5.1,
//! §2.4, D4).
//!
//! Every git dv runs gets the same treatment, so its output is parseable and the same on every
//! machine whatever the user's configuration says about presentation:
//!
//! - `LC_ALL=C`: untranslated messages and C collation, because dv parses git's output.
//! - `--no-pager` and `GIT_PAGER=cat`: nothing waits on a pager.
//! - `GIT_TERMINAL_PROMPT=0`: a credential prompt fails instead of hanging a headless process.
//! - `GIT_OPTIONAL_LOCKS=0` and `-c diff.autoRefreshIndex=false`: read commands never refresh
//!   and rewrite the index, so dv never changes the user's index (PLAN §2.4) and never holds
//!   `index.lock` while an agent runs `git add`. `GIT_OPTIONAL_LOCKS` covers `git status`; the
//!   worktree forms of `git diff` (`git diff`, `git diff HEAD`) ignore it and rewrite the index
//!   after finding stat-dirty files unchanged, which only `diff.autoRefreshIndex=false` stops.
//!   The price: those diffs list a stat-dirty but unchanged file in `--name-status` (as `M`),
//!   while `--numstat` and the patch leave it out, so the file list keeps only entries that
//!   `--numstat` reports (PLAN §6).
//! - `-c core.quotepath=off`: paths print as UTF-8, not octal escapes.
//! - `-c color.ui=never -c color.diff=never`: no escape codes. `color.diff` needs its own
//!   override because a configured `color.diff=always` wins over `color.ui`.
//! - `-c diff.algorithm=myers`: the algorithm GitHub uses (PLAN D4), whatever `diff.algorithm`
//!   the user prefers. A diff driver's `diff.<driver>.algorithm` beats this key, so every
//!   `git diff` dv runs also passes `--diff-algorithm=myers`, which beats both; that and the
//!   other diff presentation settings (`diff.indentHeuristic`, `diff.submodule`,
//!   `diff.ignoreSubmodules`, textconv) are pinned by flags on the diff commands themselves
//!   (`diff::DIFF_PINS`).
//! - The inherited variables that point git at another repository, index or configuration, or
//!   that change how pathspecs and diffs are interpreted ([`DROPPED_ENV`]), are removed, because
//!   dv may be started from a git hook or an alias that exports them.
//! - It runs through `toolbox_process`: on Windows with no console window, and in a tree of
//!   its own, so a timeout kills git with everything it started (hooks, `ssh`, credential
//!   helpers), and whatever it leaves running when it exits is killed then.
//!
//! The user's own configuration otherwise applies (autocrlf, attributes, excludes): that is the
//! repo dv is reviewing. Tests replace it with the fixture's through an environment overlay
//! (`LocalBackendOptions::env`), applied before the policy so the policy always wins.

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use toolbox_process::{CancelToken, Command, NO_TIMEOUT, ProcessError, Stop};

use crate::backend::{BackendError, GitOpts, GitOutput};

/// `-c` settings every git call gets (module docs).
pub const POLICY_CONFIG: &[(&str, &str)] = &[
    ("core.quotepath", "off"),
    ("color.ui", "never"),
    ("color.diff", "never"),
    ("diff.algorithm", "myers"),
    ("diff.autoRefreshIndex", "false"),
];

/// Environment variables every git call gets (module docs).
pub const POLICY_ENV: &[(&str, &str)] = &[
    ("LC_ALL", "C"),
    ("GIT_PAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_OPTIONAL_LOCKS", "0"),
];

/// Inherited variables removed from every git call: they redirect git to another repository,
/// index or configuration, or change how pathspecs (dv uses `:(literal)` magic, which
/// `GIT_LITERAL_PATHSPECS` would disable) and diffs (`GIT_DIFF_OPTS` sets the context size;
/// `GIT_ATTR_SOURCE`, which `git --attr-source=<tree>` exports, reads `.gitattributes` from that
/// tree instead of the worktree and index, so `-diff`, `binary` and `eol` follow the wrong
/// rules, and fails every attribute lookup in a repo without commits when it names `HEAD`) are
/// interpreted. The test fixture's overlay (`dv_test_support::FixtureRepo::env_overlay`)
/// removes each of these too, so tests' reference git calls match; a test checks that.
pub const DROPPED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_QUARANTINE_PATH",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
    "GIT_DIFF_OPTS",
    "GIT_EXTERNAL_DIFF",
    "GIT_ATTR_SOURCE",
];

/// An environment change: `Some` sets the variable, `None` removes it.
pub type EnvOverlay = [(OsString, Option<OsString>)];

/// A `git` command running in `cwd` with `overlay` applied, then the policy (module docs), and
/// `index_file` as its index (`GIT_INDEX_FILE`), set after the policy removed any inherited one;
/// `None` is the worktree's own index. The caller adds the subcommand and its arguments.
fn command(cwd: &Path, overlay: &EnvOverlay, index_file: Option<&Path>) -> Command {
    let command = overlay.iter().fold(
        Command::new("git").current_dir(cwd),
        |command, (name, value)| match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        },
    );
    let command = DROPPED_ENV
        .iter()
        .fold(command, |command, name| command.env_remove(name));
    let command = POLICY_ENV
        .iter()
        .fold(command, |command, (name, value)| command.env(name, value));
    let command = match index_file {
        Some(index_file) => command.env("GIT_INDEX_FILE", index_file),
        None => command,
    };
    POLICY_CONFIG
        .iter()
        .fold(command.arg("--no-pager"), |command, (key, value)| {
            command.arg("-c").arg(format!("{key}={value}"))
        })
}

/// Applies an environment overlay to a `std` command: sets the `Some` variables, removes the
/// `None` ones. For tests that start a process of their own in a fixture's environment.
#[cfg(test)]
pub(crate) fn apply_overlay(command: &mut std::process::Command, overlay: &EnvOverlay) {
    for (name, value) in overlay {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
}

/// Runs `git <args>` in `cwd` (see [`command`]) and collects its output. Fails with
/// [`BackendError::Git`] when the exit code isn't one of `opts.ok_codes`, and with
/// [`BackendError::Timeout`] when `opts.timeout_ms` passes first, killing git and everything it
/// started. What git leaves running when it exits (an alias or hook that backgrounds a command)
/// is killed then; one that left git's tree on purpose and still holds its output pipes is
/// waited for a second at most.
///
/// `opts.index_file` is a store path only a backend can resolve; this runs with the worktree's
/// own index and refuses a call that names another ([`run_with_index`] takes the resolved path).
pub fn run(
    cwd: &Path,
    overlay: &EnvOverlay,
    args: &[&str],
    opts: &GitOpts,
) -> Result<GitOutput, BackendError> {
    if let Some(rel) = &opts.index_file {
        return Err(BackendError::InvalidPath {
            path: rel.clone(),
            reason: "an index file is resolved by a backend".into(),
        });
    }
    run_with_index(cwd, overlay, None, args, opts)
}

/// [`run`] with `index_file`, already resolved from `opts.index_file` by the backend, as the
/// call's index (`GIT_INDEX_FILE`).
pub fn run_with_index(
    cwd: &Path,
    overlay: &EnvOverlay,
    index_file: Option<&Path>,
    args: &[&str],
    opts: &GitOpts,
) -> Result<GitOutput, BackendError> {
    let owned_args = || args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let started = Instant::now();
    let command = command(cwd, overlay, index_file)
        .args(args)
        .stop(Stop::Kill);
    let command = match &opts.stdin {
        Some(bytes) => command.input(bytes.clone()),
        None => command,
    };
    let timeout = opts.timeout_ms.map_or(NO_TIMEOUT, Duration::from_millis);
    let output =
        toolbox_process::run(&command, timeout, &CancelToken::new()).map_err(
            |error| match error {
                ProcessError::Start { source, .. } => BackendError::Spawn {
                    args: owned_args(),
                    source,
                },
                ProcessError::TimedOut { .. } => BackendError::Timeout {
                    args: owned_args(),
                    timeout_ms: opts.timeout_ms.unwrap_or_default(),
                },
                other => BackendError::io("waiting for git in", cwd, std::io::Error::other(other)),
            },
        )?;
    // Where the time goes on a large review (PLAN §17's budgets): `RUST_LOG=dv_core=debug`.
    log::debug!(
        "git {} ({} bytes out) in {:?}",
        args.join(" "),
        output.stdout.len(),
        started.elapsed()
    );
    match output.status.code() {
        Some(code) if opts.ok_codes.contains(&code) => Ok(GitOutput {
            code,
            stdout: output.stdout,
            stderr: output.stderr,
        }),
        code => Err(BackendError::Git {
            args: owned_args(),
            code,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }),
    }
}
