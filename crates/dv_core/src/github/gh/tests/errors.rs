//! Every way a call fails, as `gh` reports it, and the [`GithubError`] it is (PLAN §9.2).

use super::*;
use crate::review::event::now_ms;

/// A response as `gh api --include` prints it.
fn with_head(status: &str, headers: &[(&str, &str)], body: Value) -> String {
    let headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    format!("HTTP/2.0 {status}\n{headers}\r\n{body}\n")
}

/// The error of a single REST call answered with `entry`'s stdout, stderr and exit code.
fn rest_error(entry: Value) -> GithubError {
    let mut entry = entry;
    entry["argv"] = json!(["api", "**"]);
    MockGh::new(json!([entry]))
        .gh
        .rest(HOST, "repos/acme/api/pulls/482")
        .unwrap_err()
}

/// The error of `gh pr view` answered with `stderr` and `exit`.
fn pr_view_error(stderr: &str, exit: u8) -> GithubError {
    MockGh::new(json!([{"argv": ["pr", "**"], "stderr": stderr, "exit": exit}]))
        .gh
        .repo_json(&repo(), &["pr", "view", "482", "--json", "number"])
        .unwrap_err()
}

#[test]
fn no_login_or_a_refused_one_is_not_authenticated() {
    let errors = [
        // No login at all: gh's own exit code and words.
        pr_view_error(
            "To get started with GitHub CLI, please run:  gh auth login\nAlternatively, \
             populate the GH_TOKEN environment variable with a GitHub API authentication token.\n",
            4,
        ),
        // A token GitHub no longer takes, from one of gh's own commands.
        pr_view_error(
            "HTTP 401: Bad credentials (https://api.github.com/graphql)\nTry authenticating \
             with:  gh auth login\n",
            1,
        ),
        // The same from `gh api`.
        rest_error(json!({
            "stdout": with_head("401 Unauthorized", &[], json!({"message": "Bad credentials", "status": "401"})),
            "stderr": "gh: Bad credentials (HTTP 401)\n",
            "exit": 1
        })),
        // Without a head, the status in stderr decides.
        rest_error(json!({"stderr": "gh: Bad credentials (HTTP 401)\n", "exit": 1})),
    ];
    for error in &errors {
        assert!(
            matches!(error, GithubError::NotAuthenticated { host, .. } if host == HOST),
            "{error:?}"
        );
        assert_eq!(error.to_string(), "gh isn't signed in to github.com");
        assert_eq!(error.kind(), "not_authenticated");
        assert_eq!(error.fix().as_deref(), Some("gh auth login"));
    }

    let enterprise = MockGh::with(
        json!([{"argv": ["**"], "stderr": "gh auth login\n", "exit": 4}]),
        |options| {
            options
                .env
                .push(("GH_HOST".into(), Some("ghe.example.com".into())));
        },
    )
    .gh
    .rest("ghe.example.com", "user")
    .unwrap_err();
    assert!(
        matches!(&enterprise, GithubError::NotAuthenticated { host, .. } if host == "ghe.example.com"),
        "{enterprise:?}"
    );
    assert_eq!(
        enterprise.fix().as_deref(),
        Some("gh auth login --hostname ghe.example.com")
    );
}

#[test]
fn a_404_or_a_graphql_not_found_is_not_found() {
    let graphql = MockGh::new(json!([{
        "argv": ["api", "graphql", "**"],
        "stdout": with_head("200 OK", &[], json!({
            "data": {"repository": {"pullRequest": null}},
            "errors": [{
                "type": "NOT_FOUND",
                "path": ["repository", "pullRequest"],
                "message": "Could not resolve to a PullRequest with the number of 999."
            }]
        })),
        "stderr": "gh: Could not resolve to a PullRequest with the number of 999.\n",
        "exit": 1
    }]))
    .gh
    .graphql(HOST, "query { … }", &Map::new())
    .unwrap_err();
    let errors = [
        rest_error(json!({
            "stdout": with_head("404 Not Found", &[], json!({"message": "Not Found", "status": "404"})),
            "stderr": "gh: Not Found (HTTP 404)\n",
            "exit": 1
        })),
        rest_error(json!({"stderr": "gh: Not Found (HTTP 404)\n", "exit": 1})),
        pr_view_error(
            "GraphQL: Could not resolve to a PullRequest with the number of 999. \
             (repository.pullRequest)\n",
            1,
        ),
        pr_view_error(
            "GraphQL: Could not resolve to a Repository with the name 'acme/api'. (repository)\n",
            1,
        ),
        graphql,
    ];
    for error in &errors {
        assert!(matches!(error, GithubError::NotFound { .. }), "{error:?}");
        assert_eq!(error.fix(), None);
        assert_eq!(error.kind(), "not_found");
    }
}

#[test]
fn a_rate_limit_says_when_it_ends_when_github_does() {
    let limited = |entry: Value| match rest_error(entry) {
        GithubError::RateLimited { reset, .. } => reset,
        other => panic!("{other:?}"),
    };
    let message = json!({"message": "API rate limit exceeded for user ID 1."});

    // A secondary limit: wait `retry-after` seconds from now.
    let before = now_ms();
    let reset = limited(json!({
        "stdout": with_head("403 Forbidden", &[("Retry-After", "60")], json!({
            "message": "You have exceeded a secondary rate limit. Please wait a few minutes \
                        before you try again."
        })),
        "stderr": "gh: You have exceeded a secondary rate limit. (HTTP 403)\n",
        "exit": 1
    }))
    .unwrap();
    assert!(
        (before + 60_000..=now_ms() + 60_000).contains(&reset),
        "{reset}"
    );

    // The primary limit: none left until `x-ratelimit-reset`, in seconds since the epoch.
    let headers = [
        ("X-Ratelimit-Remaining", "0"),
        ("X-Ratelimit-Reset", "1767225600"),
    ];
    assert_eq!(
        limited(json!({
            "stdout": with_head("403 Forbidden", &headers, message.clone()),
            "stderr": "gh: API rate limit exceeded for user ID 1. (HTTP 403)\n",
            "exit": 1
        })),
        Some(1_767_225_600_000)
    );
    assert_eq!(
        limited(json!({
            "stdout": with_head("429 Too Many Requests", &headers, message.clone()),
            "stderr": "gh: API rate limit exceeded for user ID 1. (HTTP 429)\n",
            "exit": 1
        })),
        Some(1_767_225_600_000)
    );
    // GitHub's words alone, with no head to read a time from.
    assert_eq!(
        limited(json!({
            "stderr": "gh: API rate limit exceeded for user ID 1. (HTTP 403)\n",
            "exit": 1
        })),
        None
    );
    let own_command = pr_view_error(
        "HTTP 403: API rate limit exceeded for user ID 1. (https://api.github.com/graphql)\n",
        1,
    );
    assert!(
        matches!(own_command, GithubError::RateLimited { reset: None, .. }),
        "{own_command:?}"
    );

    // GraphQL's primary limit is an answer with errors, not a status.
    let graphql = MockGh::new(json!([{
        "argv": ["api", "graphql", "**"],
        "stdout": with_head("200 OK", &headers, json!({
            "errors": [{"type": "RATE_LIMITED", "message": "API rate limit exceeded for user ID 1."}]
        })),
        "stderr": "gh: API rate limit exceeded for user ID 1.\n",
        "exit": 1
    }]))
    .gh
    .graphql(HOST, "query { … }", &Map::new())
    .unwrap_err();
    assert!(
        matches!(
            graphql,
            GithubError::RateLimited {
                reset: Some(1_767_225_600_000),
                ..
            }
        ),
        "{graphql:?}"
    );
}

#[test]
fn a_rate_limit_is_one_however_github_words_it() {
    // GraphQL errors without a type, as the secondary limits come, on a 200.
    for message in [
        "You have exceeded a secondary rate limit. Please wait a few minutes before you try again.",
        "was submitted too quickly",
        "API rate limit exceeded for user ID 1234.",
    ] {
        let error = MockGh::new(json!([{
            "argv": ["api", "graphql", "**"],
            "stdout": with_head(
                "200 OK",
                &[("X-Ratelimit-Remaining", "4000"), ("X-Ratelimit-Reset", "1767225600")],
                json!({"data": {"addPullRequestReviewThread": null}, "errors": [{"message": message}]})
            ),
            "stderr": format!("gh: {message}\n"),
            "exit": 1
        }]))
        .gh
        .mutate(HOST, "mutation { … }", &Map::new(), Mutation::Post)
        .unwrap_err();
        // Requests remain, so the headers say no time: the limit is the secondary one.
        assert!(
            matches!(error, GithubError::RateLimited { reset: None, .. }),
            "{message}: {error:?}"
        );
        assert_eq!(error.kind(), "rate_limited");
    }

    // The same from gh's own commands, which print the GraphQL error on stderr alone.
    for stderr in [
        "GraphQL: API rate limit exceeded for user ID 1234. (rateLimit)\n",
        "GraphQL: You have exceeded a secondary rate limit. Please wait a few minutes before \
         you try again. (viewer)\n",
        "GraphQL: was submitted too quickly (addPullRequestReview)\n",
        "HTTP 403: You have exceeded a secondary rate limit. (https://api.github.com/graphql)\n",
    ] {
        let error = pr_view_error(stderr, 1);
        assert!(
            matches!(error, GithubError::RateLimited { reset: None, .. }),
            "{stderr}: {error:?}"
        );
    }
}

#[test]
fn a_403_that_is_not_a_rate_limit_is_ghs_own_failure() {
    // Every response carries the rate-limit headers; only an exhausted one is a limit.
    let headers = [
        ("X-Ratelimit-Remaining", "4999"),
        ("X-Ratelimit-Reset", "1767225600"),
    ];
    let error = rest_error(json!({
        "stdout": with_head("403 Forbidden", &headers, json!({
            "message": "Resource not accessible by personal access token"
        })),
        "stderr": "gh: Resource not accessible by personal access token (HTTP 403)\n",
        "exit": 1
    }));
    assert!(matches!(error, GithubError::Gh { .. }), "{error:?}");
    assert_eq!(error.kind(), "gh");
    assert_eq!(
        error.to_string(),
        "gh api failed: Resource not accessible by personal access token (HTTP 403)"
    );
}

#[test]
fn a_422_keeps_githubs_messages_verbatim() {
    let validation = |body: Value| match rest_error(json!({
        "stdout": with_head("422 Unprocessable Entity", &[], body),
        "stderr": "gh: Validation Failed (HTTP 422)\n",
        "exit": 1
    })) {
        GithubError::Validation { messages, .. } => messages,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        validation(json!({
            "message": "Validation Failed",
            "errors": [
                {
                    "resource": "PullRequestReviewComment",
                    "code": "custom",
                    "field": "pull_request_review_thread.line",
                    "message": "pull_request_review_thread.line must be part of the diff"
                },
                "body is too long (maximum is 65536 characters)",
                {"resource": "PullRequestReview", "code": "missing_field", "field": "body"}
            ]
        })),
        [
            "pull_request_review_thread.line must be part of the diff".to_string(),
            "body is too long (maximum is 65536 characters)".to_string(),
            json!({"resource": "PullRequestReview", "code": "missing_field", "field": "body"})
                .to_string(),
        ]
    );
    assert_eq!(
        validation(
            json!({"message": "Unprocessable Entity: the commit is not part of the pull request"})
        ),
        ["Unprocessable Entity: the commit is not part of the pull request"]
    );

    let error = pr_view_error(
        "HTTP 422: Validation Failed (https://api.github.com/x)\n",
        1,
    );
    assert!(
        matches!(&error, GithubError::Validation { messages, .. }
            if messages == &["HTTP 422: Validation Failed (https://api.github.com/x)"]),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "GitHub refused the request: HTTP 422: Validation Failed (https://api.github.com/x)"
    );
    assert_eq!(error.kind(), "validation");
}

#[test]
fn a_graphql_answer_with_errors_is_a_graphql_error() {
    let mock = MockGh::new(json!([{
        "argv": ["api", "graphql", "**"],
        "stdout": with_head("200 OK", &[("X-Ratelimit-Remaining", "4990")], json!({
            "data": {"addPullRequestReviewThread": null},
            "errors": [
                {
                    "type": "UNPROCESSABLE",
                    "path": ["addPullRequestReviewThread"],
                    "message": "Line must be part of the diff"
                },
                {"message": "Variable $input of type AddPullRequestReviewThreadInput! was provided invalid value"}
            ]
        })),
        "stderr": "gh: Line must be part of the diff\nVariable $input of type \
                   AddPullRequestReviewThreadInput! was provided invalid value\n",
        "exit": 1
    }]));
    let error = mock
        .gh
        .mutate(HOST, "mutation { … }", &Map::new(), Mutation::Post)
        .unwrap_err();
    assert!(
        matches!(&error, GithubError::Graphql { messages, .. } if messages == &[
            "Line must be part of the diff",
            "Variable $input of type AddPullRequestReviewThreadInput! was provided invalid value",
        ]),
        "{error:?}"
    );

    assert_eq!(error.kind(), "graphql");

    // One of gh's own commands reports it on stderr alone.
    let error = pr_view_error(
        "GraphQL: Field 'nope' doesn't exist on type 'PullRequest' (repository.pullRequest.nope)\n",
        1,
    );
    assert!(
        matches!(&error, GithubError::Graphql { messages, .. } if messages
            == &["Field 'nope' doesn't exist on type 'PullRequest' (repository.pullRequest.nope)"]),
        "{error:?}"
    );
}

#[test]
fn a_5xx_or_a_network_failure_is_unavailable() {
    let errors = [
        rest_error(json!({
            "stdout": with_head("502 Bad Gateway", &[], json!({"message": "Server Error"})),
            "stderr": "gh: Server Error (HTTP 502)\n",
            "exit": 1
        })),
        pr_view_error(
            "HTTP 503: Service Unavailable (https://api.github.com/graphql)\n",
            1,
        ),
        pr_view_error(
            "error connecting to api.github.com\ncheck your internet connection or \
             https://githubstatus.com\n",
            1,
        ),
        pr_view_error(
            "Post \"https://api.github.com/graphql\": dial tcp: lookup api.github.com: no such host\n",
            1,
        ),
    ];
    for error in &errors {
        assert!(
            matches!(error, GithubError::Unavailable { .. }),
            "{error:?}"
        );
    }
    assert_eq!(
        errors[0].to_string(),
        "GitHub answered HTTP 502, a server error"
    );
    assert_eq!(
        errors[2].to_string(),
        "GitHub can't be reached: error connecting to api.github.com"
    );
}

#[test]
fn anything_else_is_ghs_own_failure_with_what_it_said() {
    // `mock_gh` without an entry for the call: exit 64 and a line on stderr.
    let mock = MockGh::new(json!([]));
    let error = mock
        .gh
        .repo_json(&repo(), &["pr", "view", "482", "--json", "number"])
        .unwrap_err();
    let GithubError::Gh { failure } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(failure.code, Some(64));
    assert!(
        failure.stderr.starts_with("mock_gh: no scenario entry"),
        "{failure:?}"
    );
    assert_eq!(failure.body, "");
    assert!(
        error
            .to_string()
            .starts_with("gh pr view failed: mock_gh: no scenario entry"),
        "{error}"
    );
    let detail = error.detail().unwrap();
    assert!(
        detail.starts_with(
            "$ gh pr view 482 --json number --repo=github.com/acme/api\nexit code 64\nmock_gh: no \
             scenario entry"
        ),
        "{detail}"
    );
}

#[test]
fn a_failure_keeps_two_kilobytes_of_stderr_and_of_the_body() {
    // Multi-byte characters, so a cut in the middle of one would show.
    let long = "é".repeat(4 * KEPT_BYTES);
    let error = rest_error(json!({
        "stdout": with_head("500 Internal Server Error", &[], json!({"message": long})),
        "stderr": format!("gh: {long} (HTTP 500)\n"),
        "exit": 1
    }));
    let GithubError::Unavailable { failure, .. } = &error else {
        panic!("{error:?}");
    };
    for kept in [&failure.stderr, &failure.body] {
        assert!(
            (KEPT_BYTES - 3..=KEPT_BYTES).contains(&kept.len()),
            "{}",
            kept.len()
        );
    }
    assert!(failure.stderr.starts_with("gh: ééé"));
    assert!(failure.body.starts_with("{\"message\":\"ééé"));
    assert_eq!(KEPT_BYTES, 2048);
}

#[test]
fn a_head_is_read_however_its_lines_end() {
    use super::response::outcome;
    let args = ["api".to_string(), "user".to_string()];
    let body = |stdout: &[u8]| outcome(&args, HOST, Some(0), stdout, b"", 0).unwrap();
    // As gh prints it, with only `\n`, with no headers, and with none at all.
    assert_eq!(
        body(b"HTTP/2.0 200 OK\nEtag: \"a\"\r\n\r\n{\"a\":1}"),
        b"{\"a\":1}"
    );
    assert_eq!(
        body(b"HTTP/1.1 200 OK\nEtag: \"a\"\n\n{\"a\":1}"),
        b"{\"a\":1}"
    );
    assert_eq!(body(b"HTTP/2.0 204 No Content\n\r\n"), b"");
    assert_eq!(body(b"{\"a\":1}\n"), b"{\"a\":1}\n");
    // A body that only looks like a head is a body.
    assert_eq!(body(b"HTTP/2.0 200 OK"), b"HTTP/2.0 200 OK");

    // Header names compare without case; the status comes from the head, not from stderr.
    let error = outcome(
        &args,
        HOST,
        Some(1),
        b"HTTP/2.0 429 Too Many Requests\nretry-after: 5\r\n\r\n{}",
        b"gh: slow down (HTTP 403)\n",
        1_000,
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            GithubError::RateLimited {
                reset: Some(6_000),
                ..
            }
        ),
        "{error:?}"
    );
}
