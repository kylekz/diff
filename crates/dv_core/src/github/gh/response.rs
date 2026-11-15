//! What a finished `gh` call means (docs/PLAN.md §9.2): its JSON, or the [`GithubError`] kind
//! its failure is.
//!
//! gh reports a failed request in two ways. `gh api` prints the response's body on stdout
//! (after its head, with `--include`) and `gh: <message> (HTTP <status>)` on stderr; a GraphQL
//! answer with `errors[]` is a failure too, its messages on stderr. gh's own commands (`gh pr
//! view`) print `HTTP <status>: <message> (<url>)` or `GraphQL: <message> (<path>)` on stderr
//! alone. So the status is read from the head when there is one, else from stderr, and the
//! messages from the body when it has them.

use serde_json::Value;

use super::super::error::{GhFailure, GithubError, subcommand, unexpected};

/// The exit code gh ends with when it has no login to use.
const EXIT_AUTH: i32 = 4;

/// What gh prints when a login is missing or was refused: "You are not logged into any
/// GitHub hosts", "To get started with GitHub CLI, please run:  gh auth login", "Try
/// authenticating with:  gh auth login", "To re-authenticate, run: gh auth login -h
/// github.com".
const SIGNED_OUT_MARKERS: &[&str] = &["not logged in", "not logged into", "gh auth login"];

/// GitHub's words for a rate limit, primary ("API rate limit exceeded for user ID 1.") or
/// secondary ("You have exceeded a secondary rate limit", "… was submitted too quickly").
const RATE_LIMIT_MARKERS: &[&str] = &["rate limit", "was submitted too quickly"];

/// Whether what gh said is that nobody is signed in, or that the sign-in must be made again.
pub fn says_signed_out(said: &str) -> bool {
    let said = said.to_lowercase();
    SIGNED_OUT_MARKERS
        .iter()
        .any(|marker| said.contains(marker))
}

/// What gh's stderr holds when the request never reached GitHub: its own "error connecting to
/// api.github.com", and Go's network errors.
const NETWORK_MARKERS: &[&str] = &[
    "error connecting to",
    "dial tcp",
    "no such host",
    "i/o timeout",
    "handshake timeout",
    "connection refused",
    "connection reset",
    "network is unreachable",
    "context deadline exceeded",
];

/// A response as `gh api --include` prints it: the status line and the headers, a blank line,
/// the body. Without a head, all of it is the body.
struct Response<'a> {
    status: Option<u16>,
    /// Names lowercase.
    headers: Vec<(String, &'a str)>,
    body: &'a [u8],
}

impl<'a> Response<'a> {
    fn parse(stdout: &'a [u8]) -> Self {
        let (head, body) = stdout
            .starts_with(b"HTTP/")
            .then(|| split_head(stdout))
            .flatten()
            .unwrap_or((Vec::new(), stdout));
        let mut lines = head.into_iter();
        Self {
            // `HTTP/2.0 403 Forbidden`
            status: lines
                .next()
                .and_then(|line| line.split_whitespace().nth(1)?.parse().ok()),
            headers: lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim()))
                .collect(),
            body,
        }
    }

    fn header(&self, name: &str) -> Option<&'a str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| *value)
    }
}

/// The lines before the first blank one, and what follows it. gh ends the status line with
/// `\n` and each header with `\r\n`.
fn split_head(stdout: &[u8]) -> Option<(Vec<&str>, &[u8])> {
    let mut lines = Vec::new();
    let mut rest = stdout;
    loop {
        let end = rest.iter().position(|&b| b == b'\n')?;
        let line = &rest[..end];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        rest = &rest[end + 1..];
        if line.is_empty() {
            return Some((lines, rest));
        }
        lines.push(std::str::from_utf8(line).ok()?);
    }
}

/// The body of a call that succeeded, or the error a failed one maps to. `now_ms` is the
/// clock a `retry-after` counts from.
pub(super) fn outcome(
    args: &[String],
    host: &str,
    code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    now_ms: u64,
) -> Result<Vec<u8>, GithubError> {
    let response = Response::parse(stdout);
    if code == Some(0) {
        return Ok(response.body.to_vec());
    }
    let stderr = String::from_utf8_lossy(stderr);
    let body = String::from_utf8_lossy(response.body);
    let failure = GhFailure::new(args, code, &stderr, &body);
    let json: Option<Value> = serde_json::from_slice(response.body).ok();
    let errors = json
        .as_ref()
        .and_then(|json| json.get("errors")?.as_array())
        .map_or(&[][..], Vec::as_slice);
    let status = response.status.or_else(|| stderr_status(&stderr));
    // A GraphQL error's `type`, when every error has the same one.
    let graphql_type = |wanted: &str| {
        !errors.is_empty()
            && errors
                .iter()
                .all(|error| error.get("type").and_then(Value::as_str) == Some(wanted))
    };
    let lower = stderr.to_lowercase();
    let said = |marker: &str| lower.contains(marker);

    if code == Some(EXIT_AUTH) || status == Some(401) || says_signed_out(&stderr) {
        return Err(GithubError::NotAuthenticated {
            host: host.to_string(),
            failure,
        });
    }
    if status == Some(404) || graphql_type("NOT_FOUND") || said("could not resolve to a") {
        return Err(GithubError::NotFound { failure });
    }
    // Every response carries the rate-limit headers, so on their own they mean a limit only
    // when the request was refused (403) and nothing remains or GitHub says how long to wait.
    let limited = status == Some(429)
        || (status == Some(403)
            && (response.header("retry-after").is_some()
                || response.header("x-ratelimit-remaining") == Some("0")))
        || graphql_type("RATE_LIMITED")
        || RATE_LIMIT_MARKERS.iter().any(|marker| said(marker));
    if limited {
        let seconds = |name: &str| response.header(name)?.parse::<u64>().ok();
        let reset = seconds("retry-after")
            .map(|wait| now_ms.saturating_add(wait.saturating_mul(1000)))
            .or_else(|| {
                seconds("x-ratelimit-reset")
                    .filter(|_| response.header("x-ratelimit-remaining") == Some("0"))
                    .map(|at| at.saturating_mul(1000))
            });
        return Err(GithubError::RateLimited { reset, failure });
    }
    match status {
        Some(422) => {
            return Err(GithubError::Validation {
                messages: messages(errors, json.as_ref(), &failure),
                failure,
            });
        }
        Some(status) if status >= 500 => {
            return Err(GithubError::Unavailable {
                reason: format!("GitHub answered HTTP {status}"),
                failure,
            });
        }
        _ => {}
    }
    if !errors.is_empty() && status.is_none_or(|status| status == 200) {
        return Err(GithubError::Graphql {
            messages: messages(errors, None, &failure),
            failure,
        });
    }
    if let Some(message) = stderr
        .lines()
        .find_map(|line| line.strip_prefix("GraphQL: "))
    {
        return Err(GithubError::Graphql {
            messages: vec![message.trim().to_string()],
            failure,
        });
    }
    if NETWORK_MARKERS.iter().any(|marker| said(marker)) {
        return Err(GithubError::Unavailable {
            reason: failure.first_line().to_string(),
            failure,
        });
    }
    Err(GithubError::Gh { failure })
}

/// A successful call's body as JSON.
pub(super) fn json(args: &[String], body: &[u8]) -> Result<Value, GithubError> {
    serde_json::from_slice(body).map_err(|error| {
        unexpected(
            &subcommand(args),
            &format!("not JSON: {error}"),
            &String::from_utf8_lossy(body),
        )
    })
}

/// The status in gh's stderr: `gh: Not Found (HTTP 404)`, `HTTP 404: Not Found (<url>)`.
fn stderr_status(stderr: &str) -> Option<u16> {
    stderr.match_indices("HTTP ").find_map(|(at, marker)| {
        let rest = &stderr[at + marker.len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        (digits == 3).then(|| rest[..3].parse().ok()).flatten()
    })
}

/// GitHub's own words for a refused request, verbatim: each `errors[]` entry's `message` (or
/// the entry, when it is a string; an entry with neither as it is), else the body's
/// `message`, else what gh said.
fn messages(errors: &[Value], body: Option<&Value>, failure: &GhFailure) -> Vec<String> {
    let listed: Vec<String> = errors
        .iter()
        .map(|error| match error {
            Value::String(text) => text.clone(),
            other => match other.get("message").and_then(Value::as_str) {
                Some(message) => message.to_string(),
                None => other.to_string(),
            },
        })
        .collect();
    if !listed.is_empty() {
        return listed;
    }
    match body.and_then(|body| body.get("message")?.as_str()) {
        Some(message) => vec![message.to_string()],
        None => vec![failure.first_line().to_string()],
    }
}
