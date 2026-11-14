//! `dv status` and who a command acts as (docs/PLAN.md §2.5, §10.1, §10.2).

use dv_core::review::{ReviewStatus, ops};
use dv_core::target::Target;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

#[test]
fn status_names_the_repo_and_the_current_review() {
    let fixture = Fixture::new();
    let json = fixture.run(&["status", "--json"]).ok_json();
    let root = fixture.backend().repo().toplevel.clone();
    assert_eq!(
        json["dv"],
        json!({"version": env!("CARGO_PKG_VERSION"), "channel": "development"})
    );
    assert_eq!(
        json["repo"],
        json!({"root": root, "branch": "main", "head": "main"})
    );
    assert_eq!(
        json["author"],
        json!({"kind": "reviewer", "name": "dv fixture", "model": null})
    );
    assert_eq!(json["current"], json!(null));
    assert_eq!(json["reviews"], json!([]));
    assert_eq!(json["unreadable"], json!([]));

    let review = fixture.review(Target::Working);
    fixture.thread(&review, "src/app.ts", 10, 10, "Why 100?");
    let json = fixture.run(&["status", "--json"]).ok_json();
    assert_eq!(json["current"]["id"], review.id.as_str());
    assert_eq!(
        json["current"]["threads"],
        json!({"open": 1, "addressed": 0, "resolved": 0, "annotation": 0})
    );
    assert_eq!(json["reviews"].as_array().unwrap().len(), 1);
    assert_eq!(json["reviews"][0], json["current"]);
}

#[test]
fn status_reads_as_short_lines() {
    let fixture = Fixture::new();
    let root = fixture.backend().repo().toplevel.clone();
    let ran = fixture.run(&["status"]);
    assert_eq!(ran.exit, Exit::Ok);
    assert_eq!(
        ran.lines(),
        [
            format!("repo: {root} (on main)").as_str(),
            "acting as: reviewer dv fixture",
            "current review: none",
            "open reviews: none",
        ]
    );
    assert_eq!(ran.stderr, "");

    let review = fixture.review(Target::Working);
    let ran = fixture.run(&["status"]);
    let line = format!(
        "{}  working tree vs main  open  round 1  0 open, 0 addressed, 0 resolved",
        review.id
    );
    assert_eq!(ran.lines()[2], format!("current review: {line}"));
    assert_eq!(ran.lines()[3], "open reviews (1):");
    assert_eq!(ran.lines()[4], format!("  {line}"));
}

#[test]
fn the_author_is_an_agent_when_named_or_under_claude_code() {
    let mut fixture = Fixture::new();
    let author = |fixture: &Fixture, args: &[&str]| {
        let mut line = vec!["status", "--json"];
        line.extend_from_slice(args);
        fixture.run(&line).ok_json()["author"].clone()
    };
    assert_eq!(
        author(&fixture, &[]),
        json!({"kind": "reviewer", "name": "dv fixture", "model": null})
    );

    fixture.env.insert("CLAUDECODE".into(), "1".into());
    assert_eq!(
        author(&fixture, &[]),
        json!({"kind": "agent", "name": "claude-code", "model": null})
    );

    fixture.env.insert("DV_AUTHOR".into(), "codex".into());
    assert_eq!(
        author(&fixture, &[]),
        json!({"kind": "agent", "name": "codex", "model": null})
    );
    assert_eq!(
        author(&fixture, &["--author", "cursor"]),
        json!({"kind": "agent", "name": "cursor", "model": null})
    );

    // An empty variable counts as unset; an empty flag is a mistake.
    fixture.env.insert("DV_AUTHOR".into(), " ".into());
    fixture.env.insert("CLAUDECODE".into(), String::new());
    assert_eq!(
        author(&fixture, &[]),
        json!({"kind": "reviewer", "name": "dv fixture", "model": null})
    );
    let error = fixture
        .run(&["status", "--json", "--author", ""])
        .error(Exit::Usage, "usage");
    assert_eq!(error["message"], "--author needs an agent name");
}

#[test]
fn an_agent_names_its_model() {
    let mut fixture = Fixture::new();
    let author = |fixture: &Fixture, args: &[&str]| {
        let mut line = vec!["status", "--json"];
        line.extend_from_slice(args);
        fixture.run(&line).ok_json()["author"].clone()
    };
    let claude = |model: &str| json!({"kind": "agent", "name": "claude-code", "model": model});

    // The flag, with whoever the agent is.
    fixture.env.insert("CLAUDECODE".into(), "1".into());
    assert_eq!(
        author(&fixture, &["--model", " Claude Fable 5.2 "]),
        claude("Claude Fable 5.2")
    );
    assert_eq!(
        author(&fixture, &["--author", "codex", "--model", "GPT 6.1 Sol"]),
        json!({"kind": "agent", "name": "codex", "model": "GPT 6.1 Sol"})
    );

    // The variable, which the flag overrides; empty, it counts as unset.
    fixture
        .env
        .insert("DV_MODEL".into(), "Claude Opus 5.5".into());
    assert_eq!(author(&fixture, &[]), claude("Claude Opus 5.5"));
    assert_eq!(
        author(&fixture, &["--model", "Claude Fable 5.2"]),
        claude("Claude Fable 5.2")
    );
    let ran = fixture.run(&["status"]);
    assert_eq!(
        ran.lines()[1],
        "acting as: agent claude-code (Claude Opus 5.5)"
    );
    fixture.env.insert("DV_MODEL".into(), " ".into());
    assert_eq!(
        author(&fixture, &[]),
        json!({"kind": "agent", "name": "claude-code", "model": null})
    );

    // The reviewer has no model: the variable is ignored, the flag is a mistake, as an empty
    // one or one of several lines is.
    fixture.env.insert("CLAUDECODE".into(), String::new());
    fixture
        .env
        .insert("DV_MODEL".into(), "Claude Opus 5.5".into());
    assert_eq!(
        author(&fixture, &[]),
        json!({"kind": "reviewer", "name": "dv fixture", "model": null})
    );
    let error = fixture
        .run(&["status", "--json", "--model", "Claude Fable 5.2"])
        .error(Exit::Usage, "usage");
    assert_eq!(
        error["message"],
        "--model names the model an agent runs on: pass --author <agent name> with it"
    );
    let error = fixture
        .run(&["status", "--json", "--author", "codex", "--model", ""])
        .error(Exit::Usage, "usage");
    assert_eq!(error["message"], "--model needs a model name");
    let error = fixture
        .run(&["status", "--json", "--author", "codex", "--model", "a\nb"])
        .error(Exit::Usage, "usage");
    assert_eq!(
        error["message"],
        "--model: a model name is one line of text"
    );
}

#[test]
fn status_shows_the_handoff_note() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    let store = fixture.store();
    ops::set_status(
        &store,
        &review.id,
        ReviewStatus::Ready,
        &agent(),
        Some("Renamed the cache.\n\nAlso added a test."),
    )
    .unwrap();
    let json = fixture.run(&["status", "--json"]).ok_json();
    assert_eq!(json["current"]["status"], "ready");
    let note = &json["current"]["status_note"];
    assert_eq!(note["status"], "ready");
    assert_eq!(note["note"], "Renamed the cache.\n\nAlso added a test.");
    assert_eq!(
        note["author"],
        json!({"kind": "agent", "name": "claude-code", "model": null})
    );
    assert!(note["at"].as_u64().unwrap() > 0);

    let ran = fixture.run(&["review", "show"]);
    assert!(
        ran.lines()
            .contains(&"note from agent claude-code (ready): Renamed the cache."),
        "{}",
        ran.stdout
    );
}

#[test]
fn several_open_reviews_leave_no_current_one() {
    let fixture = Fixture::new();
    let working = fixture.review(Target::Working);
    later();
    let staged = fixture.review(Target::Staged);
    let json = fixture.run(&["status", "--json"]).ok_json();
    assert_eq!(json["current"], json!(null));
    let ids: Vec<&str> = json["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .map(|review| review["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [staged.id.as_str(), working.id.as_str()],
        "newest first"
    );
    let ran = fixture.run(&["status"]);
    assert_eq!(
        ran.lines()[2],
        "current review: none (name one with --review <id>)"
    );

    // Naming one makes it current.
    let json = fixture
        .run(&["status", "--json", "--review", working.id.as_str()])
        .ok_json();
    assert_eq!(json["current"]["id"], working.id.as_str());
}
