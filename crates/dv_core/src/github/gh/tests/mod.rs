//! Tests for the `gh` runner against `mock_gh` (docs/PLAN.md §9.11): what it runs, what it
//! sends, its pace, every failure's kind ([`errors`]), lists read whole ([`pages`]), and
//! what it says about a host and a sign-in ([`hosts`]).

mod errors;
mod hosts;
mod pages;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use dv_test_support::mock_gh;
use serde_json::{Map, Value, json};

use super::*;
use crate::github::KEPT_BYTES;

const HOST: &str = "github.com";

/// A [`Gh`] that runs `mock_gh` with a scenario of its own, and the log of its calls.
pub(in crate::github) struct MockGh {
    pub gh: Gh,
    log: PathBuf,
    _dir: tempfile::TempDir,
}

impl MockGh {
    /// `mock_gh` answering from `calls`, a scenario file's `calls` array.
    pub fn new(calls: Value) -> Self {
        Self::with(calls, |_| {})
    }

    /// [`MockGh::new`] with the runner's options changed by `tweak`.
    pub fn with(calls: Value, tweak: impl FnOnce(&mut GhOptions)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let scenario = dir.path().join("scenario.json");
        std::fs::write(&scenario, json!({ "calls": calls }).to_string()).unwrap();
        let log = dir.path().join("gh_calls.jsonl");
        let mut options = GhOptions::new(mock_gh().into_os_string());
        options.env = vec![
            ("DV_MOCK_GH".into(), Some(scenario.into_os_string())),
            ("DV_MOCK_GH_LOG".into(), Some(log.clone().into_os_string())),
        ];
        tweak(&mut options);
        Self {
            gh: Gh::new(options),
            log,
            _dir: dir,
        }
    }

    /// The calls made so far: `{"argv", "stdin", "matched"}` each.
    pub fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// The JSON each call so far sent on stdin.
    fn requests(&self) -> Vec<Value> {
        self.calls()
            .iter()
            .map(|call| serde_json::from_str(call["stdin"].as_str().unwrap()).unwrap())
            .collect()
    }
}

fn repo() -> GithubRepo {
    GithubRepo {
        host: HOST.to_string(),
        owner: "acme".to_string(),
        repo: "api".to_string(),
        remote: "origin".to_string(),
    }
}

/// `value`, a JSON object, as GraphQL variables.
fn variables(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

#[test]
fn the_program_is_what_dv_gh_names_else_gh() {
    assert_eq!(program(None), "gh");
    assert_eq!(program(Some("".into())), "gh");
    assert_eq!(program(Some("/opt/mock_gh".into())), "/opt/mock_gh");
}

#[test]
fn a_command_of_ghs_own_names_the_repository() {
    let mock = MockGh::new(json!([
        {"argv": ["pr", "view", "482", "**"], "stdout": {"number": 482, "title": "Fix it"}}
    ]));
    let pr = mock
        .gh
        .repo_json(&repo(), &["pr", "view", "482", "--json", "number,title"])
        .unwrap();
    assert_eq!(pr, json!({"number": 482, "title": "Fix it"}));
    assert_eq!(
        mock.calls(),
        [json!({
            "argv": ["pr", "view", "482", "--json", "number,title", "--repo=github.com/acme/api"],
            "stdin": null,
            "matched": 0
        })]
    );
}

#[test]
fn a_rest_call_names_the_host_and_reads_the_body_after_the_head() {
    let mock = MockGh::new(json!([
        {"argv": ["auth", "status", "--hostname=ghe.example.com"], "exit": 0},
        {
            "argv": ["api", "user", "**"],
            "stdout": "HTTP/2.0 200 OK\nContent-Type: application/json; charset=utf-8\r\n\
                       X-Ratelimit-Remaining: 4999\r\n\r\n{\"login\":\"kylekz\"}\n"
        }
    ]));
    for _ in 0..2 {
        let user = mock.gh.rest("ghe.example.com", "user").unwrap();
        assert_eq!(user, json!({"login": "kylekz"}));
    }
    // An Enterprise host is checked before its first call, and not again.
    let argv: Vec<Value> = mock
        .calls()
        .iter()
        .map(|call| call["argv"].clone())
        .collect();
    let api = json!(["api", "user", "--hostname=ghe.example.com", "--include"]);
    assert_eq!(
        argv,
        [
            json!(["auth", "status", "--hostname=ghe.example.com"]),
            api.clone(),
            api
        ]
    );
}

#[test]
fn no_call_is_made_for_a_host_that_is_not_githubs() {
    // Whatever names the host: a remote, or a file someone edited.
    let mock = MockGh::new(json!([
        {"argv": ["auth", "**"], "stderr": "You are not logged into any accounts on that host\n", "exit": 1},
        {"argv": ["**"], "stdout": {"data": {"viewer": {"login": "kylekz"}}}}
    ]));
    let elsewhere = GithubRepo {
        host: "gitlab.com".to_string(),
        ..repo()
    };
    let none = Map::new();
    let refused: [(&str, Option<GithubError>); 7] = [
        ("gitlab.com", mock.gh.rest("gitlab.com", "user").err()),
        (
            "gitlab.com",
            mock.gh.graphql("gitlab.com", "query { … }", &none).err(),
        ),
        (
            "gitlab.com",
            mock.gh
                .mutate("gitlab.com", "mutation { … }", &none, Mutation::Post)
                .err(),
        ),
        (
            "gitlab.com",
            mock.gh.repo_json(&elsewhere, &["pr", "list"]).err(),
        ),
        ("gitlab.com", mock.gh.repo_list(&elsewhere, "pulls").err()),
        // Not hosts at all: nothing is asked about them.
        ("--help", mock.gh.rest("--help", "user").err()),
        (
            "evil.com:x.ghe.com",
            mock.gh.rest("evil.com:x.ghe.com", "user").err(),
        ),
    ];
    for (host, error) in refused {
        let error = error.unwrap_or_else(|| panic!("a call for {host} was made"));
        assert!(
            matches!(&error, GithubError::NotGithub { host: refused } if refused == host),
            "{error:?}"
        );
        assert_eq!(error.kind(), "not_github");
    }
    let error = mock.gh.rest("gitlab.com", "user").unwrap_err();
    assert_eq!(
        error.to_string(),
        "gitlab.com isn't GitHub, nor a GitHub Enterprise host gh is signed in to; if it is \
         one, `gh auth login --hostname gitlab.com`"
    );
    assert_eq!(
        error.fix().as_deref(),
        Some("gh auth login --hostname gitlab.com")
    );
    // Only the question went to gh, once for each call about a host.
    let calls = mock.calls();
    assert_eq!(calls.len(), 6);
    for call in calls {
        assert_eq!(
            call["argv"],
            json!(["auth", "status", "--hostname=gitlab.com"])
        );
    }
}

#[test]
fn a_graphql_request_goes_whole_on_stdin() {
    let mock = MockGh::new(json!([
        {"argv": ["api", "graphql", "**"], "stdout": {"data": {"viewer": {"login": "kylekz"}}}}
    ]));
    // A comment far longer than Windows' 32K command line, with what `-f` arguments can't
    // carry: quotes, newlines, a nested input.
    let body = "A \"quoted\" line\n".repeat(4096);
    let sent = variables(json!({"input": {"body": body, "path": "src/api.rs", "line": 40}}));
    let query = "mutation($input: AddPullRequestReviewThreadInput!) { … }";
    let data = mock.gh.mutate(HOST, query, &sent, Mutation::Post).unwrap();
    assert_eq!(data, json!({"viewer": {"login": "kylekz"}}));
    assert_eq!(
        mock.calls()[0]["argv"],
        json!([
            "api",
            "graphql",
            "--hostname=github.com",
            "--include",
            "--input",
            "-"
        ])
    );
    assert_eq!(
        mock.requests(),
        [json!({"query": query, "variables": sent})]
    );
}

#[test]
fn every_call_gets_the_policy_environment() {
    let mut options = GhOptions::new("gh".into());
    options.env = vec![
        ("NO_COLOR".into(), None),
        ("GH_PAGER".into(), Some("less".into())),
        ("GH_TOKEN".into(), Some("from-a-test".into())),
    ];
    let gh = Gh::new(options);
    let command = gh.command(&Call {
        args: vec!["pr".to_string(), "list".to_string()],
        body: Some(b"{}".to_vec()),
        host: HOST,
        kind: Kind::Read,
        without: &[],
    });
    assert_eq!(command.get_program(), std::path::Path::new("gh"));
    assert_eq!(command.get_input(), Some(&b"{}"[..]));
    assert_eq!(command.get_current_dir(), None);
    assert_eq!(command.get_stop(), Stop::Kill);

    // What the child ends up with: later changes win.
    let mut env = std::collections::BTreeMap::new();
    for (name, value) in command.get_envs() {
        env.insert(
            name.to_str().unwrap(),
            value.map(|value| value.to_str().unwrap()),
        );
    }
    for (name, value) in POLICY_ENV {
        assert_eq!(env[name], Some(*value), "{name}");
    }
    for name in DROPPED_ENV {
        assert_eq!(env[name], None, "{name}");
    }
    assert_eq!(env["GH_TOKEN"], Some("from-a-test"));
    assert_eq!(
        POLICY_ENV,
        [
            ("GH_PROMPT_DISABLED", "1"),
            ("GH_NO_UPDATE_NOTIFIER", "1"),
            ("GH_PAGER", "cat"),
            ("NO_COLOR", "1"),
        ]
    );
}

#[test]
fn a_missing_gh_is_its_own_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-gh");
    let gh = Gh::new(GhOptions::new(missing.clone().into_os_string()));
    let error = gh.rest(HOST, "user").unwrap_err();
    assert!(
        matches!(&error, GithubError::GhMissing { program } if *program == missing.to_string_lossy()),
        "{error:?}"
    );
    assert_eq!(error.detail(), None);
}

#[test]
fn a_call_past_its_own_timeout_is_killed_and_github_is_unavailable() {
    let mock = MockGh::with(
        json!([{"argv": ["**"], "stdout": {"data": {}}, "delay_ms": 30_000}]),
        |options| {
            options.timeout = Duration::from_millis(300);
            options.post_timeout = Duration::from_millis(500);
            options.list_timeout = Duration::from_millis(700);
        },
    );
    // Each kind of call is given its own time, which the error names.
    let timed_out = |what: &str, millis: u64, call: &dyn Fn() -> Option<GithubError>| {
        let started = Instant::now();
        let error = call().unwrap_or_else(|| panic!("{what} was answered"));
        let took = started.elapsed();
        let wanted = format!(
            "gh didn't answer within {} s",
            Duration::from_millis(millis).as_secs_f32()
        );
        assert!(
            matches!(&error, GithubError::Unavailable { reason, .. } if *reason == wanted),
            "{what}: {error:?}"
        );
        assert_eq!(error.kind(), "unavailable");
        assert!(took >= Duration::from_millis(millis), "{what}: {took:?}");
        assert!(took < Duration::from_secs(20), "{what}: {took:?}");
    };
    let mutate = |mutation| {
        mock.gh
            .mutate(HOST, "mutation { … }", &Map::new(), mutation)
            .err()
    };
    timed_out("a read", 300, &|| mock.gh.rest(HOST, "user").err());
    timed_out("a query", 300, &|| {
        mock.gh.graphql(HOST, "query { … }", &Map::new()).err()
    });
    timed_out("a change", 300, &|| mutate(Mutation::Change));
    timed_out("a post", 500, &|| mutate(Mutation::Post));
    timed_out("a list", 700, &|| {
        mock.gh.repo_list(&repo(), "pulls/482/files").err()
    });
}

#[test]
fn the_real_timeouts_are_the_specs() {
    let options = GhOptions::new("gh".into());
    assert_eq!(options.timeout, Duration::from_secs(30));
    assert_eq!(options.post_timeout, Duration::from_secs(60));
    assert_eq!(options.list_timeout, Duration::from_secs(120));
    assert_eq!(options.mutation_gap, Duration::from_secs(1));
}

#[test]
fn only_a_mutation_waits_for_the_one_before_it() {
    let gap = Duration::from_secs(1);
    let ago = |ms| Some(Duration::from_millis(ms));
    for mutation in [Mutation::Change, Mutation::Post] {
        let kind = Kind::Mutation(mutation);
        assert_eq!(wait(kind, None, gap), Duration::ZERO);
        assert_eq!(wait(kind, ago(0), gap), gap);
        assert_eq!(wait(kind, ago(400), gap), Duration::from_millis(600));
        assert_eq!(wait(kind, ago(1000), gap), Duration::ZERO);
        assert_eq!(wait(kind, ago(5000), gap), Duration::ZERO);
    }
    for kind in [Kind::Read, Kind::List] {
        assert_eq!(wait(kind, ago(0), gap), Duration::ZERO);
    }
}

#[test]
fn mutations_run_a_gap_apart() {
    let gap = Duration::from_millis(400);
    let mock = MockGh::with(
        json!([{"argv": ["**"], "stdout": {"data": {"ok": true}}}]),
        |options| options.mutation_gap = gap,
    );
    let mutate = || {
        mock.gh
            .mutate(HOST, "mutation { … }", &Map::new(), Mutation::Change)
            .unwrap()
    };
    let started = Instant::now();
    mutate();
    mock.gh.graphql(HOST, "query { … }", &Map::new()).unwrap();
    mutate();
    mutate();
    assert!(started.elapsed() >= gap * 2, "{:?}", started.elapsed());
    assert_eq!(mock.calls().len(), 4);
}

#[test]
fn calls_run_one_at_a_time() {
    let mock = MockGh::new(json!([
        {"argv": ["**"], "stdout": {"login": "kylekz"}, "delay_ms": 150}
    ]));
    let started = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| mock.gh.rest(HOST, "user").unwrap());
        }
    });
    assert!(
        started.elapsed() >= Duration::from_millis(600),
        "four calls of 150 ms each overlapped: {:?}",
        started.elapsed()
    );
}

#[test]
fn output_that_is_not_the_json_asked_for_is_unexpected_and_kept() {
    let long = "é".repeat(4 * KEPT_BYTES);
    let mock = MockGh::new(json!([
        {"argv": ["pr", "list", "**"], "stdout": "no pull requests found\n"},
        {"argv": ["pr", "view", "**"], "stdout": long},
        {"argv": ["api", "graphql", "**"], "stdout": {"data": null, "extensions": {"cost": 1}}}
    ]));
    let unexpected = |error: GithubError| {
        assert_eq!(error.kind(), "unexpected_output");
        let detail = error.detail();
        match error {
            GithubError::UnexpectedOutput {
                command, output, ..
            } => {
                assert_eq!(detail.as_deref(), Some(output.as_str()));
                (command, output)
            }
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        unexpected(mock.gh.repo_json(&repo(), &["pr", "list"]).unwrap_err()),
        (
            "pr list".to_string(),
            "no pull requests found\n".to_string()
        )
    );
    let (_, kept) = unexpected(mock.gh.repo_json(&repo(), &["pr", "view"]).unwrap_err());
    assert!(
        (KEPT_BYTES - 1..=KEPT_BYTES).contains(&kept.len()),
        "{}",
        kept.len()
    );
    assert!(kept.starts_with("ééé"));
    assert_eq!(
        unexpected(
            mock.gh
                .graphql(HOST, "query { … }", &Map::new())
                .unwrap_err()
        ),
        (
            "api graphql".to_string(),
            json!({"data": null, "extensions": {"cost": 1}}).to_string()
        )
    );
}
