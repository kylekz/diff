//! Running `gh`, the one way dv reaches GitHub (docs/PLAN.md D7, §9.2).
//!
//! - **The program** is the one `DV_GH` names, else `gh` on `PATH` ([`program`]); tests name
//!   `mock_gh`.
//! - **Its environment** ([`POLICY_ENV`], [`DROPPED_ENV`]): no prompt, no update notice, no
//!   pager, no colour, and none of the inherited variables that make gh print for a terminal
//!   or log its requests, so what it prints is the same on every machine. It runs through
//!   `toolbox_process`: no console window on Windows, a tree of its own that a timeout kills.
//! - **The repository is always named**: `--repo=<host>/<owner>/<repo>` for gh's own commands,
//!   `--hostname=<host>` and a `repos/<owner>/<repo>/…` endpoint for `gh api`. Nothing depends
//!   on the working directory, and a name is always the value of its option, whatever it
//!   starts with.
//! - **A request's body goes on stdin** as JSON (`--input -`), a GraphQL request whole
//!   (`{"query", "variables"}`): `-f`/`-F` arguments can't carry nested inputs, and a long
//!   comment would meet Windows' 32K command-line limit.
//! - **One answer.** A single `gh api` call asks for the response's head (`--include`), so its
//!   status and rate-limit headers are read as GitHub sent them; every call ends as its JSON
//!   or a [`GithubError`] kind ([`response`]).
//! - **Pace.** Calls run one at a time, and mutations at least [`MUTATION_GAP`] apart, for
//!   GitHub's secondary rate limits.
//! - **Lists** are read whole ([`pages`]).
//! - **Which hosts are GitHub's** ([`Gh::is_github_host`], PLAN §9.1): `github.com`, a
//!   `*.ghe.com` tenant and the host `GH_HOST` names, and otherwise only a host gh holds a
//!   stored login for (`gh auth status --hostname=<host>`; a yes is remembered, a no is asked
//!   again, so a `gh auth login` made meanwhile is seen). Every call checks its host first
//!   and ends as [`GithubError::NotGithub`] without running gh for any other, wherever the
//!   host came from: gh is never pointed at another forge, and a token in the environment
//!   ([`ENTERPRISE_TOKEN_ENV`]) never reaches a host gh itself wouldn't contact.
//! - **Signed in or not** ([`Gh::auth_status`]) is the setup check's question (PLAN §12.2),
//!   asked through the same runner.

mod pages;
mod response;

use std::collections::HashSet;
use std::ffi::OsString;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use toolbox_process::{CancelToken, Command, ProcessError, Stop};

pub use pages::{Connection, MAX_PAGES, Nested, PAGE_SIZE, Paged, Truncated};
pub use response::says_signed_out;

use super::error::{GhFailure, GithubError, unexpected};
use super::repo::{GithubRepo, is_host, is_known_host, normalize_host};
use crate::review::event::now_ms;

/// The variable naming the program dv runs as `gh` (PLAN D7; tests point it at `mock_gh`).
pub const GH_VAR: &str = "DV_GH";

/// Environment variables every gh call gets (module docs).
pub const POLICY_ENV: &[(&str, &str)] = &[
    ("GH_PROMPT_DISABLED", "1"),
    ("GH_NO_UPDATE_NOTIFIER", "1"),
    ("GH_PAGER", "cat"),
    ("NO_COLOR", "1"),
];

/// Inherited variables removed from every gh call: `GH_FORCE_TTY` makes gh print for a
/// terminal, `CLICOLOR_FORCE` colours its output whatever `NO_COLOR` says, and `GH_DEBUG` and
/// `DEBUG` log its requests to stderr, which dv reads.
pub const DROPPED_ENV: &[&str] = &["GH_FORCE_TTY", "CLICOLOR_FORCE", "GH_DEBUG", "DEBUG"];

/// The variables that hand gh a token for any GitHub Enterprise host. The call that asks
/// whether gh holds a login for a host runs without them, so only a stored login answers.
pub const ENTERPRISE_TOKEN_ENV: &[&str] = &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"];

/// The variable naming the host gh uses when a command names none.
const GH_HOST_VAR: &str = "GH_HOST";

/// How long a call may take.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// How long a mutation that posts a comment or a review may take.
pub const POST_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a `--paginate` list may take: one gh call, many requests.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(120);
/// The least time between one mutation's end and the next one's start.
pub const MUTATION_GAP: Duration = Duration::from_secs(1);

/// The program dv runs as `gh`: what `DV_GH` names (`named`, its value when set and not
/// empty), else `gh` on `PATH`.
pub fn program(named: Option<OsString>) -> OsString {
    named
        .filter(|named| !named.is_empty())
        .unwrap_or_else(|| OsString::from("gh"))
}

/// How a [`Gh`] runs: the program and its pace. Tests shorten the times and overlay the
/// environment (`DV_MOCK_GH`).
#[derive(Debug, Clone)]
pub struct GhOptions {
    pub program: OsString,
    /// Environment changes applied before the policy: `Some` sets a variable, `None` removes
    /// it.
    pub env: Vec<(OsString, Option<OsString>)>,
    pub timeout: Duration,
    pub post_timeout: Duration,
    pub list_timeout: Duration,
    pub mutation_gap: Duration,
}

impl GhOptions {
    /// `program` ([`program`]) at PLAN §9.2's pace.
    pub fn new(program: OsString) -> Self {
        Self {
            program,
            env: Vec::new(),
            timeout: TIMEOUT,
            post_timeout: POST_TIMEOUT,
            list_timeout: LIST_TIMEOUT,
            mutation_gap: MUTATION_GAP,
        }
    }
}

/// A GraphQL mutation's kind, which sets its timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// A state change: resolving a thread, a Viewed mark ([`GhOptions::timeout`]).
    Change,
    /// Posting a comment or a review ([`GhOptions::post_timeout`]).
    Post,
}

/// What a call is, for its timeout and its pace.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Read,
    List,
    Mutation(Mutation),
}

/// One gh invocation.
struct Call<'a> {
    args: Vec<String>,
    /// The request's body, written to gh's stdin.
    body: Option<Vec<u8>>,
    /// The host it asks, which a failed login is reported for.
    host: &'a str,
    kind: Kind,
    /// Variables this one call runs without, besides [`DROPPED_ENV`].
    without: &'static [&'static str],
}

/// What `gh auth status` said and how it ended ([`Gh::auth_status`]). gh exits 0 when every
/// account it was asked about works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStatus {
    /// `None` when a signal ended it.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The `gh` runner (module docs). One per process, shared by everything that reaches GitHub,
/// so its pace holds across them.
#[derive(Debug)]
pub struct Gh {
    options: GhOptions,
    /// Held for the length of each call, so calls run one at a time: when the last mutation
    /// ended.
    pace: Mutex<Option<Instant>>,
    /// The hosts gh said it holds a login for.
    logins: Mutex<HashSet<String>>,
}

impl Gh {
    pub fn new(options: GhOptions) -> Self {
        Self {
            options,
            pace: Mutex::new(None),
            logins: Mutex::new(HashSet::new()),
        }
    }

    /// `gh auth status`: whether gh is signed in, over every host it knows. An error only
    /// when gh couldn't be run to its end ([`GithubError::GhMissing`], a timeout).
    pub fn auth_status(&self) -> Result<AuthStatus, GithubError> {
        self.ask_auth_status(None)
    }

    /// Runs [`auth_status_call`] and returns what gh said, however it exited.
    fn ask_auth_status(&self, host: Option<&str>) -> Result<AuthStatus, GithubError> {
        let output = self.run_to_end(&auth_status_call(host))?;
        Ok(AuthStatus {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Whether `host` is GitHub's or a GitHub Enterprise host (PLAN §9.1): `github.com`, a
    /// `*.ghe.com` tenant, the host `GH_HOST` names, or one gh holds a stored login for. Only
    /// the last asks gh. A yes is remembered; a no (gh has no login, is missing, timed out)
    /// is asked again the next time, so signing in is seen without a restart.
    pub fn is_github_host(&self, host: &str) -> bool {
        if !is_host(host) {
            return false;
        }
        let host = normalize_host(host);
        let gh_host = self.var(GH_HOST_VAR);
        if is_known_host(&host, gh_host.as_deref().and_then(|named| named.to_str())) {
            return true;
        }
        // Held while gh answers, so a host is asked about once however many ask at once.
        let mut logins = self.logins.lock().unwrap_or_else(PoisonError::into_inner);
        if logins.contains(&host) {
            return true;
        }
        match self.ask_auth_status(Some(&host)) {
            Ok(status) => {
                // A login gh holds but couldn't check (expired, or GitHub out of reach) is a
                // login all the same: "X Failed to log in to <host> account <login>".
                let stored = format!("log in to {host} account");
                let known = status.code == Some(0)
                    || status.stdout.contains(&stored)
                    || status.stderr.contains(&stored);
                if known {
                    logins.insert(host);
                }
                known
            }
            Err(error) => {
                log::debug!("gh auth status --hostname={host}: {error}");
                false
            }
        }
    }

    /// A variable as gh would see it: the overlay's last word on it, else this process's.
    fn var(&self, name: &str) -> Option<OsString> {
        match self
            .options
            .env
            .iter()
            .rev()
            .find(|(variable, _)| variable == name)
        {
            Some((_, value)) => value.clone(),
            None => std::env::var_os(name),
        }
    }

    /// One of gh's own commands about `repo`, as JSON: `gh <args> --repo=<host>/<owner>/<repo>`
    /// (`pr view 482 --json …`, `pr list --json …`).
    pub fn repo_json(&self, repo: &GithubRepo, args: &[&str]) -> Result<Value, GithubError> {
        let mut args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        args.push(format!("--repo={}", repo.selector()));
        let call = Call {
            args,
            body: None,
            host: &repo.host,
            kind: Kind::Read,
            without: &[],
        };
        response::json(&call.args, &self.run(&call)?)
    }

    /// One REST resource: `gh api <endpoint> --hostname=<host> --include` (`user`).
    pub fn rest(&self, host: &str, endpoint: &str) -> Result<Value, GithubError> {
        let call = Call {
            args: api_args(endpoint, host, &["--include"]),
            body: None,
            host,
            kind: Kind::Read,
            without: &[],
        };
        response::json(&call.args, &self.run(&call)?)
    }

    /// The `data` of a GraphQL query.
    pub fn graphql(
        &self,
        host: &str,
        query: &str,
        variables: &Map<String, Value>,
    ) -> Result<Value, GithubError> {
        self.graphql_call(host, query, variables, Kind::Read)
    }

    /// The `data` of a GraphQL mutation, paced after the one before it.
    pub fn mutate(
        &self,
        host: &str,
        query: &str,
        variables: &Map<String, Value>,
        mutation: Mutation,
    ) -> Result<Value, GithubError> {
        self.graphql_call(host, query, variables, Kind::Mutation(mutation))
    }

    /// `gh api graphql --hostname=<host> --include --input -`, the request on stdin.
    fn graphql_call(
        &self,
        host: &str,
        query: &str,
        variables: &Map<String, Value>,
        kind: Kind,
    ) -> Result<Value, GithubError> {
        let call = Call {
            args: api_args("graphql", host, &["--include", "--input", "-"]),
            body: Some(
                json!({"query": query, "variables": variables})
                    .to_string()
                    .into_bytes(),
            ),
            host,
            kind,
            without: &[],
        };
        let mut answer = response::json(&call.args, &self.run(&call)?)?;
        match answer.get_mut("data").map(Value::take) {
            Some(data) if !data.is_null() => Ok(data),
            _ => Err(unexpected(
                "api graphql",
                "the answer has no data",
                &answer.to_string(),
            )),
        }
    }

    /// The process `call` runs: the program with the overlay applied, then the policy, so the
    /// policy always wins, and the request's body as its stdin.
    fn command(&self, call: &Call<'_>) -> Command {
        let command = self.options.env.iter().fold(
            Command::new(&self.options.program).args(&call.args),
            |command, (name, value)| match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            },
        );
        let command = DROPPED_ENV
            .iter()
            .chain(call.without)
            .fold(command, |command, name| command.env_remove(name));
        let command = POLICY_ENV
            .iter()
            .fold(command, |command, (name, value)| command.env(name, value))
            .stop(Stop::Kill);
        match &call.body {
            Some(body) => command.input(body.clone()),
            None => command,
        }
    }

    /// Runs `call` in its turn and returns the response's body, or the [`GithubError`] its
    /// failure maps to.
    fn run(&self, call: &Call<'_>) -> Result<Vec<u8>, GithubError> {
        if !self.is_github_host(call.host) {
            return Err(GithubError::NotGithub {
                host: call.host.to_string(),
            });
        }
        let output = self.run_to_end(call)?;
        response::outcome(
            &call.args,
            call.host,
            output.status.code(),
            &output.stdout,
            &output.stderr,
            now_ms(),
        )
    }

    /// Runs `call` in its turn, to its end, and returns what gh printed however it exited;
    /// an error when it couldn't be started or outran its timeout.
    fn run_to_end(&self, call: &Call<'_>) -> Result<toolbox_process::Output, GithubError> {
        let options = &self.options;
        let timeout = match call.kind {
            Kind::Read | Kind::Mutation(Mutation::Change) => options.timeout,
            Kind::Mutation(Mutation::Post) => options.post_timeout,
            Kind::List => options.list_timeout,
        };

        // A panic in another call's thread leaves the pace as it stood, which is still right.
        let mut last_mutation = self.pace.lock().unwrap_or_else(PoisonError::into_inner);
        std::thread::sleep(wait(
            call.kind,
            last_mutation.map(|ended| ended.elapsed()),
            options.mutation_gap,
        ));
        let started = Instant::now();
        let ran = toolbox_process::run(&self.command(call), timeout, &CancelToken::new());
        if matches!(call.kind, Kind::Mutation(_)) {
            *last_mutation = Some(Instant::now());
        }
        drop(last_mutation);
        log::debug!("gh {} in {:?}", call.args.join(" "), started.elapsed());

        let failure = |stderr: &str| GhFailure::new(&call.args, None, stderr, "");
        ran.map_err(|error| match error {
            ProcessError::Start { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
                GithubError::GhMissing {
                    program: options.program.to_string_lossy().into_owned(),
                }
            }
            ProcessError::TimedOut { .. } => GithubError::Unavailable {
                reason: format!("gh didn't answer within {} s", timeout.as_secs_f32()),
                failure: failure(""),
            },
            error => GithubError::Gh {
                failure: failure(&error.to_string()),
            },
        })
    }
}

/// `gh auth status`, or with a `host` `gh auth status --hostname=<host>` run without
/// [`ENTERPRISE_TOKEN_ENV`], so that only a login gh has stored for that host answers.
fn auth_status_call(host: Option<&str>) -> Call<'_> {
    let mut args = vec!["auth".to_string(), "status".to_string()];
    if let Some(host) = host {
        args.push(format!("--hostname={host}"));
    }
    Call {
        args,
        body: None,
        host: host.unwrap_or_default(),
        kind: Kind::Read,
        without: match host {
            Some(_) => ENTERPRISE_TOKEN_ENV,
            None => &[],
        },
    }
}

/// How long a call waits before it starts: a mutation until `gap` after the last one ended
/// (`since_mutation` ago), anything else not at all.
fn wait(kind: Kind, since_mutation: Option<Duration>, gap: Duration) -> Duration {
    match (kind, since_mutation) {
        (Kind::Mutation(_), Some(since)) => gap.saturating_sub(since),
        _ => Duration::ZERO,
    }
}

/// `api <endpoint> --hostname=<host> <flags…>`.
fn api_args(endpoint: &str, host: &str, flags: &[&str]) -> Vec<String> {
    [
        "api".to_string(),
        endpoint.to_string(),
        format!("--hostname={host}"),
    ]
    .into_iter()
    .chain(flags.iter().map(|flag| flag.to_string()))
    .collect()
}

#[cfg(test)]
pub(super) mod tests;
