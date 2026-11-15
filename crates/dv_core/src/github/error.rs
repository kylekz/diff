//! Why a GitHub operation failed (docs/PLAN.md §9.1–§9.3).

use crate::backend::BackendError;

/// How much of `gh`'s stderr, and of the response body, a failure keeps (PLAN §9.2).
pub const KEPT_BYTES: usize = 2048;

/// What `gh` did when a call failed, kept for the detail line (PLAN §9.2): its arguments (a
/// request's body goes on stdin, so they hold no comment text), how it ended, and the first
/// [`KEPT_BYTES`] of its stderr and of the response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhFailure {
    pub args: Vec<String>,
    /// `None` when it never ran to an exit: killed by a signal, timed out, not started.
    pub code: Option<i32>,
    pub stderr: String,
    pub body: String,
}

impl GhFailure {
    pub(super) fn new(args: &[String], code: Option<i32>, stderr: &str, body: &str) -> Self {
        Self {
            args: args.to_vec(),
            code,
            stderr: kept(stderr),
            body: kept(body),
        }
    }

    /// The first line gh wrote to stderr, without its own `gh: ` prefix.
    pub(super) fn first_line(&self) -> &str {
        self.stderr
            .lines()
            .map(|line| line.trim().trim_start_matches("gh: "))
            .find(|line| !line.is_empty())
            .unwrap_or("it printed nothing")
    }

    fn detail(&self) -> String {
        let ended = match self.code {
            Some(code) => format!("exit code {code}"),
            None => "no exit code".to_string(),
        };
        [
            format!("$ gh {}", self.args.join(" ")),
            ended,
            self.stderr.trim_end().to_string(),
            self.body.trim_end().to_string(),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
    }
}

/// The subcommand in gh's arguments, for a headline: `pr view`, `api graphql`, `api`.
pub(super) fn subcommand(args: &[String]) -> String {
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .take_while(|arg| !arg.starts_with('-'))
        .take(2)
        .collect();
    match words.as_slice() {
        ["api", "graphql"] => "api graphql".to_string(),
        ["api", ..] => "api".to_string(),
        words => words.join(" "),
    }
}

/// The first [`KEPT_BYTES`] of `text`, cut at a character.
fn kept(text: &str) -> String {
    text[..text.floor_char_boundary(KEPT_BYTES)].to_string()
}

/// Why a GitHub operation failed.
///
/// `Display` is a one-line summary; [`GithubError::detail`] holds what `gh` said, and
/// [`GithubError::fix`] the command that puts it right, when there is one. dv never runs that
/// command itself (PLAN §9.2).
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    /// git failed where the repository lives (reading its remotes).
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// The clone has no GitHub repository dv can pick (PLAN §9.1). `remotes` names the remotes
    /// on GitHub: none of them, or several with nothing to choose between them. `elsewhere`
    /// are the others: local paths, other forges, hosts gh has no login for.
    #[error("{}", no_repository(.remotes, .elsewhere))]
    NoRepository {
        remotes: Vec<String>,
        elsewhere: Vec<Elsewhere>,
    },
    /// The mark `gh repo set-default` leaves on a remote (`remote.<name>.gh-resolved`) is
    /// neither `base` nor an `owner/repo`.
    #[error(
        "the remote {remote}'s `gh-resolved` mark ({value:?}) is neither `base` nor an \
         `owner/repo`; set it again with `gh repo set-default`"
    )]
    UnreadableDefault { remote: String, value: String },
    /// A call for a host that isn't GitHub's (PLAN §9.1): gh is never run for it, so no
    /// token reaches it.
    #[error(
        "{host} isn't GitHub, nor a GitHub Enterprise host gh is signed in to; if it is one, \
         `gh auth login --hostname {host}`"
    )]
    NotGithub { host: String },
    /// `gh repo set-default` named a repository (`owner/repo`) that no remote of the clone
    /// points at. dv fetches a pull request's commits from a remote, so it needs one.
    #[error(
        "`gh repo set-default` names {repository}, which no remote of this repository points \
         at; add a remote for it (dv fetches a pull request from a remote)"
    )]
    NoRemote { repository: String },
    /// A PR reference that names another repository than the clone's (PLAN §9.3).
    #[error("#{number} is in {repository}; open it from a clone of that repository")]
    OtherRepository { number: u64, repository: String },
    /// The program dv runs as `gh` isn't there: the setup check's state (PLAN §12.2).
    #[error("gh isn't installed: dv reaches GitHub through it (https://cli.github.com)")]
    GhMissing { program: String },
    /// gh has no login for the host, or GitHub refused the one it has (HTTP 401).
    #[error("gh isn't signed in to {host}")]
    NotAuthenticated { host: String, failure: GhFailure },
    /// HTTP 404, or a GraphQL `NOT_FOUND`: the PR or repository doesn't exist, or the login
    /// has no access to a private one.
    #[error(
        "GitHub found nothing there: the pull request or repository doesn't exist, or this \
         login can't see it"
    )]
    NotFound { failure: GhFailure },
    /// A primary or secondary rate limit. `reset` is when GitHub takes requests again, in ms
    /// since the Unix epoch, when it said (`retry-after`, `x-ratelimit-reset`).
    #[error("GitHub's rate limit is used up")]
    RateLimited {
        reset: Option<u64>,
        failure: GhFailure,
    },
    /// HTTP 422, with GitHub's `errors[]` messages verbatim.
    #[error("GitHub refused the request: {}", .messages.join("; "))]
    Validation {
        messages: Vec<String>,
        failure: GhFailure,
    },
    /// A GraphQL answer with `errors[]`, which gh reports as a failure.
    #[error("GitHub answered with an error: {}", .messages.join("; "))]
    Graphql {
        messages: Vec<String>,
        failure: GhFailure,
    },
    /// HTTP 5xx, a network failure or a timeout.
    #[error("GitHub can't be reached: {reason}")]
    Unavailable { reason: String, failure: GhFailure },
    /// Any other failure of gh.
    #[error("gh {} failed: {}", subcommand(&.failure.args), .failure.first_line())]
    Gh { failure: GhFailure },
    /// gh succeeded with something dv can't read. `output` is the first [`KEPT_BYTES`] of it.
    #[error("unexpected output from gh {command}: {detail}")]
    UnexpectedOutput {
        command: String,
        detail: String,
        output: String,
    },
}

impl GithubError {
    /// The diagnostics behind the headline: gh's command line, its exit code, its stderr and
    /// the response body.
    pub fn detail(&self) -> Option<String> {
        match self {
            GithubError::Backend(error) => error.detail(),
            GithubError::NotAuthenticated { failure, .. }
            | GithubError::NotFound { failure }
            | GithubError::RateLimited { failure, .. }
            | GithubError::Validation { failure, .. }
            | GithubError::Graphql { failure, .. }
            | GithubError::Unavailable { failure, .. }
            | GithubError::Gh { failure } => Some(failure.detail()),
            GithubError::UnexpectedOutput { output, .. } => Some(output.clone()),
            GithubError::NoRepository { .. }
            | GithubError::UnreadableDefault { .. }
            | GithubError::NotGithub { .. }
            | GithubError::NoRemote { .. }
            | GithubError::OtherRepository { .. }
            | GithubError::GhMissing { .. } => None,
        }
    }

    /// The kind as the CLI's JSON names it (PLAN §9.10).
    pub fn kind(&self) -> &'static str {
        match self {
            GithubError::Backend(_) => "git",
            GithubError::NoRepository { .. } => "no_repository",
            GithubError::UnreadableDefault { .. } => "unreadable_default",
            GithubError::NotGithub { .. } => "not_github",
            GithubError::NoRemote { .. } => "no_remote",
            GithubError::OtherRepository { .. } => "other_repository",
            GithubError::GhMissing { .. } => "gh_missing",
            GithubError::NotAuthenticated { .. } => "not_authenticated",
            GithubError::NotFound { .. } => "not_found",
            GithubError::RateLimited { .. } => "rate_limited",
            GithubError::Validation { .. } => "validation",
            GithubError::Graphql { .. } => "graphql",
            GithubError::Unavailable { .. } => "unavailable",
            GithubError::Gh { .. } => "gh",
            GithubError::UnexpectedOutput { .. } => "unexpected_output",
        }
    }

    /// The command the user runs to fix this, when one does.
    pub fn fix(&self) -> Option<String> {
        match self {
            GithubError::NoRepository { remotes, .. } if !remotes.is_empty() => {
                Some("gh repo set-default".to_string())
            }
            // The one host that may be a GitHub Enterprise one gh isn't signed in to.
            GithubError::NoRepository { elsewhere, .. } => match other_hosts(elsewhere)[..] {
                [host] => Some(format!("gh auth login --hostname {host}")),
                _ => None,
            },
            GithubError::UnreadableDefault { .. } => Some("gh repo set-default".to_string()),
            GithubError::NotAuthenticated { host, .. } if host == super::repo::GITHUB_COM => {
                Some("gh auth login".to_string())
            }
            GithubError::NotAuthenticated { host, .. } | GithubError::NotGithub { host } => {
                Some(format!("gh auth login --hostname {host}"))
            }
            _ => None,
        }
    }
}

/// A remote that isn't on GitHub ([`GithubError::NoRepository`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elsewhere {
    pub remote: String,
    /// The host its URL names, when it has a GitHub URL's shape: another forge's, or a
    /// GitHub Enterprise host gh has no login for. `None` for a local path and the like.
    pub host: Option<String>,
}

/// The hosts the remotes that aren't on GitHub are on, each once.
fn other_hosts(elsewhere: &[Elsewhere]) -> Vec<&str> {
    let mut hosts: Vec<&str> = elsewhere
        .iter()
        .filter_map(|remote| remote.host.as_deref())
        .collect();
    hosts.sort_unstable();
    hosts.dedup();
    hosts
}

fn no_repository(remotes: &[String], elsewhere: &[Elsewhere]) -> String {
    let names = || {
        elsewhere
            .iter()
            .map(|remote| remote.remote.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (remotes.is_empty(), elsewhere.is_empty()) {
        (true, true) => "this repository has no remotes".to_string(),
        (true, false) => {
            let places: Vec<String> = elsewhere
                .iter()
                .map(|remote| match &remote.host {
                    Some(host) => format!("{} is on {host}", remote.remote),
                    None => format!("{} has no GitHub URL", remote.remote),
                })
                .collect();
            let sign_in = match other_hosts(elsewhere)[..] {
                [] => String::new(),
                [host] => {
                    format!("; if that is GitHub Enterprise, `gh auth login --hostname {host}`")
                }
                _ => "; for a host that is GitHub Enterprise, `gh auth login --hostname <host>`"
                    .to_string(),
            };
            format!(
                "none of this repository's remotes is on GitHub: {}{sign_in}",
                places.join(", ")
            )
        }
        (false, true) => format!(
            "can't tell which of this repository's remotes ({}) is its GitHub repository; pick \
             one with `gh repo set-default`",
            remotes.join(", ")
        ),
        (false, false) => format!(
            "can't tell which of this repository's remotes ({}) is its GitHub repository; pick \
             one with `gh repo set-default` (not on GitHub: {})",
            remotes.join(", "),
            names()
        ),
    }
}

/// A [`GithubError::UnexpectedOutput`] of `gh <command>`, keeping the first [`KEPT_BYTES`]
/// of `output`.
pub(super) fn unexpected(command: &str, detail: &str, output: &str) -> GithubError {
    GithubError::UnexpectedOutput {
        command: command.to_string(),
        detail: detail.to_string(),
        output: kept(output),
    }
}
