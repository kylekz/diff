//! A GitHub pull request's commands (PLAN §9.10) against the fixture GitHub and `mock_gh`:
//! `review new --pr`, `review refresh`, the `pr` document, exit 5's error document and what an
//! agent is refused.

use std::collections::BTreeMap;
use std::path::PathBuf;

use dv_core::review::{ReviewStatus, Verdict};
use dv_test_support::fixture_gh::{self, GhPr};
use dv_test_support::{FixtureGithub, GITHUB_OWNER, GitTrace};
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

mod threads;

const LIB: &str = "pub fn answer() -> u32 {\n    42\n}\n";

/// The fixture GitHub's `dv-fixture/api` with pull request 7 (`feat` into `main`, one commit
/// adding `src/lib.rs`), its clone, and a `mock_gh` that answers about it.
struct PrFixture {
    github: FixtureGithub,
    pr: GhPr,
    env: BTreeMap<String, String>,
    scenario: PathBuf,
    /// git's trace of the commands the fixture's `dv` runs, once a test asks for one.
    trace: Option<GitTrace>,
}

impl PrFixture {
    fn new() -> Self {
        let github = FixtureGithub::new("api");
        let author = github.author();
        author
            .branch("feat")
            .checkout("feat")
            .write("src/lib.rs", LIB);
        author.commit("Add the answer");
        let head = github.open_pr(7, "feat");
        let mut pr = GhPr::new(7, &head);
        pr.files = vec![fixture_gh::file(
            "src/lib.rs",
            "added",
            Some("@@ -0,0 +1,3 @@\n+pub fn answer() -> u32 {\n+    42\n+}"),
        )];
        let dir = github.checkout().outside_dir("gh");
        let scenario = dir.join("scenario.json");
        let env = BTreeMap::from([
            (
                "DV_LINUX_DIR".to_string(),
                no_linux_build().display().to_string(),
            ),
            ("DV_GH".to_string(), mock_gh().display().to_string()),
            ("DV_MOCK_GH".to_string(), scenario.display().to_string()),
            (
                "DV_MOCK_GH_LOG".to_string(),
                dir.join("calls.jsonl").display().to_string(),
            ),
        ]);
        let fixture = Self {
            github,
            pr,
            env,
            scenario,
            trace: None,
        };
        fixture.answer(Vec::new());
        fixture
    }

    /// Has `gh` answer about the pull request as [`PrFixture::pr`] stands, `first` taking any
    /// call they match before that.
    fn answer(&self, first: Vec<Value>) {
        let mut calls = first;
        calls.push(fixture_gh::viewer("kylekz"));
        calls.extend(self.pr.calls_in(&self.github));
        std::fs::write(&self.scenario, json!({ "calls": calls }).to_string()).unwrap();
    }

    /// A push to the pull request: `src/lib.rs` rewritten as `text`. Returns the new head.
    fn push(&mut self, text: &str) -> String {
        let author = self.github.author();
        author.checkout("feat").write("src/lib.rs", text);
        author.commit("Change the answer");
        let head = self.github.open_pr(7, "feat");
        self.pr.head = head.clone();
        self.answer(Vec::new());
        head
    }

    fn repo(&self) -> &FixtureRepo {
        self.github.checkout()
    }

    /// Runs `dv <args>` in the clone as the reviewer.
    fn run(&self, args: &[&str]) -> Ran {
        self.run_as(&[], args)
    }

    /// Runs `dv <args>` in the clone with `vars` set besides the fixture's (`CLAUDECODE`).
    fn run_as(&self, vars: &[(&str, &str)], args: &[&str]) -> Ran {
        let mut env = self.env.clone();
        env.extend(
            vars.iter()
                .map(|(name, value)| (name.to_string(), value.to_string())),
        );
        let (stdout, stderr) = (Buffer::default(), Buffer::default());
        let mut ctx = Context {
            args: Vec::new(),
            cwd: self.repo().path().to_path_buf(),
            env: Env::Vars(env),
            stdin: Box::new(std::io::empty()),
            stdout: Box::new(stdout.clone()),
            stderr: Box::new(stderr.clone()),
            stdout_is_terminal: false,
            stderr_is_terminal: false,
            stdin_is_terminal: false,
            process_stdio: false,
            git_env: self
                .repo()
                .env_overlay()
                .into_iter()
                .chain(self.trace.as_ref().map(GitTrace::env))
                .collect(),
        };
        let exit = crate::main_with(std::iter::once("dv").chain(args.iter().copied()), &mut ctx);
        Ran {
            exit,
            stdout: stdout.text(),
            stderr: stderr.text(),
        }
    }

    fn store(&self) -> ReviewStore {
        ReviewStore::new(Arc::new(
            LocalBackend::open_with(
                self.repo().path(),
                LocalBackendOptions {
                    env: self.repo().env_overlay(),
                },
            )
            .unwrap(),
        ))
    }

    /// The clone's refs under `refs/dv/`.
    fn dv_refs(&self) -> Vec<String> {
        self.repo()
            .git(&["for-each-ref", "--format=%(refname)", "refs/dv/"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Opens pull request 7 as the reviewer and returns the review's id.
    fn open(&self) -> String {
        let json = self
            .run(&["review", "new", "--pr", "7", "--json"])
            .ok_json();
        json["review"]["id"].as_str().unwrap().to_string()
    }
}

#[test]
fn review_new_pr_opens_the_pull_request_as_a_draft() {
    let fixture = PrFixture::new();
    let main = fixture
        .repo()
        .git(&["rev-parse", "main"])
        .trim()
        .to_string();
    let json = fixture
        .run(&["review", "new", "--pr", "7", "--json"])
        .ok_json();
    assert_eq!(json["created"], true);
    let review = &json["review"];
    let id = review["id"].as_str().unwrap();
    assert_eq!(
        review["target"],
        json!({
            "kind": "pr",
            "owner": GITHUB_OWNER,
            "repo": "api",
            "number": 7,
            "base_oid": main,
            "head_oid": fixture.pr.head,
            "label": "dv-fixture/api#7",
        })
    );
    assert_eq!(review["status"], "draft");
    assert_eq!(review["round"], 1);
    assert_eq!(
        review["pr"],
        json!({
            "owner": GITHUB_OWNER,
            "repo": "api",
            "number": 7,
            "url": "https://github.com/dv-fixture/api/pull/7",
            "state": "open",
            "head_oid": fixture.pr.head,
            "pr_round": 1,
            "threads": {"open": 0, "resolved": 0},
        })
    );
    assert_eq!(review["submissions"], json!([]));
    // The draft is the reviewer's.
    let loaded = fixture.store().load(&id.parse().unwrap()).unwrap().review;
    assert_eq!(loaded.author, reviewer());

    // Every spelling of the pull request resumes it.
    for reference in [
        "#7",
        "dv-fixture/api#7",
        "https://github.com/dv-fixture/api/pull/7/files",
    ] {
        let again = fixture
            .run(&["review", "new", "--pr", reference, "--json"])
            .ok_json();
        assert_eq!(again["created"], false, "{reference}");
        assert_eq!(again["review"]["id"], id);
    }
    let ran = fixture.run(&["review", "new", "--pr", "7"]);
    assert_eq!(
        ran.lines(),
        [format!(
            "resumed {id}  dv-fixture/api#7  draft  round 1  0 open, 0 addressed, 0 resolved"
        )]
    );

    // It lists and shows like any review, with its pull request.
    let status = fixture.run(&["status", "--json"]).ok_json();
    assert_eq!(status["reviews"][0]["pr"]["number"], 7);
    assert_eq!(status["current"], Value::Null);
    let shown = fixture.run(&["review", "show", id]);
    assert_eq!(shown.exit, Exit::Ok, "{}", shown.stderr);
    assert_eq!(
        shown.lines()[5],
        format!(
            "pull request: https://github.com/dv-fixture/api/pull/7  open  head {}  PR round 1",
            &fixture.pr.head[..7]
        )
    );
    // Its files are the pull request's, read from its pins.
    let added = fixture.run(&[
        "comment",
        "add",
        "--review",
        id,
        "--file",
        "src/lib.rs",
        "--lines",
        "2",
        "--body",
        "Why 42?",
        "--json",
    ]);
    let thread = added.ok_json();
    assert_eq!(thread["thread"]["code"], "    42");
    assert_eq!(thread["thread"]["anchor"]["current"]["start_line"], 2);
}

#[test]
fn a_reference_that_names_nothing_here_is_refused() {
    let fixture = PrFixture::new();
    let error = fixture
        .run(&["review", "new", "--pr", "seven", "--json"])
        .error(Exit::Usage, "usage");
    assert_eq!(
        error["message"],
        "--pr: not a pull request (a number, #482, owner/repo#482 or its URL): \"seven\""
    );
    let error = fixture
        .run(&["review", "new", "--pr", "acme/other#7", "--json"])
        .error(Exit::GitHub, "github");
    assert_eq!(
        error["message"],
        "#7 is in acme/other; open it from a clone of that repository"
    );
    assert_eq!(error["detail"], "other_repository");
    assert!(fixture.store().list().unwrap().is_empty());
}

#[test]
fn review_refresh_follows_the_head_and_the_threads_relocate() {
    let mut fixture = PrFixture::new();
    let id = fixture.open();
    let added = fixture
        .run(&[
            "comment",
            "add",
            "--review",
            &id,
            "--file",
            "src/lib.rs",
            "--lines",
            "2",
            "--body",
            "Why 42?",
            "--json",
        ])
        .ok_json();
    let thread = added["thread"]["id"].as_str().unwrap().to_string();

    // Nothing moved: the one open PR review is the one meant.
    let json = fixture.run(&["review", "refresh", "--json"]).ok_json();
    assert_eq!(json["review"]["id"], id.as_str());
    assert_eq!(json["moved"], Value::Null);

    // A push puts two lines above the commented one.
    let old = fixture.pr.head.clone();
    let head = fixture.push("// The answer.\n\npub fn answer() -> u32 {\n    42\n}\n");
    let json = fixture.run(&["review", "refresh", &id, "--json"]).ok_json();
    assert_eq!(json["moved"], json!({"from": old, "to": head}));
    assert_eq!(json["review"]["pr"]["head_oid"], head.as_str());
    // The target keeps the commits the review was made at.
    assert_eq!(json["review"]["target"]["head_oid"], old.as_str());

    let shown = fixture
        .run(&["comment", "show", &thread, "--json"])
        .ok_json();
    assert_eq!(shown["thread"]["placement"], "moved");
    assert_eq!(
        shown["thread"]["anchor"]["current"],
        json!({"path": "src/lib.rs", "side": "new", "start_line": 4, "end_line": 4})
    );

    // In text, with where the head went.
    fixture.push("// The answer.\n\npub fn answer() -> u32 {\n    42\n}\n// end\n");
    let ran = fixture.run(&["review", "refresh", &id]);
    assert_eq!(
        ran.lines(),
        [
            format!(
                "refreshed {id}  dv-fixture/api#7  draft  round 1  1 open, 0 addressed, 0 resolved"
            ),
            format!(
                "the pull request moved: head {} -> {}",
                &head[..7],
                &fixture.pr.head[..7]
            ),
        ]
    );
}

#[test]
fn review_refresh_needs_a_pr_review() {
    let mut fixture = PrFixture::new();
    let error = fixture
        .run(&["review", "refresh", "--json"])
        .error(Exit::Usage, "no_review");
    assert_eq!(
        error["message"],
        "no open PR review in this repository; open one with `dv review new --pr <n>`"
    );
    assert_eq!(error["candidates"], json!([]));

    let local = fixture
        .run(&["review", "new", "--commit", "HEAD", "--json"])
        .ok_json();
    let local = local["review"]["id"].as_str().unwrap();
    let error = fixture
        .run(&["review", "refresh", local, "--json"])
        .error(Exit::Usage, "usage");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .ends_with("only a PR review (`dv review new --pr <n>`) refreshes from GitHub"),
        "{error}"
    );

    // Two open PR reviews: neither is guessed.
    let seventh = fixture.open();
    let head = fixture.github.open_pr(8, "feat");
    let mut eighth = GhPr::new(8, &head);
    eighth.files = fixture.pr.files.clone();
    let mut calls = vec![fixture_gh::viewer("kylekz")];
    calls.extend(eighth.calls("api"));
    std::fs::write(&fixture.scenario, json!({ "calls": calls }).to_string()).unwrap();
    let eighth = fixture
        .run(&["review", "new", "--pr", "8", "--json"])
        .ok_json();
    let error = fixture
        .run(&["review", "refresh", "--json"])
        .error(Exit::Usage, "ambiguous_review");
    let mut candidates: Vec<&str> = error["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|candidate| candidate["id"].as_str().unwrap())
        .collect();
    candidates.sort();
    let mut expected = [seventh.as_str(), eighth["review"]["id"].as_str().unwrap()];
    expected.sort();
    assert_eq!(candidates, expected);
    // `$DV_REVIEW` names one.
    fixture.pr.head = fixture.pr.head.clone();
    fixture.answer(Vec::new());
    let ran = fixture.run_as(&[("DV_REVIEW", &seventh)], &["review", "refresh", "--json"]);
    assert_eq!(ran.ok_json()["review"]["id"], seventh.as_str());
}

#[test]
fn a_github_failure_is_exit_5_with_its_kind() {
    let fixture = PrFixture::new();
    // Not signed in.
    fixture.answer(vec![fixture_gh::signed_out()]);
    let ran = fixture.run(&["review", "new", "--pr", "7", "--json"]);
    let error = ran.error(Exit::GitHub, "github");
    assert_eq!(ran.exit.code(), 5);
    assert_eq!(
        error["message"],
        "gh isn't signed in to github.com: run `gh auth login`"
    );
    let detail = error["detail"].as_str().unwrap();
    let mut lines = detail.lines();
    assert_eq!(lines.next(), Some("not_authenticated"));
    assert!(
        lines.next().unwrap().starts_with("$ gh pr view 7 --json "),
        "{detail}"
    );
    assert_eq!(lines.next(), Some("exit code 4"));
    assert!(detail.contains("gh auth login"), "{detail}");
    assert!(
        ran.stderr.starts_with(
            "dv: gh isn't signed in to github.com: run `gh auth login`\n  not_authenticated\n"
        ),
        "{}",
        ran.stderr
    );

    // No such pull request.
    fixture.answer(vec![json!({
        "argv": ["pr", "view", "9", "**"],
        "stderr": "GraphQL: Could not resolve to a PullRequest with the number of 9. (repository.pullRequest)\n",
        "exit": 1
    })]);
    let error = fixture
        .run(&["review", "new", "--pr", "9", "--json"])
        .error(Exit::GitHub, "github");
    assert!(
        error["detail"].as_str().unwrap().starts_with("not_found\n"),
        "{error}"
    );

    // gh isn't there at all.
    let mut missing = PrFixture::new();
    missing
        .env
        .insert("DV_GH".to_string(), "dv-test-has-no-gh".to_string());
    let error = missing
        .run(&["review", "new", "--pr", "7", "--json"])
        .error(Exit::GitHub, "github");
    assert_eq!(error["detail"], "gh_missing");
    assert!(missing.store().list().unwrap().is_empty());

    // A refresh that fails keeps the review and is exit 5 too.
    fixture.answer(Vec::new());
    let id = fixture.open();
    fixture.answer(vec![json!({
        "argv": ["api", "graphql", "**"],
        "stdin_contains": "reviewThreads(first",
        "stdout": "HTTP/2.0 403 Forbidden\nX-Ratelimit-Remaining: 0\r\nX-Ratelimit-Reset: 1792400000\r\n\r\n{\"message\":\"API rate limit exceeded for user ID 1.\"}",
        "stderr": "gh: API rate limit exceeded for user ID 1. (HTTP 403)\n",
        "exit": 1
    })]);
    let error = fixture
        .run(&["review", "refresh", &id, "--json"])
        .error(Exit::GitHub, "github");
    assert_eq!(error["message"], "GitHub's rate limit is used up");
    assert!(
        error["detail"]
            .as_str()
            .unwrap()
            .starts_with("rate_limited\n$ gh api graphql "),
        "{error}"
    );

    // The head keeps moving under the read.
    fixture.answer(vec![json!({
        "argv": ["pr", "view", "7", "--json", "headRefOid", "**"],
        "stdout": {"headRefOid": "f".repeat(40)}
    })]);
    let error = fixture
        .run(&["review", "refresh", &id, "--json"])
        .error(Exit::GitHub, "github");
    assert_eq!(error["detail"], "moved");
}

#[test]
fn a_mirror_that_cant_be_read_still_opens_the_review_with_a_warning() {
    let fixture = PrFixture::new();
    fixture.answer(vec![json!({
        "argv": ["api", "graphql", "**"],
        "stdin_contains": "viewerViewedState",
        "stderr": "gh: Server Error (HTTP 502)\n",
        "exit": 1
    })]);
    let ran = fixture.run(&["review", "new", "--pr", "7", "--json"]);
    let json = ran.ok_json();
    assert_eq!(json["created"], true);
    let id = json["review"]["id"].as_str().unwrap();
    // Never read whole, there is no mirror to tell GitHub's word on its state, or its URL.
    assert_eq!(json["review"]["pr"]["state"], Value::Null);
    assert_eq!(json["review"]["pr"]["url"], Value::Null);
    assert_eq!(
        ran.stderr,
        format!(
            "dv: warning: what GitHub holds about the pull request couldn't be read (GitHub \
             can't be reached: GitHub answered HTTP 502); `dv review refresh {id}` reads it again\n"
        )
    );
}

#[test]
fn an_agent_reads_a_pr_review_and_writes_nothing_to_it() {
    let fixture = PrFixture::new();
    let id = fixture.open();
    let thread = fixture
        .run(&[
            "comment",
            "add",
            "--review",
            &id,
            "--file",
            "src/lib.rs",
            "--lines",
            "2",
            "--body",
            "Why 42?",
            "--json",
        ])
        .ok_json()["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let reply = fixture
        .run(&["comment", "reply", &thread, "--body", "Really.", "--json"])
        .ok_json()["reply"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let review_id = id.parse().unwrap();
    let events = fixture.store().events(&review_id).unwrap().events.len();

    let refused = "an agent can't write to a PR review: PR reviews are the reviewer's; read the \
                   threads with `dv comment list`";
    let claude = [("CLAUDECODE", "1")];
    let lines: Vec<Vec<&str>> = vec![
        vec![
            "comment",
            "add",
            "--review",
            &id,
            "--file",
            "src/lib.rs",
            "--lines",
            "1",
            "--body",
            "x",
        ],
        // A file the review doesn't change, lines it doesn't have: the answer is the same.
        vec![
            "comment",
            "add",
            "--review",
            &id,
            "--file",
            "nowhere.rs",
            "--lines",
            "99",
            "--body",
            "x",
        ],
        vec![
            "comment",
            "annotate",
            "--review",
            &id,
            "--file",
            "src/lib.rs",
            "--lines",
            "1",
            "--body",
            "x",
        ],
        vec!["comment", "reply", &thread, "--body", "x"],
        vec!["comment", "address", &thread, "--note", "done"],
        vec!["comment", "address", &thread],
        vec!["comment", "resolve", &thread],
        vec!["comment", "reopen", &thread],
        vec!["comment", "edit", &thread, "--body", "x"],
        vec!["comment", "edit", &reply, "--body", "x"],
        vec!["comment", "delete", &thread],
        vec!["comment", "delete", &reply],
        vec!["review", "done", &id],
        vec!["review", "done", &id, "--note", "x", "--round", "1"],
        // Submitting is the reviewer's, whichever version it arrives in.
        vec!["review", "submit", &id, "--verdict", "comment"],
        // Whatever else is wrong with the line, the answer is the same: text that is
        // empty, an id that is no thread's, a thread that isn't there.
        vec![
            "comment",
            "add",
            "--review",
            &id,
            "--file",
            "src/lib.rs",
            "--lines",
            "1",
            "--body",
            "",
        ],
        vec!["comment", "reply", "x", "--review", &id, "--body", "x"],
        vec![
            "comment",
            "reply",
            "t-01k6zzzzzzzzzzzzzzzzzzzzzz",
            "--review",
            &id,
            "--body",
            "x",
        ],
        vec!["comment", "address", "x", "--review", &id],
        vec!["comment", "edit", &thread, "--body", ""],
        vec!["review", "done", &id, "--note", ""],
    ];
    for line in &lines {
        let mut line = line.clone();
        line.push("--json");
        let error = fixture
            .run_as(&claude, &line)
            .error(Exit::Failure, "not_allowed");
        assert_eq!(error["message"], refused, "{line:?}");
        // By its own name too.
        line.extend(["--author", "codex"]);
        let error = fixture.run(&line).error(Exit::Failure, "not_allowed");
        assert_eq!(error["message"], refused, "{line:?}");
    }
    // The reviewer's own verbs stay the reviewer's.
    for line in [
        vec!["review", "close", &id, "--json"],
        vec!["review", "delete", &id, "--json"],
    ] {
        fixture
            .run_as(&claude, &line)
            .error(Exit::Failure, "not_allowed");
    }
    // The reviewer hears that submitting arrives later; the same mistakes are theirs to
    // hear about as on any review.
    fixture
        .run(&["review", "submit", &id, "--verdict", "comment", "--json"])
        .error(Exit::Usage, "unsupported");
    fixture
        .run(&[
            "comment", "reply", "x", "--review", &id, "--body", "x", "--json",
        ])
        .error(Exit::Usage, "usage");
    assert_eq!(
        fixture.store().events(&review_id).unwrap().events.len(),
        events,
        "nothing was written"
    );

    // It reads all of it, and may open and refresh: neither writes what a reviewer wrote.
    let listed = fixture
        .run_as(&claude, &["comment", "list", "--review", &id, "--json"])
        .ok_json();
    assert_eq!(listed["threads"][0]["id"], thread.as_str());
    assert_eq!(listed["threads"][0]["replies"][0]["body"], "Really.");
    let shown = fixture
        .run_as(&claude, &["review", "show", &id, "--json"])
        .ok_json();
    assert_eq!(shown["review"]["pr"]["number"], 7);
    let resumed = fixture
        .run_as(&claude, &["review", "new", "--pr", "7", "--json"])
        .ok_json();
    assert_eq!(resumed["created"], false);
    fixture
        .run_as(&claude, &["review", "refresh", &id, "--json"])
        .ok_json();
    assert_eq!(
        fixture.store().events(&review_id).unwrap().events.len(),
        events
    );

    // A draft an agent's command makes is the reviewer's all the same.
    let other = PrFixture::new();
    let made = other
        .run_as(&claude, &["review", "new", "--pr", "7", "--json"])
        .ok_json();
    assert_eq!(made["created"], true);
    let made = other
        .store()
        .load(&made["review"]["id"].as_str().unwrap().parse().unwrap())
        .unwrap()
        .review;
    assert_eq!(made.author, reviewer());
}

#[test]
fn the_pull_requests_rounds_are_its_reviews() {
    let mut fixture = PrFixture::new();
    let first = fixture.open();
    ops::record_submission(
        &fixture.store(),
        &first.parse().unwrap(),
        9001,
        "https://github.com/dv-fixture/api/pull/7#pullrequestreview-9001",
        Verdict::RequestChanges,
    )
    .unwrap();
    // Submitted at the head: opening again shows it, and it takes nothing more.
    let again = fixture
        .run(&["review", "new", "--pr", "7", "--json"])
        .ok_json();
    assert_eq!(again["created"], false);
    assert_eq!(again["review"]["id"], first.as_str());
    assert_eq!(again["review"]["status"], "submitted");
    let submitted = fixture
        .store()
        .load(&first.parse().unwrap())
        .unwrap()
        .review;
    assert_eq!(
        again["review"]["submissions"],
        json!([{
            "github_review_id": 9001,
            "url": "https://github.com/dv-fixture/api/pull/7#pullrequestreview-9001",
            "event": "request_changes",
            "at": submitted.submissions[0].at,
        }])
    );
    assert_eq!(submitted.status, ReviewStatus::Submitted);
    let shown = fixture.run(&["review", "show", &first]);
    assert!(
        shown.lines().contains(
            &"submitted to GitHub: request_changes  https://github.com/dv-fixture/api/pull/7#pullrequestreview-9001"
        ),
        "{}",
        shown.stdout
    );

    // New commits: the next review is the pull request's second round.
    let old = fixture.pr.head.clone();
    let head = fixture.push("pub fn answer() -> u32 {\n    43\n}\n");
    let next = fixture
        .run(&["review", "new", "--pr", "7", "--json"])
        .ok_json();
    assert_eq!(next["created"], true);
    assert_eq!(next["review"]["pr"]["pr_round"], 2);
    assert_eq!(next["review"]["pr"]["head_oid"], head.as_str());
    assert_eq!(next["review"]["round"], 1, "one dv round per PR review");
    // The submitted one stays at the head it was submitted at, refreshed or not.
    let refreshed = fixture
        .run(&["review", "refresh", &first, "--json"])
        .ok_json();
    assert_eq!(refreshed["moved"], Value::Null);
    assert_eq!(refreshed["review"]["pr"]["head_oid"], old.as_str());
    assert_eq!(refreshed["review"]["pr"]["pr_round"], 1);

    // Deleting them takes what dv kept for the pull request with the last one.
    let second = next["review"]["id"].as_str().unwrap();
    fixture
        .run(&["review", "delete", &first, "--json"])
        .ok_json();
    assert_eq!(fixture.dv_refs().len(), 4);
    fixture
        .run(&["review", "delete", second, "--json"])
        .ok_json();
    assert_eq!(fixture.dv_refs(), Vec::<String>::new());
}

#[test]
fn commands_that_only_read_never_start_gh() {
    let fixture = PrFixture::new();
    let id = fixture.open();
    let thread = fixture
        .run(&[
            "comment",
            "add",
            "--review",
            &id,
            "--file",
            "src/lib.rs",
            "--lines",
            "2",
            "--body",
            "Why 42?",
            "--json",
        ])
        .ok_json()["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    // A remote on a host only `gh` could say is GitHub's: telling which repository the clone
    // is would ask it.
    fixture.repo().git(&[
        "remote",
        "add",
        "work",
        "https://ghe.example.com/dv-fixture/api.git",
    ]);
    let log = fixture.scenario.with_file_name("calls.jsonl");
    std::fs::remove_file(&log).unwrap();

    // The pull request as its stored mirror has it.
    let pr = json!({
        "owner": GITHUB_OWNER,
        "repo": "api",
        "number": 7,
        "url": "https://github.com/dv-fixture/api/pull/7",
        "state": "open",
        "head_oid": fixture.pr.head,
        "pr_round": 1,
        "threads": {"open": 0, "resolved": 0},
    });
    for vars in [&[][..], &[("CLAUDECODE", "1")]] {
        let status = fixture.run_as(vars, &["status", "--json"]).ok_json();
        assert_eq!(status["reviews"][0]["pr"], pr);
        let list = fixture
            .run_as(vars, &["review", "list", "--json"])
            .ok_json();
        assert_eq!(list["reviews"][0]["pr"], pr);
        let shown = fixture
            .run_as(vars, &["review", "show", &id, "--json"])
            .ok_json();
        assert_eq!(shown["review"]["pr"], pr);
        for line in [
            vec!["review", "show", &id],
            vec!["comment", "list", "--review", &id, "--json"],
            vec!["comment", "show", &thread, "--json"],
        ] {
            let ran = fixture.run_as(vars, &line);
            assert_eq!(ran.exit, Exit::Ok, "{line:?}: {}", ran.stderr);
        }
        // A draft waits to be submitted (PLAN §10.1), which no command here does.
        let waited = fixture.run_as(vars, &["review", "wait", &id, "--timeout", "0.2", "--json"]);
        assert_eq!(waited.exit, Exit::Timeout, "{}", waited.stderr);
    }
    assert!(
        !log.exists(),
        "{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

#[test]
fn a_review_that_is_here_opens_when_github_cant_be_asked() {
    let fixture = PrFixture::new();
    let id = fixture.open();
    let mut offline = PrFixture {
        github: fixture.github,
        pr: fixture.pr,
        env: fixture.env,
        scenario: fixture.scenario,
        trace: None,
    };
    offline
        .env
        .insert("DV_GH".to_string(), "dv-test-has-no-gh".to_string());
    let ran = offline.run(&["review", "new", "--pr", "7", "--json"]);
    let json = ran.ok_json();
    assert_eq!(json["created"], false);
    assert_eq!(json["review"]["id"], id.as_str());
    // What the last read left: the mirror, and the head the review follows.
    assert_eq!(json["review"]["pr"]["state"], "open");
    assert_eq!(json["review"]["pr"]["head_oid"], offline.pr.head.as_str());
    assert!(
        ran.stderr.starts_with(
            "dv: warning: what GitHub holds about the pull request couldn't be read ("
        ) && ran
            .stderr
            .ends_with(&format!("); `dv review refresh {id}` reads it again\n")),
        "{}",
        ran.stderr
    );
    // The same for its text, and for a submitted review of the pull request.
    let text = offline.run(&["review", "new", "--pr", "#7"]);
    assert_eq!(text.exit, Exit::Ok, "{}", text.stderr);
    assert!(text.stdout.starts_with("resumed "), "{}", text.stdout);
    ops::record_submission(
        &offline.store(),
        &id.parse().unwrap(),
        9001,
        "https://github.com/dv-fixture/api/pull/7#pullrequestreview-9001",
        Verdict::Comment,
    )
    .unwrap();
    let json = offline
        .run(&["review", "new", "--pr", "7", "--json"])
        .ok_json();
    assert_eq!(json["review"]["id"], id.as_str());
    assert_eq!(json["review"]["status"], "submitted");
    // A refresh says it failed: exit 5, the review as it was.
    let error = offline
        .run(&["review", "refresh", &id, "--json"])
        .error(Exit::GitHub, "github");
    assert_eq!(error["detail"], "gh_missing");
}

#[test]
fn review_new_pr_takes_no_review_beside_it() {
    let fixture = PrFixture::new();
    let id = fixture.open();
    let error = fixture
        .run(&["review", "new", "--pr", "7", "--review", &id, "--json"])
        .error(Exit::Usage, "usage");
    assert_eq!(
        error["message"],
        "`dv review new` takes a pull request or --review, not both: a pull request opens its \
         own review"
    );
}

#[test]
fn a_draft_pinned_again_reports_the_head_it_showed() {
    let mut fixture = PrFixture::new();
    let id = fixture.open();
    let old = fixture.pr.head.clone();
    for pin in ["pr-head", "pr-base"] {
        fixture
            .repo()
            .git(&["update-ref", "-d", &format!("refs/dv/reviews/{id}/{pin}")]);
    }
    let head = fixture.push("pub fn answer() -> u32 {\n    43\n}\n");
    let json = fixture.run(&["review", "refresh", &id, "--json"]).ok_json();
    assert_eq!(json["moved"], json!({"from": old, "to": head}));
    assert_eq!(json["review"]["pr"]["head_oid"], head.as_str());
}

#[test]
fn what_github_couldnt_tell_is_in_the_document_and_says_how_to_fix_it() {
    let fixture = PrFixture::new();
    // Read whole: nothing stale.
    let json = fixture
        .run(&["review", "new", "--pr", "7", "--json"])
        .ok_json();
    assert_eq!(json["stale"], Value::Null);
    let id = json["review"]["id"].as_str().unwrap().to_string();

    // Signed out, the review that is here opens: the warning and the document carry the
    // failure as exit 5 words it, with the command that fixes it.
    fixture.answer(vec![fixture_gh::signed_out()]);
    let ran = fixture.run(&["review", "new", "--pr", "7", "--json"]);
    let json = ran.ok_json();
    assert_eq!(json["created"], false);
    assert_eq!(
        json["stale"],
        json!({
            "kind": "not_authenticated",
            "message": "gh isn't signed in to github.com: run `gh auth login`",
        })
    );
    assert_eq!(
        ran.stderr,
        format!(
            "dv: warning: what GitHub holds about the pull request couldn't be read (gh isn't \
             signed in to github.com: run `gh auth login`); `dv review refresh {id}` reads it \
             again\n"
        )
    );
    let error = fixture
        .run(&["review", "refresh", &id, "--json"])
        .error(Exit::GitHub, "github");
    assert_eq!(
        error["message"],
        "gh isn't signed in to github.com: run `gh auth login`"
    );

    // A fetch that fails says what git said: here, a pull request GitHub tells about whose
    // commits the remote doesn't have.
    let mut calls = vec![fixture_gh::viewer("kylekz")];
    let mut absent = fixture.pr.clone();
    absent.number = 9;
    calls.extend(absent.calls("api"));
    std::fs::write(&fixture.scenario, json!({ "calls": calls }).to_string()).unwrap();
    let error = fixture
        .run(&["review", "new", "--pr", "9", "--json"])
        .error(Exit::GitHub, "github");
    assert_eq!(
        error["message"],
        "git fetch failed (exit code 128): couldn't find remote ref refs/pull/9/head"
    );
    assert!(
        error["detail"].as_str().unwrap().starts_with("git\n"),
        "{error}"
    );
    // The open that failed left nothing of pull request 9 behind.
    assert!(
        fixture
            .dv_refs()
            .iter()
            .all(|name| !name.starts_with("refs/dv/pr/dv-fixture/api/9/")),
        "{:?}",
        fixture.dv_refs()
    );
    // The local review's document has the key too.
    let local = fixture.run(&["review", "new", "--json"]).ok_json();
    assert_eq!(local["stale"], Value::Null);
}
