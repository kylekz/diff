//! `dv comment add`, `reply`, `address`, `resolve` and `reopen`: each write lands as its event
//! with the right author, the role rules hold (PLAN §2.5), an agent's first write moves a
//! `changes_requested` review to `in_progress` (PLAN §2.4), and writes on a moved thread record
//! where it is now (PLAN §8).

use dv_core::review::{AnchorSide, AuthorKind, ReviewStatus, ThreadState};
use dv_core::target::Target;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::*;

fn last(fixture: &Fixture, id: &ReviewId) -> EventBody {
    fixture.events(id).pop().unwrap()
}

#[test]
fn what_an_agent_writes_carries_the_model_it_names() {
    let mut fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    let thread = fixture.thread(&review, "src/app.ts", 10, 10, "Why a cache?");
    fixture.finish(&review.id, Verdict::RequestChanges);
    fixture.env.insert("CLAUDECODE".into(), "1".into());
    fixture
        .env
        .insert("DV_MODEL".into(), "Claude Fable 5.2".into());

    let json = fixture
        .run(&[
            "comment",
            "reply",
            thread.as_str(),
            "--body",
            "It halves the queries.",
            "--json",
        ])
        .ok_json();
    let fable = json!({"kind": "agent", "name": "claude-code", "model": "Claude Fable 5.2"});
    assert_eq!(json["reply"]["author"], fable);
    let loaded = fixture.load(&review.id);
    assert_eq!(
        loaded.thread(&thread).unwrap().replies[0].author,
        Author::agent("claude-code").with_model("Claude Fable 5.2")
    );
    // The automatic `in_progress` is the same agent's, model and all.
    assert_eq!(
        loaded.status_change.unwrap().author,
        Some(Author::agent("claude-code").with_model("Claude Fable 5.2"))
    );

    // On another model it is still the reply's author: it edits it, and the thread's state
    // change says which model made it.
    let json = fixture
        .run(&[
            "comment",
            "address",
            thread.as_str(),
            "--note",
            "Kept, with a cap.",
            "--model",
            "Claude Opus 5.5",
            "--json",
        ])
        .ok_json();
    assert_eq!(json["thread"]["author"]["kind"], "reviewer");
    assert_eq!(
        fixture
            .load(&review.id)
            .thread(&thread)
            .unwrap()
            .last_change
            .as_ref()
            .unwrap()
            .author,
        Author::agent("claude-code").with_model("Claude Opus 5.5")
    );
    assert_eq!(
        fixture
            .run(&["comment", "show", thread.as_str(), "--json"])
            .ok_json()["thread"]["replies"][0]["author"],
        fable
    );
}

#[test]
fn an_agent_answers_a_thread_and_each_write_is_its_event() {
    let mut fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    let thread = fixture.thread(&review, "src/app.ts", 10, 10, "Why a cache?");
    fixture.finish(&review.id, Verdict::RequestChanges);
    let before = fixture.events(&review.id).len();

    // A reply by the agent named on the command line.
    let json = fixture
        .run(&[
            "comment",
            "reply",
            thread.as_str(),
            "--body",
            "It halves the queries.",
            "--author",
            "codex",
            "--json",
        ])
        .ok_json();
    assert_eq!(json["reply"]["body"], "It halves the queries.");
    assert_eq!(
        json["reply"]["author"],
        json!({"kind": "agent", "name": "codex", "model": null})
    );
    assert_eq!(json["thread"]["id"], thread.as_str());
    assert_eq!(json["thread"]["replies"][0]["id"], json["reply"]["id"]);
    let events = fixture.events(&review.id);
    assert_eq!(events.len(), before + 2, "the reply, then in_progress");
    match &events[before] {
        EventBody::ReplyAdded {
            thread: on,
            body,
            author,
            anchor,
            ..
        } => {
            assert_eq!(on, &thread);
            assert_eq!(body, "It halves the queries.");
            assert_eq!(author, &Author::agent("codex"));
            assert_eq!(anchor, &None, "the thread hasn't moved");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        events[before + 1],
        EventBody::Status {
            status: ReviewStatus::InProgress,
            note: None,
            author: Some(Author::agent("codex")),
            if_status: Some(ReviewStatus::ChangesRequested),
            if_round: None,
        }
    );

    // Claude Code's variable makes `claude-code` the author; the review is in progress already,
    // so no second status event.
    fixture.env.insert("CLAUDECODE".into(), "1".into());
    let ran = fixture.run(&["comment", "reply", thread.as_str(), "--body", "Done."]);
    assert_eq!(ran.exit, Exit::Ok, "{}", ran.stderr);
    let reply = fixture.load(&review.id).thread(&thread).unwrap().replies[1].clone();
    assert_eq!(
        ran.lines(),
        [format!("replied to {thread}  {}  src/app.ts:10", reply.id)]
    );
    assert_eq!(author_of(&last(&fixture, &review.id)), Some(&agent()));
    assert_eq!(fixture.events(&review.id).len(), before + 3);

    // Addressing, with a note.
    let json = fixture
        .run(&[
            "comment",
            "address",
            thread.as_str(),
            "--note",
            "Kept the cache.",
            "--json",
        ])
        .ok_json();
    assert_eq!(json["changed"], true);
    assert_eq!(json["thread"]["state"], "addressed");
    assert_eq!(json["thread"]["note"], "Kept the cache.");
    match last(&fixture, &review.id) {
        EventBody::ThreadState {
            state,
            note,
            author,
            anchor,
            ..
        } => {
            assert_eq!(state, ThreadState::Addressed);
            assert_eq!(note.as_deref(), Some("Kept the cache."));
            assert_eq!(author, agent());
            assert_eq!(anchor, None);
        }
        other => panic!("{other:?}"),
    }
    // Addressing again replaces the note (PLAN §2.5).
    let ran = fixture.run(&[
        "comment",
        "address",
        thread.as_str(),
        "--note",
        "Kept the cache, documented why.",
    ]);
    assert_eq!(ran.lines(), [format!("{thread}  addressed  src/app.ts:10")]);
    let count = fixture.events(&review.id).len();
    assert_eq!(
        fixture
            .load(&review.id)
            .thread(&thread)
            .unwrap()
            .last_change
            .as_ref()
            .unwrap()
            .note
            .as_deref(),
        Some("Kept the cache, documented why.")
    );

    // Resolving and reopening are the reviewer's: refused by ops, nothing written.
    for (verb, message) in [
        (
            "resolve",
            "an agent can't resolve a thread: only the reviewer resolves threads",
        ),
        (
            "reopen",
            "an agent can't reopen a thread: only the reviewer reopens threads",
        ),
    ] {
        let error = fixture
            .run(&["comment", verb, thread.as_str(), "--json"])
            .error(Exit::Failure, "not_allowed");
        assert_eq!(error["message"], message);
    }
    assert_eq!(fixture.events(&review.id).len(), count);

    // The reviewer (no agent identity: git's user.name) resolves and reopens.
    fixture.env.remove("CLAUDECODE");
    let json = fixture
        .run(&["comment", "resolve", thread.as_str(), "--json"])
        .ok_json();
    assert_eq!(json["thread"]["state"], "resolved");
    assert_eq!(author_of(&last(&fixture, &review.id)), Some(&reviewer()));
    let count = fixture.events(&review.id).len();
    let ran = fixture.run(&["comment", "resolve", thread.as_str()]);
    assert_eq!(
        ran.lines(),
        [format!("{thread}  resolved (unchanged)  src/app.ts:10")]
    );
    assert_eq!(fixture.events(&review.id).len(), count, "a no-op");
    let ran = fixture.run(&["comment", "reopen", thread.as_str()]);
    assert_eq!(ran.lines(), [format!("{thread}  open  src/app.ts:10")]);
    assert_eq!(
        fixture.load(&review.id).thread(&thread).unwrap().state,
        ThreadState::Open
    );
    // The reviewer's writes never move the status.
    assert_eq!(
        fixture.load(&review.id).status,
        ReviewStatus::InProgress,
        "still the agent's"
    );
}

#[test]
fn an_agents_first_write_starts_the_work_whichever_write_it_is() {
    for first in ["address", "add"] {
        let fixture = Fixture::new();
        let review = fixture.review(Target::Working);
        let thread = fixture.thread(&review, "src/app.ts", 10, 10, "Why a cache?");
        // Before any changes are requested, an agent's write leaves the status alone.
        let ran = fixture.run(&[
            "comment",
            "reply",
            thread.as_str(),
            "--body",
            "Early.",
            "--author",
            "codex",
        ]);
        assert_eq!(ran.exit, Exit::Ok, "{}", ran.stderr);
        assert_eq!(fixture.load(&review.id).status, ReviewStatus::Open);

        fixture.finish(&review.id, Verdict::RequestChanges);
        let args: Vec<&str> = match first {
            "address" => vec!["comment", "address", thread.as_str()],
            _ => vec![
                "comment",
                "add",
                "--file",
                "src/app.ts",
                "--lines",
                "2",
                "--body",
                "Unused?",
            ],
        };
        let mut args = args;
        args.extend(["--author", "codex"]);
        let ran = fixture.run(&args);
        assert_eq!(ran.exit, Exit::Ok, "{first}: {}", ran.stderr);
        let loaded = fixture.load(&review.id);
        assert_eq!(loaded.status, ReviewStatus::InProgress, "{first}");
        assert_eq!(
            loaded.status_change.unwrap().author,
            Some(Author::agent("codex"))
        );
    }
}

#[test]
fn comment_add_anchors_on_the_side_as_it_is_now() {
    let fixture = Fixture::new();
    // A committed file the working tree deletes, and an untracked binary. (`commit` takes the
    // whole working tree, so app.ts goes back to its committed text first.)
    fixture.edit_app(&app_lines());
    fixture
        .repo
        .write("src/gone.ts", "export const gone = 1;\n");
    fixture.repo.commit("gone");
    fixture.edit_app(&edited_lines());
    fixture.repo.remove("src/gone.ts");
    fixture
        .repo
        .write("logo.png", b"\x89PNG\r\n\x1a\n\0\0\0binary");
    fixture
        .repo
        .write("src/new.ts", "export const fresh = 1;\n");
    let review = fixture.review(Target::Working);

    let json = fixture
        .run(&[
            "comment",
            "add",
            "--file",
            "src/app.ts",
            "--lines",
            "10:12",
            "--body",
            "Why a cache?",
            "--json",
        ])
        .ok_json();
    let thread = &json["thread"];
    assert_eq!(thread["review_id"], review.id.as_str());
    assert_eq!(thread["state"], "open");
    assert_eq!(thread["placement"], "unchanged");
    assert_eq!(
        thread["anchor"]["original"],
        json!({"round": 1, "path": "src/app.ts", "side": "new", "start_line": 10, "end_line": 12})
    );
    assert_eq!(thread["code"], edited_lines()[9..12].join("\n"));
    assert_eq!(
        thread["author"],
        json!({"kind": "reviewer", "name": "dv fixture", "model": null})
    );
    match last(&fixture, &review.id) {
        EventBody::ThreadAdded { anchor, author, .. } => {
            assert_eq!(author, reviewer());
            assert_eq!(anchor.context_before, edited_lines()[6..9].to_vec());
            assert_eq!(anchor.context_after, edited_lines()[12..15].to_vec());
        }
        other => panic!("{other:?}"),
    }

    // The old side, from a path spelled any way inside the work tree.
    let absolute = fixture.path().join("src").join("app.ts");
    for file in ["./src\\app.ts", absolute.to_str().unwrap()] {
        let json = fixture
            .run(&[
                "comment",
                "add",
                "--file",
                file,
                "--lines",
                "10",
                "--side",
                "old",
                "--body",
                "Was fine.",
                "--json",
            ])
            .ok_json();
        assert_eq!(json["thread"]["side"], "old");
        assert_eq!(json["thread"]["code"], app_lines()[9]);
    }
    let ran = fixture.run(&[
        "comment",
        "add",
        "--file",
        "src/gone.ts",
        "--lines",
        "1",
        "--side",
        "old",
        "--body",
        "Why gone?",
    ]);
    assert_eq!(ran.exit, Exit::Ok, "{}", ran.stderr);
    assert!(
        ran.stdout.starts_with("added t-") && ran.stdout.ends_with("  src/gone.ts:1 (old)\n"),
        "{}",
        ran.stdout
    );

    // Refused: files the review doesn't change, sides a file doesn't have, lines past its end,
    // files without lines.
    let refused = |args: &[&str], exit: Exit, code: &str| -> String {
        let mut line = vec!["comment", "add", "--body", "x", "--json"];
        line.extend_from_slice(args);
        fixture.run(&line).error(exit, code)["message"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        refused(
            &["--file", "README.md", "--lines", "1"],
            Exit::Failure,
            "not_found"
        ),
        format!(
            "README.md isn't among the changed files of review {}",
            review.id
        )
    );
    assert_eq!(
        refused(
            &["--file", "src/new.ts", "--lines", "1", "--side", "old"],
            Exit::Failure,
            "rejected"
        ),
        "src/new.ts is new: it has no old side"
    );
    assert_eq!(
        refused(
            &["--file", "src/gone.ts", "--lines", "1"],
            Exit::Failure,
            "rejected"
        ),
        "src/gone.ts is deleted: it has no new side (`--side old` comments on it as it was)"
    );
    assert_eq!(
        refused(
            &["--file", "src/app.ts", "--lines", "19:25"],
            Exit::Failure,
            "rejected"
        ),
        "--lines 19-25: line 25 is past the end of the file's side (20 lines)"
    );
    assert_eq!(
        refused(
            &["--file", "logo.png", "--lines", "1"],
            Exit::Failure,
            "rejected"
        ),
        "logo.png has no lines to comment on: it is binary"
    );
    let error = fixture
        .run(&[
            "comment",
            "add",
            "--file",
            "src/app.ts",
            "--lines",
            "1",
            "--body",
            "  ",
            "--json",
        ])
        .error(Exit::Usage, "usage");
    assert_eq!(error["message"], "the comment is empty");
    // Four threads were added, nothing else.
    assert_eq!(fixture.load(&review.id).live_threads().count(), 4);
}

#[test]
fn a_body_comes_from_a_file_or_stdin_as_written() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    let thread = fixture.thread(&review, "src/app.ts", 10, 10, "Why a cache?");
    let body_of = |json: &Value| json["reply"]["body"].as_str().unwrap().to_string();

    // stdin: a byte order mark, CRLF and the trailing newline go; indentation stays.
    let json = fixture
        .run_stdin(
            &[
                "comment",
                "reply",
                thread.as_str(),
                "--body-file",
                "-",
                "--json",
            ],
            "\u{feff}Fixed:\r\n\r\n    let x = 1;\r\n",
        )
        .ok_json();
    assert_eq!(body_of(&json), "Fixed:\n\n    let x = 1;");

    // A file, relative to the working directory.
    std::fs::write(fixture.path().join("src/reply.md"), "From a *file*.\n").unwrap();
    let json = fixture
        .run_in(
            &fixture.path().join("src"),
            &[
                "comment",
                "reply",
                thread.as_str(),
                "--body-file",
                "reply.md",
                "--json",
            ],
        )
        .ok_json();
    assert_eq!(body_of(&json), "From a *file*.");

    // Missing, empty or not UTF-8.
    let error = fixture
        .run(&[
            "comment",
            "reply",
            thread.as_str(),
            "--body-file",
            "nope.md",
            "--json",
        ])
        .error(Exit::Failure, "io");
    assert!(
        error["message"].as_str().unwrap().starts_with("reading "),
        "{error}"
    );
    let error = fixture
        .run_stdin(
            &[
                "comment",
                "reply",
                thread.as_str(),
                "--body-file",
                "-",
                "--json",
            ],
            "\r\n  \n",
        )
        .error(Exit::Usage, "usage");
    assert_eq!(error["message"], "the reply is empty");
    std::fs::write(fixture.path().join("latin1.md"), b"caf\xe9").unwrap();
    fixture
        .run(&[
            "comment",
            "reply",
            thread.as_str(),
            "--body-file",
            "latin1.md",
            "--json",
        ])
        .error(Exit::Failure, "io");
    assert_eq!(
        fixture
            .load(&review.id)
            .thread(&thread)
            .unwrap()
            .replies
            .len(),
        2
    );
}

#[test]
fn writes_on_a_moved_thread_record_where_it_is_now() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    let interface = fixture.thread(&review, "src/app.ts", 4, 7, "Add an email.");
    let format = fixture.thread(&review, "src/app.ts", 18, 18, "Trim earlier.");
    fixture.finish(&review.id, Verdict::RequestChanges);
    fixture.edit_app(&moved_lines());

    // The reply records the anchor re-taken at lines 6–9 in round 2, with fresh context.
    let json = fixture
        .run(&[
            "comment",
            "reply",
            interface.as_str(),
            "--body",
            "Added it.",
            "--author",
            "claude-code",
            "--json",
        ])
        .ok_json();
    let recorded = match &fixture.events(&review.id)[fixture.events(&review.id).len() - 2] {
        EventBody::ReplyAdded { anchor, .. } => anchor.clone().expect("a relocated anchor"),
        other => panic!("{other:?}"),
    };
    assert_eq!(recorded.round, 2);
    assert_eq!((recorded.start_line, recorded.end_line), (6, 9));
    assert_eq!(recorded.side, AnchorSide::New);
    assert_eq!(recorded.text, edited_lines()[3..7].to_vec());
    assert_eq!(recorded.context_before, moved_lines()[2..5].to_vec());
    let loaded = fixture.load(&review.id);
    let thread = loaded.thread(&interface).unwrap();
    assert_eq!(thread.anchor, recorded);
    assert_eq!(thread.original_anchor.as_ref().unwrap().start_line, 4);
    // The output (and every later read) says where it is and where it was written.
    for thread in [
        json["thread"].clone(),
        fixture
            .run(&["comment", "show", interface.as_str(), "--json"])
            .ok_json()["thread"]
            .clone(),
    ] {
        assert_eq!(thread["placement"], "moved");
        assert_eq!(
            thread["anchor"],
            json!({
                "original": {"round": 1, "path": "src/app.ts", "side": "new", "start_line": 4, "end_line": 7},
                "current": {"path": "src/app.ts", "side": "new", "start_line": 6, "end_line": 9},
            })
        );
    }

    // It stands where it was recorded now, so addressing records no anchor.
    let ran = fixture.run(&[
        "comment",
        "address",
        interface.as_str(),
        "--author",
        "claude-code",
    ]);
    assert_eq!(ran.exit, Exit::Ok, "{}", ran.stderr);
    assert_eq!(
        ran.lines(),
        [format!(
            "{interface}  addressed  src/app.ts:6-9 (moved from 4-7)"
        )]
    );
    match fixture.events(&review.id).pop().unwrap() {
        EventBody::ThreadState { anchor, state, .. } => {
            assert_eq!(state, ThreadState::Addressed);
            assert_eq!(anchor, None);
        }
        other => panic!("{other:?}"),
    }

    // An outdated thread records nothing either, and still takes the write.
    let json = fixture
        .run(&[
            "comment",
            "address",
            format.as_str(),
            "--note",
            "Removed formatName.",
            "--author",
            "claude-code",
            "--json",
        ])
        .ok_json();
    assert_eq!(json["thread"]["placement"], "outdated");
    assert_eq!(json["thread"]["anchor"]["current"], json!(null));
    match fixture.events(&review.id).pop().unwrap() {
        EventBody::ThreadState { anchor, .. } => assert_eq!(anchor, None),
        other => panic!("{other:?}"),
    }
    let loaded = fixture.load(&review.id);
    assert_eq!(loaded.thread(&format).unwrap().original_anchor, None);
    assert!(
        loaded
            .live_threads()
            .all(|thread| thread.author.kind == AuthorKind::Reviewer)
    );
}

#[test]
fn a_finished_review_takes_no_more_thread_writes() {
    for verdict in ["approve", "close"] {
        let fixture = Fixture::new();
        let review = fixture.review(Target::Working);
        let thread = fixture.thread(&review, "src/app.ts", 10, 10, "Why a cache?");
        match verdict {
            "approve" => fixture.finish(&review.id, Verdict::Approve),
            _ => {
                ops::set_status(
                    &fixture.store(),
                    &review.id,
                    ReviewStatus::Closed,
                    &reviewer(),
                    None,
                )
                .unwrap();
            }
        }
        let before = fixture.events(&review.id).len();
        let id = review.id.as_str();
        let thread = thread.as_str();
        let writes: [Vec<&str>; 6] = [
            vec![
                "comment",
                "add",
                "--review",
                id,
                "--file",
                "src/app.ts",
                "--lines",
                "2",
                "--body",
                "Late",
            ],
            vec![
                "comment", "reply", thread, "--body", "Late", "--author", "codex",
            ],
            vec!["comment", "reply", thread, "--body", "Late"],
            vec![
                "comment", "address", thread, "--note", "n", "--author", "codex",
            ],
            vec!["comment", "resolve", thread],
            vec!["comment", "reopen", thread],
        ];
        for mut write in writes {
            write.push("--json");
            let error = fixture.run(&write).error(Exit::Failure, "finished");
            assert!(
                error["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("the review is "),
                "{verdict} {write:?}: {error}"
            );
        }
        assert_eq!(fixture.events(&review.id).len(), before, "{verdict}");
        // Reading it still works.
        let json = fixture
            .run(&["comment", "list", "--review", id, "--json"])
            .ok_json();
        assert_eq!(json["threads"][0]["state"], "open");
    }
}

#[test]
fn a_target_that_no_longer_resolves_is_exit_1_target() {
    let fixture = Fixture::new();
    fixture.repo.branch("topic").checkout("topic");
    fixture.repo.commit("topic work");
    let json = fixture
        .run(&["review", "new", "--branch", "--json"])
        .ok_json();
    let id = json["review"]["id"].as_str().unwrap().to_string();
    let added = fixture
        .run(&[
            "comment",
            "add",
            "--file",
            "src/app.ts",
            "--lines",
            "10",
            "--body",
            "Why?",
            "--json",
        ])
        .ok_json();
    let thread = added["thread"]["id"].as_str().unwrap().to_string();
    // The branch goes.
    fixture.repo.checkout("main");
    fixture.repo.git(&["branch", "-q", "-D", "topic"]);

    for args in [
        vec!["comment", "list", "--review", id.as_str()],
        vec!["comment", "show", thread.as_str()],
        vec!["comment", "reply", thread.as_str(), "--body", "x"],
        vec!["comment", "resolve", thread.as_str()],
        vec![
            "comment",
            "add",
            "--review",
            id.as_str(),
            "--file",
            "src/app.ts",
            "--lines",
            "1",
            "--body",
            "x",
        ],
        vec![
            "review",
            "submit",
            id.as_str(),
            "--verdict",
            "request-changes",
        ],
    ] {
        let mut line = args.clone();
        line.push("--json");
        let error = fixture.run(&line).error(Exit::Failure, "target");
        assert_eq!(
            error["message"], "the review's target no longer resolves: unknown revision \"topic\"",
            "{args:?}"
        );
    }
}
