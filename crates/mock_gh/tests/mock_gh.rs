//! `mock_gh` as dv meets it: the built binary, started with `DV_MOCK_GH` and `DV_MOCK_GH_LOG`
//! (docs/PLAN.md §14.2).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

/// A temp dir with a scenario file and a call log path.
struct Fixture {
    _dir: tempfile::TempDir,
    scenario: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new(scenario: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scenario.json");
        std::fs::write(&path, scenario).unwrap();
        Self {
            log: dir.path().join("gh_calls.jsonl"),
            scenario: path,
            _dir: dir,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        mock_gh(args, Some(&self.scenario), Some(&self.log))
    }

    fn run(&self, args: &[&str]) -> Outcome {
        Outcome::of(self.command(args).stdin(Stdio::null()))
    }

    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Outcome {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        Outcome::from_output(child.wait_with_output().unwrap())
    }

    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn mock_gh(args: &[&str], scenario: Option<&Path>, log: Option<&Path>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mock_gh"));
    command
        .args(args)
        .env_remove("DV_MOCK_GH")
        .env_remove("DV_MOCK_GH_LOG");
    if let Some(scenario) = scenario {
        command.env("DV_MOCK_GH", scenario);
    }
    if let Some(log) = log {
        command.env("DV_MOCK_GH_LOG", log);
    }
    command
}

#[derive(Debug)]
struct Outcome {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Outcome {
    fn of(command: &mut Command) -> Self {
        Self::from_output(command.output().unwrap())
    }

    fn from_output(output: std::process::Output) -> Self {
        Self {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    }
}

const PR_SCENARIO: &str = r#"{"calls": [
    {"argv": ["pr", "view", "482", "--json", "*"], "stdout": {"number": 482, "title": "Fix it"}, "times": 1},
    {"argv": ["api", "**"], "stdin_contains": "\"event\":\"COMMENT\"", "stdout": {"id": 7}},
    {"argv": ["api", "**"], "stderr": "gh: HTTP 422: line is not part of the diff\n", "exit": 1},
    {"argv": ["auth", "status"], "stdout": "Logged in to github.com as kylekz\n"}
]}"#;

#[test]
fn a_matched_call_answers_with_its_entry() {
    let fixture = Fixture::new(PR_SCENARIO);

    let view = fixture.run(&["pr", "view", "482", "--json", "number,title"]);
    assert_eq!(view.code, Some(0), "{view:?}");
    assert_eq!(view.stdout, "{\"number\":482,\"title\":\"Fix it\"}\n");
    assert_eq!(view.stderr, "");

    let status = fixture.run(&["auth", "status"]);
    assert_eq!(status.code, Some(0), "{status:?}");
    assert_eq!(status.stdout, "Logged in to github.com as kylekz\n");
}

#[test]
fn stdin_selects_between_entries() {
    let fixture = Fixture::new(PR_SCENARIO);
    let args = ["api", "repos/o/r/pulls/482/reviews", "--input", "-"];

    let comment = fixture.run_with_stdin(&args, "{\"event\":\"COMMENT\",\"body\":\"ok\"}");
    assert_eq!(comment.code, Some(0), "{comment:?}");
    assert_eq!(comment.stdout, "{\"id\":7}\n");

    let approve = fixture.run_with_stdin(&args, "{\"event\":\"APPROVE\"}");
    assert_eq!(approve.code, Some(1), "{approve:?}");
    assert_eq!(approve.stdout, "");
    assert_eq!(
        approve.stderr,
        "gh: HTTP 422: line is not part of the diff\n"
    );

    let calls = fixture.calls();
    assert_eq!(
        calls,
        [
            json!({"argv": args, "stdin": "{\"event\":\"COMMENT\",\"body\":\"ok\"}", "matched": 1}),
            json!({"argv": args, "stdin": "{\"event\":\"APPROVE\"}", "matched": 2}),
        ]
    );
}

#[test]
fn times_counts_across_separate_calls() {
    let fixture = Fixture::new(PR_SCENARIO);
    let args = ["pr", "view", "482", "--json", "number"];
    assert_eq!(fixture.run(&args).code, Some(0));
    let second = fixture.run(&args);
    assert_eq!(second.code, Some(64), "entry 0 answers once: {second:?}");

    let calls = fixture.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["matched"], json!(0));
    assert_eq!(calls[1]["matched"], Value::Null);
    assert_eq!(
        calls[1]["stdin"],
        Value::Null,
        "this call doesn't read stdin"
    );
}

#[test]
fn an_unmatched_call_exits_64_and_is_logged() {
    let fixture = Fixture::new(PR_SCENARIO);
    let outcome = fixture.run(&["repo", "set-default", "--view"]);
    assert_eq!(outcome.code, Some(64), "{outcome:?}");
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.starts_with(
            "mock_gh: no scenario entry for argv [\"repo\", \"set-default\", \"--view\"] in the scenario "
        ),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("scenario.json"),
        "{}",
        outcome.stderr
    );
    assert_eq!(
        fixture.calls(),
        [json!({"argv": ["repo", "set-default", "--view"], "stdin": null, "matched": null})]
    );
}

#[test]
fn without_a_scenario_every_call_is_unmatched() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.jsonl");
    let outcome = Outcome::of(mock_gh(&["pr", "list"], None, Some(&log)).stdin(Stdio::null()));
    assert_eq!(outcome.code, Some(64), "{outcome:?}");
    assert!(
        outcome
            .stderr
            .contains("no scenario (DV_MOCK_GH is not set)"),
        "{}",
        outcome.stderr
    );
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("\"matched\":null")
    );

    // With neither variable there is nothing to log, and the answer is the same.
    let bare = Outcome::of(mock_gh(&["pr", "list"], None, None).stdin(Stdio::null()));
    assert_eq!(bare.code, Some(64), "{bare:?}");
}

#[test]
fn a_call_that_reads_no_stdin_leaves_an_open_pipe_alone() {
    let fixture = Fixture::new(PR_SCENARIO);
    let mut child = fixture
        .command(&["auth", "status"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Held open, as a parent that lets gh inherit its own stdin would.
    let _stdin = child.stdin.take();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("mock_gh waited on a stdin its arguments never asked for");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
}

#[test]
fn concurrent_calls_never_overrun_times() {
    let fixture = Fixture::new(
        r#"{"calls": [
            {"argv": ["api", "**"], "stdout": "limited", "times": 3},
            {"argv": ["api", "**"], "stdout": "fallback"}
        ]}"#,
    );
    let outcomes: Vec<Outcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..12)
            .map(|n| {
                let fixture = &fixture;
                scope.spawn(move || fixture.run(&["api", &format!("repos/o/r/issues/{n}")]))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let limited = outcomes
        .iter()
        .filter(|outcome| outcome.stdout == "limited")
        .count();
    assert_eq!(limited, 3, "{outcomes:?}");
    assert!(outcomes.iter().all(|outcome| outcome.code == Some(0)));

    let calls = fixture.calls();
    assert_eq!(calls.len(), 12);
    let logged_limited = calls
        .iter()
        .filter(|call| call["matched"] == json!(0))
        .count();
    assert_eq!(logged_limited, 3);
}

#[test]
fn a_delayed_answer_comes_after_its_delay() {
    let fixture = Fixture::new(
        r#"{"calls": [{"argv": ["pr", "list"], "stdout": "late\n", "delay_ms": 400}]}"#,
    );
    let started = Instant::now();
    let outcome = fixture.run(&["pr", "list"]);
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert_eq!(outcome.code, Some(0), "{outcome:?}");
    assert_eq!(outcome.stdout, "late\n");
    assert_eq!(fixture.calls().len(), 1);
}

#[test]
fn a_bad_scenario_exits_65_and_the_call_is_still_logged() {
    let fixture = Fixture::new(r#"{"calls": [{"argv": ["**", "pr"]}]}"#);
    let outcome = fixture.run(&["pr", "list"]);
    assert_eq!(outcome.code, Some(65), "{outcome:?}");
    assert!(
        outcome.stderr.contains("scenario.json"),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("must be last"),
        "{}",
        outcome.stderr
    );
    assert_eq!(fixture.calls().len(), 1);

    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.json");
    let outcome = Outcome::of(mock_gh(&["pr", "list"], Some(&missing), None).stdin(Stdio::null()));
    assert_eq!(outcome.code, Some(65), "{outcome:?}");
    assert!(
        outcome.stderr.contains("missing.json"),
        "{}",
        outcome.stderr
    );
}

#[test]
fn an_unwritable_log_exits_74() {
    let fixture = Fixture::new(PR_SCENARIO);
    // A directory where the log file should be.
    std::fs::create_dir(&fixture.log).unwrap();
    let outcome = fixture.run(&["auth", "status"]);
    assert_eq!(outcome.code, Some(74), "{outcome:?}");
    assert_eq!(outcome.stdout, "", "nothing is answered without a log line");
    assert!(
        outcome.stderr.contains("gh_calls.jsonl"),
        "{}",
        outcome.stderr
    );
}
