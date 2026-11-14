//! `dv review new`, `done`, `submit`, `close` and `delete`: the events they write and who may
//! run them (PLAN §2.3, §2.4, §10.1).

use dv_core::review::{AuthorKind, ReviewStatus, StatusChange};
use dv_core::target::Target;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

#[test]
fn review_new_starts_or_resumes_each_target() {
    let mut fixture = Fixture::new();
    let json = fixture.run(&["review", "new", "--json"]).ok_json();
    assert_eq!(json["created"], true);
    let id = json["review"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        json["review"]["target"],
        json!({"kind": "working", "label": "working tree vs main"})
    );
    assert_eq!(json["review"]["status"], "open");
    let review_id = ReviewId::parse(&id).unwrap();
    assert_eq!(
        fixture.events(&review_id)[0],
        EventBody::Created {
            target: Target::Working,
            author: reviewer(),
            worktree: fixture.backend().repo().toplevel.clone(),
        }
    );

    // Again: the open review of the target, resumed.
    let ran = fixture.run(&["review", "new"]);
    assert_eq!(
        ran.lines(),
        [format!(
            "resumed {id}  working tree vs main  open  round 1  0 open, 0 addressed, 0 resolved"
        )]
    );

    // The other targets, each its own review; an agent may start one.
    fixture.env.insert("CLAUDECODE".into(), "1".into());
    let started = |fixture: &Fixture, args: &[&str]| {
        let mut line = vec!["review", "new", "--json"];
        line.extend_from_slice(args);
        let json = fixture.run(&line).ok_json();
        assert_eq!(json["created"], true, "{args:?}");
        json["review"]["target"].clone()
    };
    assert_eq!(
        started(&fixture, &["--staged"]),
        json!({"kind": "staged", "label": "staged vs main"})
    );
    assert_eq!(
        started(&fixture, &["--commit", "HEAD"]),
        json!({"kind": "commit", "rev": "HEAD", "label": "commit HEAD"})
    );
    assert_eq!(
        started(&fixture, &["--range", "HEAD..main"])["kind"],
        "range"
    );
    let staged = fixture
        .store()
        .list()
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry {
            dv_core::review::ListEntry::Ok(summary) => Some(summary),
            _ => None,
        })
        .find(|summary| summary.target == Target::Staged)
        .unwrap();
    assert_eq!(staged.author, agent());

    // A branch: the one checked out (here, in a linked worktree), against --base.
    let linked = fixture.repo.add_worktree("feature", "feat");
    let json = fixture
        .run_in(
            &linked,
            &["review", "new", "--branch", "--base", "main", "--json"],
        )
        .ok_json();
    assert_eq!(
        json["review"]["target"],
        json!({"kind": "branch", "branch": "feat", "base": "main", "label": "feat vs main"})
    );

    // A target that doesn't resolve is refused before any review is made.
    let count = fixture.store().list().unwrap().len();
    fixture
        .run(&["review", "new", "--commit", "no-such-rev", "--json"])
        .error(Exit::Failure, "git");
    fixture
        .run(&[
            "review", "new", "--branch", "nope", "--base", "main", "--json",
        ])
        .error(Exit::Failure, "git");
    // A detached HEAD has no branch to name.
    fixture.repo.git(&["checkout", "-q", "--detach"]);
    let error = fixture
        .run(&["review", "new", "--branch", "--json"])
        .error(Exit::Usage, "usage");
    assert_eq!(
        error["message"],
        "HEAD is detached: name the branch, --branch <name>"
    );
    assert_eq!(fixture.store().list().unwrap().len(), count);
}

#[test]
fn done_hands_the_round_back_with_a_note() {
    let mut fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    fixture.finish(&review.id, Verdict::RequestChanges);

    // Only an agent hands a round back.
    let error = fixture
        .run(&["review", "done", "--json"])
        .error(Exit::Usage, "usage");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("`dv review done` is an agent's handoff: pass --author <agent name>"),
        "{error}"
    );

    let json = fixture
        .run(&[
            "review",
            "done",
            "--author",
            "claude-code",
            "--note",
            "All **fixed**.\nSee the replies.",
            "--json",
        ])
        .ok_json();
    assert_eq!(json["changed"], true);
    assert_eq!(json["review"]["status"], "ready");
    let at = json["review"]["status_note"]["at"].clone();
    assert_eq!(
        json["review"]["status_note"],
        json!({
            "status": "ready",
            "note": "All **fixed**.\nSee the replies.",
            "author": {"kind": "agent", "name": "claude-code", "model": null},
            "at": at,
        })
    );
    assert_eq!(
        fixture.events(&review.id).pop().unwrap(),
        EventBody::Status {
            status: ReviewStatus::Ready,
            note: Some("All **fixed**.\nSee the replies.".into()),
            author: Some(agent()),
            if_status: None,
            // Conditional on the rounds it was checked against (PLAN §2.4): round 1.
            if_round: Some(1),
        }
    );

    // The same again writes nothing; $DV_AUTHOR names the agent too.
    let count = fixture.events(&review.id).len();
    fixture.env.insert("DV_AUTHOR".into(), "claude-code".into());
    let ran = fixture.run(&[
        "review",
        "done",
        "--note",
        "All **fixed**.\nSee the replies.",
    ]);
    assert_eq!(ran.exit, Exit::Ok, "{}", ran.stderr);
    assert_eq!(
        ran.lines(),
        [format!(
            "unchanged (already ready): {}  working tree vs main  ready  round 2  0 open, 0 \
             addressed, 0 resolved",
            review.id
        )]
    );
    assert_eq!(fixture.events(&review.id).len(), count);
    let ran = fixture.run(&["review", "done", "--note", "One more fix."]);
    assert!(
        ran.stdout.starts_with("handed back to the reviewer: "),
        "{}",
        ran.stdout
    );
    assert_eq!(
        fixture
            .load(&review.id)
            .status_change
            .unwrap()
            .note
            .as_deref(),
        Some("One more fix.")
    );

    // A note from a file (relative to the working directory, or absolute), a Markdown list and
    // all, cleaned as a body is.
    let note = fixture.outside("notes").join("note.md");
    std::fs::write(
        &note,
        "\u{feff}- Swept the cache.\r\n- Added a test.\r\n\r\n",
    )
    .unwrap();
    let json = fixture
        .run(&[
            "review",
            "done",
            "--note-file",
            note.to_str().unwrap(),
            "--json",
        ])
        .ok_json();
    assert_eq!(
        json["review"]["status_note"]["note"],
        "- Swept the cache.\n- Added a test."
    );
    fixture
        .run(&["review", "done", "--note-file", "missing-note.md", "--json"])
        .error(Exit::Failure, "io");
}

#[test]
fn submit_closes_the_round_with_the_verdict() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    fixture.thread(&review, "src/app.ts", 10, 10, "Why a cache?");

    // A local round approves or requests changes.
    let error = fixture
        .run(&["review", "submit", "--verdict", "comment", "--json"])
        .error(Exit::Usage, "usage");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("a local round approves or requests changes"),
        "{error}"
    );

    let json = fixture
        .run_stdin(
            &[
                "review",
                "submit",
                "--verdict",
                "request-changes",
                "--body-file",
                "-",
                "--json",
            ],
            "Two things.\n",
        )
        .ok_json();
    let closed_at = json["round"]["closed_at"].clone();
    assert_eq!(
        json["round"],
        json!({"n": 1, "verdict": "request_changes", "body": "Two things.", "closed_at": closed_at})
    );
    assert_eq!(json["review"]["status"], "changes_requested");
    assert_eq!(json["review"]["round"], 2);
    // The round's snapshot holds the working tree as it was.
    assert_eq!(
        fixture.repo.git(&[
            "show",
            &format!("refs/dv/reviews/{}/1:src/app.ts", review.id)
        ]),
        text_of(&edited_lines())
    );
    let status = fixture.load(&review.id).status_change.unwrap();
    assert_eq!(
        (status.status, status.author),
        (ReviewStatus::ChangesRequested, Some(reviewer()))
    );

    // Approving finishes the review.
    let ran = fixture.run(&[
        "review",
        "submit",
        review.id.as_str(),
        "--verdict",
        "approve",
    ]);
    assert_eq!(
        ran.lines(),
        [
            "round 2 closed: approve".to_string(),
            format!(
                "{}  working tree vs main  approved  round 2  1 open, 0 addressed, 0 resolved",
                review.id
            ),
        ]
    );
    fixture
        .run(&[
            "review",
            "submit",
            review.id.as_str(),
            "--verdict",
            "approve",
            "--json",
        ])
        .error(Exit::Failure, "finished");
    // The agent's handoff racing the approval, and the reviewer's close after it, hear the same
    // stop signal (PLAN §10.1).
    let error = fixture
        .run(&[
            "review",
            "done",
            review.id.as_str(),
            "--author",
            "codex",
            "--note",
            "- Fixed it.",
            "--json",
        ])
        .error(Exit::Failure, "finished");
    assert_eq!(error["message"], "the review is approved");
    fixture
        .run(&["review", "close", review.id.as_str(), "--json"])
        .error(Exit::Failure, "finished");
    assert_eq!(
        fixture.load(&review.id).status,
        ReviewStatus::Approved,
        "nothing written"
    );
}

#[test]
fn a_pr_review_is_submitted_in_a_later_version() {
    let fixture = Fixture::new();
    let head = dv_core::git::Oid::parse(fixture.repo.git(&["rev-parse", "HEAD"]).trim()).unwrap();
    let pr = fixture
        .store()
        .create(
            Target::Pr {
                owner: "kylekz".into(),
                repo: "difftest".into(),
                number: 7,
                base_oid: head.clone(),
                head_oid: head,
            },
            reviewer(),
        )
        .unwrap();
    for verdict in ["approve", "comment"] {
        let error = fixture
            .run(&[
                "review",
                "submit",
                pr.id.as_str(),
                "--verdict",
                verdict,
                "--json",
            ])
            .error(Exit::Usage, "unsupported");
        assert_eq!(
            error["message"],
            "submitting a PR review to GitHub arrives in a later version"
        );
    }
    assert_eq!(fixture.events(&pr.id).len(), 1);
}

#[test]
fn a_working_review_snapshots_its_own_worktree_from_anywhere() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    let linked = fixture.repo.add_worktree("feature", "feat");
    let ran = fixture.run_in(
        &linked,
        &[
            "review",
            "submit",
            "--review",
            review.id.as_str(),
            "--verdict",
            "approve",
        ],
    );
    assert_eq!(ran.exit, Exit::Ok, "{}", ran.stderr);
    assert_eq!(
        fixture.repo.git(&[
            "show",
            &format!("refs/dv/reviews/{}/1:src/app.ts", review.id)
        ]),
        text_of(&edited_lines()),
        "the main work tree's edit, not the linked worktree's committed file"
    );
}

#[test]
fn close_and_delete_are_the_reviewers() {
    let mut fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    later();
    let other = fixture.review(Target::Staged);

    // An agent may do neither, nor submit; nothing is written.
    fixture.env.insert("CLAUDECODE".into(), "1".into());
    let count = fixture.events(&review.id).len();
    let error = fixture
        .run(&["review", "close", review.id.as_str(), "--json"])
        .error(Exit::Failure, "not_allowed");
    assert_eq!(
        error["message"],
        "an agent can't set a review's status to closed"
    );
    let error = fixture
        .run(&["review", "delete", review.id.as_str(), "--json"])
        .error(Exit::Failure, "not_allowed");
    assert_eq!(
        error["message"],
        "an agent can't delete a review: only the reviewer deletes reviews"
    );
    let error = fixture
        .run(&[
            "review",
            "submit",
            review.id.as_str(),
            "--verdict",
            "approve",
            "--json",
        ])
        .error(Exit::Failure, "not_allowed");
    assert_eq!(error["message"], "an agent can't finish a round");
    assert_eq!(fixture.events(&review.id).len(), count);
    fixture.env.remove("CLAUDECODE");

    // The reviewer closes; closing again changes nothing.
    let json = fixture
        .run(&["review", "close", review.id.as_str(), "--json"])
        .ok_json();
    assert_eq!(json["changed"], true);
    assert_eq!(json["review"]["status"], "closed");
    assert!(matches!(
        fixture.load(&review.id).status_change,
        Some(StatusChange {
            status: ReviewStatus::Closed,
            author: Some(Author {
                kind: AuthorKind::Reviewer,
                ..
            }),
            ..
        })
    ));
    let ran = fixture.run(&["review", "close", review.id.as_str()]);
    assert!(
        ran.stdout.starts_with("unchanged (already closed): "),
        "{}",
        ran.stdout
    );

    // The reviewer deletes: the log and the round snapshots go.
    fixture.finish(&other.id, Verdict::RequestChanges);
    let refs = |id: &ReviewId| {
        fixture
            .repo
            .git(&["for-each-ref", &format!("refs/dv/reviews/{id}")])
    };
    assert!(!refs(&other.id).is_empty());
    let json = fixture
        .run(&["review", "delete", other.id.as_str(), "--json"])
        .ok_json();
    assert_eq!(json, json!({"schema": 1, "deleted": other.id.as_str()}));
    assert_eq!(refs(&other.id), "");
    assert!(fixture.store().load(&other.id).is_err());
    let error = fixture
        .run(&["review", "delete", other.id.as_str(), "--json"])
        .error(Exit::Failure, "not_found");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with(&format!("no review {} in ", other.id))
    );
    let ran = fixture.run(&["review", "delete", review.id.as_str()]);
    assert_eq!(ran.lines(), [format!("deleted {}", review.id)]);
    // The id is required, and must be one.
    fixture
        .run(&["review", "delete", "--json"])
        .error(Exit::Usage, "usage");
    fixture
        .run(&["review", "delete", "r-../x", "--json"])
        .error(Exit::Usage, "usage");
}
