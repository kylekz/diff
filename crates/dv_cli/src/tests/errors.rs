//! Exit codes and error documents (docs/PLAN.md §10.3): repositories that can't be used, parse
//! errors under `--json`, and the commands of later versions. A WSL repository's are
//! `reexec`'s.

use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

#[test]
fn outside_a_repository_is_exit_1() {
    let fixture = Fixture::new();
    let outside = fixture.outside("plain");
    let error = fixture
        .run_in(&outside, &["status", "--json"])
        .error(Exit::Failure, "not_a_repository");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("not a git repository")
    );
    let missing = fixture.path().join("no-such-dir");
    let missing = missing.to_str().unwrap();
    fixture
        .run(&["review", "list", "--repo", missing, "--json"])
        .error(Exit::Failure, "not_a_repository");

    // A relative --repo resolves against the working directory.
    let json = fixture
        .run_in(
            &fixture.path().join("src"),
            &["review", "list", "--repo", "..", "--json"],
        )
        .ok_json();
    assert_eq!(json["reviews"], json!([]));
}

#[test]
fn parse_errors_are_json_when_json_is_asked_for() {
    let fixture = Fixture::new();
    let ran = fixture.run(&["review", "list", "--bogus", "--json"]);
    let error = ran.error(Exit::Usage, "usage");
    // clap's headline, without its `error: ` and on one line; its wording is clap's.
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("--bogus") && !message.starts_with("error") && !message.contains('\n'),
        "{message}"
    );
    assert!(
        ran.stderr.contains("--bogus"),
        "clap's message on stderr: {}",
        ran.stderr
    );

    // Without --json, stderr alone.
    let ran = fixture.run(&["review", "list", "--bogus"]);
    assert_eq!(ran.exit, Exit::Usage);
    assert_eq!(ran.stdout, "");
    assert!(ran.stderr.contains("--bogus"), "{}", ran.stderr);

    // --json anywhere, even where it doesn't parse.
    let ran = fixture.run(&["--json", "review", "wait", "--timeout", "never"]);
    let error = ran.error(Exit::Usage, "usage");
    assert!(
        error["message"].as_str().unwrap().contains("--timeout"),
        "{error}"
    );
    // Spelt with a value, which clap refuses for a flag: the agent still asked for JSON.
    for json in ["--json=true", "--json="] {
        let ran = fixture.run(&[json, "status"]);
        let error = ran.error(Exit::Usage, "usage");
        assert!(
            error["message"].as_str().unwrap().contains("--json"),
            "{error}"
        );
        let ran = fixture.run(&["status", json]);
        ran.error(Exit::Usage, "usage");
    }
    // After a bare `--`, `--json` is a value, not the flag.
    let ran = fixture.run(&["review", "new", "--", "--json"]);
    assert_eq!(ran.exit, Exit::Usage);
    assert_eq!(ran.stdout, "");
    // A `--` given as a text option's value is refused as text, and isn't the end of the
    // options: the `--json` after it still asks for JSON, and nothing is written.
    let review = fixture.review(Target::Working);
    let thread = fixture.thread(&review, "src/app.ts", 10, 10, "Why a cache?");
    let before = fixture.events(&review.id).len();
    for args in [
        &[
            "comment",
            "reply",
            thread.as_str(),
            "--body",
            "--",
            "--json",
        ][..],
        &[
            "comment",
            "address",
            thread.as_str(),
            "--note",
            "--",
            "--json",
        ],
        &[
            "review", "done", "--author", "codex", "--note", "-", "--json",
        ],
    ] {
        let ran = fixture.run(args);
        let error = ran.error(Exit::Usage, "usage");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("alone isn't text"),
            "{args:?}: {error}"
        );
    }
    assert_eq!(fixture.events(&review.id).len(), before);
    // A negative timeout gets the timeout's own message, not clap's tip to write `-- -1`.
    let ran = fixture.run(&["review", "wait", "--timeout", "-1", "--json"]);
    let error = ran.error(Exit::Usage, "usage");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("the timeout must be a positive number of seconds"),
        "{error}"
    );

    // A missing subcommand says so, wherever --json is: after the group it counts as an
    // argument and clap says what is missing; before it, clap shows the group's help instead,
    // whose first line is no message.
    for (line, group) in [
        (&["review", "--json"][..], "dv review"),
        (&["--json", "review"], "dv review"),
        (&["--json", "comment"], "dv comment"),
        (&["comment", "--json"], "dv comment"),
    ] {
        let ran = fixture.run(line);
        assert_eq!(ran.exit, Exit::Usage, "{line:?}");
        // stderr: clap's error, or the group's help, which lists its commands.
        assert!(
            ran.stderr.starts_with("error: ") || ran.stderr.contains("Commands:"),
            "{line:?}: {}",
            ran.stderr
        );
        let json = ran.json();
        assert_eq!(json["error"]["code"], "usage", "{line:?}");
        assert_eq!(
            json["error"]["message"],
            format!("'{group}' requires a subcommand but one was not provided"),
            "{line:?}"
        );
    }

    // Help and the version print on stdout and succeed.
    let ran = fixture.run(&["--help"]);
    assert_eq!(ran.exit, Exit::Ok);
    assert!(ran.stdout.contains("comment"), "{}", ran.stdout);
    let ran = fixture.run(&["--version"]);
    assert_eq!(ran.exit, Exit::Ok);
    assert_eq!(
        ran.stdout,
        format!("dv {}\n", dv_backend::build::version_text())
    );
}

#[test]
fn later_commands_are_refused_with_what_to_do() {
    let fixture = Fixture::new();
    let refused = |args: &[&str], exit: Exit, code: &str| {
        let mut line = args.to_vec();
        line.push("--json");
        fixture.run(&line).error(exit, code)["message"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // `dv backend` runs only as the app starts it.
    refused(&["backend"], Exit::Usage, "usage");
}

#[test]
fn a_missing_argument_is_named_in_the_json_message() {
    let fixture = Fixture::new();
    let error = fixture
        .run(&["review", "new", "--base", "main", "--json"])
        .error(Exit::Usage, "usage");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.starts_with("the following required arguments") && message.contains("--branch"),
        "{message}"
    );
    let error = fixture
        .run(&[
            "comment",
            "add",
            "--file",
            "src/app.ts",
            "--lines",
            "1",
            "--json",
        ])
        .error(Exit::Usage, "usage");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.starts_with("the following required arguments were not provided: ")
            && message.contains("--body"),
        "{message}"
    );
}
