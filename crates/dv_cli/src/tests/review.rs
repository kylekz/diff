//! `dv review list`, `dv review show` and which review a command means (docs/PLAN.md §10.1,
//! §10.2).

use dv_core::review::{ReviewId, ReviewStatus, Verdict, ops};
use dv_core::target::Target;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::*;

fn ids(reviews: &Value) -> Vec<&str> {
    reviews
        .as_array()
        .unwrap()
        .iter()
        .map(|review| review["id"].as_str().unwrap())
        .collect()
}

fn candidate_ids(error: &Value) -> Vec<&str> {
    ids(&error["candidates"])
}

#[test]
fn list_shows_the_open_reviews_or_all() {
    let fixture = Fixture::new();
    let ran = fixture.run(&["review", "list"]);
    assert_eq!(ran.lines(), ["no open reviews"]);
    assert_eq!(
        fixture.run(&["review", "list", "--all"]).lines(),
        ["no reviews"]
    );

    let working = fixture.review(Target::Working);
    later();
    let commit = fixture.review(Target::Commit { rev: "HEAD".into() });
    ops::set_status(
        &fixture.store(),
        &commit.id,
        ReviewStatus::Approved,
        &reviewer(),
        None,
    )
    .unwrap();

    let json = fixture.run(&["review", "list", "--json"]).ok_json();
    assert_eq!(ids(&json["reviews"]), [working.id.as_str()]);
    let root = fixture.backend().repo().toplevel.clone();
    assert_eq!(
        json["reviews"][0],
        json!({
            "id": working.id.as_str(),
            "target": {"kind": "working", "label": "working tree vs main"},
            "status": "open",
            "round": 1,
            "rounds": [],
            "threads": {"open": 0, "addressed": 0, "resolved": 0, "annotation": 0},
            "worktree": root,
            "created_at": working.created_at,
            "updated_at": working.updated_at,
            "status_note": null,
            "pr": null,
            "submissions": [],
        })
    );
    assert_eq!(json["unreadable"], json!([]));

    let json = fixture
        .run(&["review", "list", "--all", "--json"])
        .ok_json();
    assert_eq!(
        ids(&json["reviews"]),
        [commit.id.as_str(), working.id.as_str()],
        "newest first"
    );
    assert_eq!(
        json["reviews"][0]["target"],
        json!({"kind": "commit", "rev": "HEAD", "label": "commit HEAD"})
    );
    assert_eq!(json["reviews"][0]["status"], "approved");
    assert_eq!(
        json["reviews"][0]["round"], 1,
        "a finished review's last round"
    );

    let json = fixture
        .run(&["review", "list", "--status", "approved", "--json"])
        .ok_json();
    assert_eq!(ids(&json["reviews"]), [commit.id.as_str()]);

    let ran = fixture.run(&["review", "list", "--all"]);
    assert_eq!(
        ran.lines(),
        [
            format!(
                "{}  commit HEAD  approved  round 1  0 open, 0 addressed, 0 resolved",
                commit.id
            ),
            format!(
                "{}  working tree vs main  open  round 1  0 open, 0 addressed, 0 resolved",
                working.id
            ),
        ]
    );
}

#[test]
fn a_log_that_does_not_load_is_listed_apart() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    let broken = ReviewId::generate();
    let reviews = fixture.repo.path().join(".git/dv/reviews");
    std::fs::write(reviews.join(format!("{broken}.jsonl")), "not json\n").unwrap();

    let json = fixture.run(&["review", "list", "--json"]).ok_json();
    assert_eq!(ids(&json["reviews"]), [review.id.as_str()]);
    assert_eq!(json["unreadable"][0]["id"], broken.as_str());
    assert!(
        json["unreadable"][0]["error"]
            .as_str()
            .unwrap()
            .contains("unreadable")
    );

    let ran = fixture.run(&["review", "list"]);
    assert!(
        ran.lines()[1].starts_with(&format!("{broken}  unreadable: ")),
        "{}",
        ran.stdout
    );
}

#[test]
fn show_prints_the_review_with_its_rounds() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    fixture.thread(&review, "src/app.ts", 10, 10, "Why 100?");
    let finished = ops::finish_round(
        &fixture.store(),
        &fixture.git(),
        &review.id,
        Some(Verdict::RequestChanges),
        Some("Two things.\nSee the threads.".into()),
        &reviewer(),
    )
    .unwrap();

    let json = fixture.run(&["review", "show", "--json"]).ok_json();
    let shown = &json["review"];
    assert_eq!(shown["id"], review.id.as_str());
    assert_eq!(shown["status"], "changes_requested");
    assert_eq!(shown["round"], 2);
    assert_eq!(
        shown["rounds"],
        json!([{
            "n": 1,
            "verdict": "request_changes",
            "body": "Two things.\nSee the threads.",
            "closed_at": finished.round.closed_at,
        }])
    );
    assert_eq!(
        shown["threads"],
        json!({"open": 1, "addressed": 0, "resolved": 0, "annotation": 0})
    );
    // The verdict's status change carries no note.
    assert_eq!(shown["status_note"], json!(null));

    let ran = fixture.run(&["review", "show"]);
    let root = fixture.backend().repo().toplevel.clone();
    assert_eq!(
        ran.lines(),
        [
            format!("{}  working tree vs main", review.id),
            "status: changes_requested".to_string(),
            "round: 2".to_string(),
            "threads: 1 open, 0 addressed, 0 resolved".to_string(),
            format!("worktree: {root}"),
            "round 1 closed: request_changes".to_string(),
            "    Two things.".to_string(),
            "    See the threads.".to_string(),
        ]
    );

    // A later round: the earlier keep their first line, the last shows its summary in full.
    ops::finish_round(
        &fixture.store(),
        &fixture.git(),
        &review.id,
        Some(Verdict::RequestChanges),
        Some("Almost.\n\n- one more test".into()),
        &reviewer(),
    )
    .unwrap();
    let ran = fixture.run(&["review", "show"]);
    assert_eq!(
        ran.lines()[5..],
        [
            "round 1 closed: request_changes: Two things.",
            "round 2 closed: request_changes",
            "    Almost.",
            "",
            "    - one more test",
        ]
    );
}

#[test]
fn a_command_means_the_one_open_review_of_the_work_checked_out() {
    let mut fixture = Fixture::new();

    // No review at all.
    let error = fixture
        .run(&["review", "show", "--json"])
        .error(Exit::Usage, "no_review");
    assert_eq!(error["candidates"], json!([]));
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("no open review in this repository")
    );

    // An open review of something else isn't the current work's, but is a candidate.
    let commit = fixture.review(Target::Commit { rev: "HEAD".into() });
    let error = fixture
        .run(&["review", "show", "--json"])
        .error(Exit::Usage, "no_review");
    assert_eq!(
        error["candidates"],
        json!([{
            "id": commit.id.as_str(),
            "label": "commit HEAD",
            "status": "open",
            "round": 1,
            "worktree": fixture.backend().repo().toplevel.clone(),
        }])
    );
    let ran = fixture.run(&["review", "show"]);
    assert_eq!(ran.exit, Exit::Usage);
    assert_eq!(ran.stdout, "", "no JSON without --json");
    assert_eq!(
        ran.stderr,
        format!(
            "dv: no open review of the work checked out here; name one with --review <id>\n  \
             {}  commit HEAD  open  round 1  {}\n",
            commit.id,
            fixture.backend().repo().toplevel
        )
    );

    // One review of the working tree: that one.
    later();
    let working = fixture.review(Target::Working);
    let json = fixture.run(&["review", "show", "--json"]).ok_json();
    assert_eq!(json["review"]["id"], working.id.as_str());

    // Two that match: ambiguous, with every open review as a candidate.
    later();
    let staged = fixture.review(Target::Staged);
    let error = fixture
        .run(&["review", "show", "--json"])
        .error(Exit::Usage, "ambiguous_review");
    assert_eq!(
        candidate_ids(&error),
        [staged.id.as_str(), working.id.as_str(), commit.id.as_str()]
    );

    // Named: the argument, --review, then $DV_REVIEW.
    let shown = |fixture: &Fixture, args: &[&str]| {
        let mut line = vec!["review", "show", "--json"];
        line.extend_from_slice(args);
        fixture.run(&line).ok_json()["review"]["id"].clone()
    };
    assert_eq!(shown(&fixture, &[staged.id.as_str()]), staged.id.as_str());
    assert_eq!(
        shown(&fixture, &["--review", commit.id.as_str()]),
        commit.id.as_str()
    );
    fixture
        .env
        .insert("DV_REVIEW".into(), working.id.to_string());
    assert_eq!(shown(&fixture, &[]), working.id.as_str());
    assert_eq!(
        shown(&fixture, &["--review", staged.id.as_str()]),
        staged.id.as_str(),
        "--review beats $DV_REVIEW"
    );
    // Even a finished review, once named.
    ops::set_status(
        &fixture.store(),
        &commit.id,
        ReviewStatus::Closed,
        &reviewer(),
        None,
    )
    .unwrap();
    assert_eq!(shown(&fixture, &[commit.id.as_str()]), commit.id.as_str());

    // Naming it wrongly.
    fixture
        .run(&[
            "review",
            "show",
            staged.id.as_str(),
            "--review",
            working.id.as_str(),
            "--json",
        ])
        .error(Exit::Usage, "usage");
    fixture
        .run(&["review", "show", "r-nope", "--json"])
        .error(Exit::Usage, "usage");
    fixture.env.insert("DV_REVIEW".into(), "../etc".into());
    let error = fixture
        .run(&["review", "show", "--json"])
        .error(Exit::Usage, "usage");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("$DV_REVIEW: ")
    );
    fixture.env.remove("DV_REVIEW");
    let missing = ReviewId::generate();
    let error = fixture
        .run(&["review", "show", missing.as_str(), "--json"])
        .error(Exit::Failure, "not_found");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with(&format!("no review {missing} in "))
    );
}

#[test]
fn a_linked_worktree_and_a_branch_have_their_own_context() {
    let fixture = Fixture::new();
    let working = fixture.review(Target::Working);
    let linked = fixture.repo.add_worktree("feature", "feat");

    // The main work tree's working review isn't the linked worktree's work.
    let error = fixture
        .run_in(&linked, &["review", "show", "--json"])
        .error(Exit::Usage, "no_review");
    assert_eq!(candidate_ids(&error), [working.id.as_str()]);

    // A review of the branch checked out there is, however it is spelled.
    later();
    let branch = ops::open_review(
        &ReviewStore::new(fixture.backend_at(&linked)),
        Target::Branch {
            branch: "refs/heads/feat".into(),
            base: Some("main".into()),
        },
        reviewer(),
    )
    .unwrap()
    .review;
    let json = fixture
        .run_in(&linked, &["review", "show", "--json"])
        .ok_json();
    assert_eq!(json["review"]["id"], branch.id.as_str());
    assert_eq!(
        json["review"]["target"],
        json!({"kind": "branch", "branch": "refs/heads/feat", "base": "main", "label": "refs/heads/feat vs main"})
    );
    // From the main work tree (on `main`) the working review is still the one.
    let json = fixture.run(&["review", "show", "--json"]).ok_json();
    assert_eq!(json["review"]["id"], working.id.as_str());
    // A subdirectory is the same work tree.
    let json = fixture
        .run_in(&fixture.path().join("src"), &["review", "show", "--json"])
        .ok_json();
    assert_eq!(json["review"]["id"], working.id.as_str());
    // So is --repo, from anywhere.
    let outside = fixture.outside("elsewhere");
    let repo = linked.to_string_lossy().into_owned();
    let json = fixture
        .run_in(
            &outside,
            &["review", "show", "--json", "--repo", repo.as_str()],
        )
        .ok_json();
    assert_eq!(json["review"]["id"], branch.id.as_str());
}

#[test]
fn another_worktrees_working_review_is_named_by_its_own_head() {
    let fixture = Fixture::new();
    let linked = fixture.repo.add_worktree("wt", "feature");
    std::fs::write(linked.join("README.md"), "# api, changed\n").unwrap();
    let json = fixture
        .run_in(&linked, &["review", "new", "--json"])
        .ok_json();
    assert_eq!(json["review"]["target"]["label"], "working tree vs feature");
    let id = json["review"]["id"].as_str().unwrap().to_string();
    let worktree = json["review"]["worktree"].clone();

    // From the main checkout (on main), it is still "vs feature": in the review, the list, and
    // as a candidate, which also says which worktree it is.
    let shown = fixture
        .run(&["review", "show", id.as_str(), "--json"])
        .ok_json();
    assert_eq!(
        shown["review"]["target"]["label"],
        "working tree vs feature"
    );
    let listed = fixture.run(&["review", "list", "--json"]).ok_json();
    assert_eq!(
        listed["reviews"][0]["target"]["label"],
        "working tree vs feature"
    );
    let error = fixture
        .run(&["comment", "list", "--json"])
        .error(Exit::Usage, "no_review");
    assert_eq!(
        error["candidates"],
        json!([{
            "id": id,
            "label": "working tree vs feature",
            "status": "open",
            "round": 1,
            "worktree": worktree,
        }])
    );
    // The main checkout's own working review is "vs main" from the linked worktree.
    later();
    let main = fixture.review(Target::Working);
    let listed = fixture
        .run_in(&linked, &["review", "list", "--json"])
        .ok_json();
    let labels: Vec<(&str, &str)> = listed["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .map(|review| {
            (
                review["id"].as_str().unwrap(),
                review["target"]["label"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        labels,
        [
            (main.id.as_str(), "working tree vs main"),
            (id.as_str(), "working tree vs feature"),
        ]
    );
}
