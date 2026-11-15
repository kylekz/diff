//! GitHub's threads placed in the diff a review shows (PLAN §9.6): their anchors read from
//! the commits GitHub names, relocated when the review shows another head, the ones without
//! a place, the code GitHub shows for one, and Resolve / Unresolve.

use serde_json::json;

use super::*;

mod guard;
use crate::diff::{DiffLimits, FileDiff, file_diff, file_list, side_file};
use crate::github::threads::{self, GithubAnchor, Standing, ThreadTexts, hunk_rows};
use crate::review::{
    AnchorSide, Placement, ReviewStatus, SideText, is_gone, moved_since, ops,
    place_unlisted_anchor, side_lines,
};
use crate::target::{DiffSpec, Side};

/// `src/api.rs` on `main`.
const BASE: &str = "\
fn alpha() {
    one();
}

fn beta() {
    two();
}

fn gamma() {
    three();
}
";

/// `src/api.rs` at the pull request's first head: `beta` changed, `delta` added.
const FIRST: &str = "\
fn alpha() {
    one();
}

fn beta() {
    two_and_a_half();
}

fn gamma() {
    three();
}

fn delta() {
    four();
}
";

/// `src/api.rs` at its second head: three lines above everything, and a line in `delta`.
const SECOND: &str = "\
// The api.
// Second line.

fn alpha() {
    one();
}

fn beta() {
    two_and_a_half();
}

fn gamma() {
    three();
}

fn delta() {
    four();
    five();
}
";

/// The fixture GitHub with `src/api.rs` and `notes.txt` on `main`, and pull request 7
/// changing the first ([`FIRST`]) and removing the second.
fn fixture() -> PrFixture {
    let github = FixtureGithub::new("threads");
    let author = github.author();
    author
        .write("src/api.rs", BASE)
        .write("notes.txt", "one\ntwo\n");
    author.commit("Add the api");
    github.publish("main");
    author
        .branch("feat")
        .checkout("feat")
        .write("src/api.rs", FIRST)
        .remove("notes.txt");
    author.commit("Change beta");
    let head = github.open_pr(NUMBER, "feat");
    let mut pr = GhPr::new(NUMBER, &head);
    pr.files = files(&[("src/api.rs", "modified"), ("notes.txt", "removed")]);
    let fixture = PrFixture {
        github,
        // No test here waits out the pace between two mutations.
        mock: MockGh::with(Value::Array(Vec::new()), |options| {
            options.mutation_gap = std::time::Duration::ZERO;
        }),
        pr,
    };
    fixture.answer(Vec::new());
    fixture
}

fn files(files: &[(&str, &str)]) -> Vec<Value> {
    files
        .iter()
        .map(|(path, status)| fixture_gh::file(path, status, Some("@@ -1 +1 @@")))
        .collect()
}

/// A push to the pull request: `write` changes the author's `feat`, which `gh` reports as
/// the head from now on, changing `files`.
fn push(
    fixture: &mut PrFixture,
    files: Vec<Value>,
    write: impl FnOnce(&dv_test_support::FixtureRepo),
) -> Oid {
    let author = fixture.github.author();
    author.checkout("feat");
    write(author);
    author.commit("Push");
    let head = fixture.github.open_pr(NUMBER, "feat");
    fixture.pr.head = head.clone();
    fixture.pr.files = files;
    fixture.answer(Vec::new());
    oid(&head)
}

/// A thread on `line` of `path`'s new side in `commit`.
fn on_line(id: &str, path: &str, line: u32, commit: &Oid) -> Value {
    fixture_gh::thread(id, path, line, "octocat", "Why?", commit.as_str())
}

/// `thread` with `fields` set on it.
fn with(mut thread: Value, fields: Value) -> Value {
    for (name, value) in fields.as_object().unwrap() {
        thread[name] = value.clone();
    }
    thread
}

/// `thread`, which GitHub calls outdated: made on `original_line` of `original`, with
/// `hunk` above it.
fn outdated(mut thread: Value, original_line: u32, original: &Oid, hunk: &str) -> Value {
    thread["isOutdated"] = json!(true);
    thread["line"] = Value::Null;
    thread["originalLine"] = json!(original_line);
    let comment = &mut thread["comments"]["nodes"][0];
    comment["outdated"] = json!(true);
    comment["commit"] = Value::Null;
    comment["originalCommit"] = json!({"oid": original.as_str()});
    comment["diffHunk"] = json!(hunk);
    thread
}

/// The mirror a refresh of `review` writes once `gh` tells `threads`.
fn mirror_with(fixture: &mut PrFixture, review: &Review, threads: Vec<Value>) -> Mirror {
    fixture.pr.threads = threads;
    fixture.answer(Vec::new());
    let refreshed = fixture.refresh(review);
    assert!(refreshed.stale.is_none(), "{:?}", refreshed.stale);
    *refreshed.mirror.unwrap()
}

/// Where each of `mirror`'s threads stands in `spec`, placed as the CLI and the app place
/// them: among the files `spec` lists, then, for one whose file it doesn't list, in that
/// file as its new side holds it. Each as `<standing> <placement>`: `unchanged path:6-6`,
/// `moved path:9-9`, `outdated path`, `outdated -`, `file path`, `unlisted path:1-1`.
fn placed(fixture: &PrFixture, mirror: &Mirror, spec: &DiffSpec) -> Vec<String> {
    let git = fixture.git();
    let entries = file_list(&git, spec).unwrap();
    threads::anchor_threads(&git, mirror, spec, &ThreadTexts::default())
        .iter()
        .map(|anchored| {
            let placement = place(&git, spec, &entries, anchored);
            let anchor = &anchored.anchor;
            let lines = placement
                .lines(anchor)
                .map_or(String::new(), |(start, end)| format!(":{start}-{end}"));
            let place = match &placement {
                Placement::Unlisted { path, .. } => format!("unlisted {path}{lines}"),
                Placement::UnlistedFile { path } => format!("unlisted-file {path}"),
                Placement::Outdated { file, .. } => {
                    format!("outdated {}", file.as_deref().unwrap_or("-"))
                }
                _ if moved_since(anchor, anchor, &placement) => {
                    format!("moved {}{lines}", placement.file().unwrap())
                }
                other => format!("{} {}{lines}", other.as_str(), other.file().unwrap()),
            };
            let standing = match anchored.standing {
                Standing::Anchored => "anchored",
                Standing::OutdatedOnGithub => "outdated-on-github",
                Standing::Unplaced => "unplaced",
            };
            format!("{standing} {place}")
        })
        .collect()
}

fn place(
    git: &GitRepo,
    spec: &DiffSpec,
    entries: &[crate::diff::FileEntry],
    anchored: &GithubAnchor,
) -> Placement {
    let lines = |diff: Option<FileDiff>, side| match diff.as_ref().and_then(FileDiff::as_text) {
        Some(text) => SideText::Lines(
            side_lines(text, side)
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        ),
        None => SideText::Unavailable,
    };
    let placement = threads::place(
        anchored,
        |path, old| threads::thread_entry(path, old, entries),
        |entry, side| {
            lines(
                file_diff(git, spec, entry, &DiffLimits::UNLIMITED).ok(),
                side,
            )
        },
    );
    if anchored.standing != Standing::Anchored || !is_gone(&placement) {
        return placement;
    }
    let path = &anchored.anchor.path;
    let read = side_file(git, &spec.new, path, &DiffLimits::UNLIMITED).unwrap();
    place_unlisted_anchor(
        &anchored.anchor,
        path,
        read.is_some(),
        lines(read, AnchorSide::New),
    )
}

#[test]
fn a_thread_stands_where_github_has_it_when_the_review_shows_that_head() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let head = fixture.head();
    let threads = vec![
        on_line("T_line", "src/api.rs", 6, &head),
        with(
            on_line("T_range", "src/api.rs", 15, &head),
            json!({"startLine": 13, "originalStartLine": 13, "startDiffSide": "RIGHT"}),
        ),
        // A range that starts on the old side is anchored at its last line.
        with(
            on_line("T_across", "src/api.rs", 6, &head),
            json!({"startLine": 6, "originalStartLine": 6, "startDiffSide": "LEFT"}),
        ),
        with(
            on_line("T_old", "src/api.rs", 6, &head),
            json!({"diffSide": "LEFT"}),
        ),
        with(
            on_line("T_file", "src/api.rs", 1, &head),
            json!({"subjectType": "FILE", "line": null, "originalLine": null}),
        ),
        // A removed file's comment is on its old side.
        with(
            on_line("T_removed", "notes.txt", 1, &head),
            json!({"subjectType": "FILE", "line": null, "originalLine": null}),
        ),
    ];
    let mirror = mirror_with(&mut fixture, &opened.review, threads);
    let spec = fixture.pins(&opened.review).spec();
    assert_eq!(mirror.merge_base_oid, Some(fixture.main()));
    assert_eq!(
        placed(&fixture, &mirror, &spec),
        [
            "anchored unchanged src/api.rs:6-6",
            "anchored unchanged src/api.rs:13-15",
            "anchored unchanged src/api.rs:6-6",
            "anchored unchanged src/api.rs:6-6",
            "anchored file src/api.rs",
            "anchored file notes.txt",
        ]
    );

    let anchors = threads::anchor_threads(&fixture.git(), &mirror, &spec, &ThreadTexts::default());
    let line = &anchors[0].anchor;
    assert_eq!((line.round, line.side), (1, AnchorSide::New));
    assert_eq!(line.text, ["    two_and_a_half();"]);
    assert_eq!(line.context_before, ["}", "", "fn beta() {"]);
    assert_eq!(line.context_after, ["}", "", "fn gamma() {"]);
    assert_eq!(anchors[1].anchor.text, ["fn delta() {", "    four();", "}"]);
    let old = &anchors[3].anchor;
    assert_eq!(
        (old.side, old.text.as_slice()),
        (AnchorSide::Old, &["    two();".to_string()][..])
    );
    assert!(anchors[4].anchor.is_file_level());
    assert_eq!(anchors[4].anchor.side, AnchorSide::New);
    assert_eq!(anchors[5].anchor.side, AnchorSide::Old);
}

#[test]
fn a_thread_is_relocated_when_the_review_shows_another_head() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let first = fixture.pins(&opened.review);
    let at_first = mirror_with(
        &mut fixture,
        &opened.review,
        vec![on_line("T_beta", "src/api.rs", 6, &first.head)],
    );
    let second = push(&mut fixture, files(&[("src/api.rs", "modified")]), |repo| {
        repo.write("src/api.rs", SECOND);
    });
    let at_second = mirror_with(
        &mut fixture,
        &opened.review,
        vec![
            on_line("T_beta", "src/api.rs", 9, &second),
            // A line the first head doesn't have.
            on_line("T_five", "src/api.rs", 18, &second),
            with(
                on_line("T_old", "src/api.rs", 6, &second),
                json!({"diffSide": "LEFT"}),
            ),
        ],
    );
    let now = fixture.pins(&opened.review);
    assert_eq!(
        (now.head.clone(), at_second.pr.head_oid.clone()),
        (second.clone(), second)
    );

    // The head the mirror was read at: exact.
    assert_eq!(
        placed(&fixture, &at_second, &now.spec()),
        [
            "anchored unchanged src/api.rs:9-9",
            "anchored unchanged src/api.rs:18-18",
            "anchored unchanged src/api.rs:6-6",
        ]
    );
    // An older head than the mirror's: up to where the code stood, gone where it didn't,
    // and the old side read at the mirror's merge base.
    assert_eq!(
        placed(&fixture, &at_second, &first.spec()),
        [
            "anchored moved src/api.rs:6-6",
            "anchored outdated src/api.rs",
            "anchored unchanged src/api.rs:6-6",
        ]
    );
    // A newer head than the mirror's: down.
    assert_eq!(
        placed(&fixture, &at_first, &now.spec()),
        ["anchored moved src/api.rs:9-9"]
    );
}

#[test]
fn an_old_side_dv_cant_tell_falls_back_to_the_reviews_own() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let first = fixture.pins(&opened.review);
    let second = push(&mut fixture, files(&[("src/api.rs", "modified")]), |repo| {
        repo.write("src/api.rs", SECOND);
    });
    let old = with(
        on_line("T_old", "src/api.rs", 6, &second),
        json!({"diffSide": "LEFT"}),
    );
    let mut mirror = mirror_with(&mut fixture, &opened.review, vec![old]);
    mirror.merge_base_oid = None;
    // Neither the head the review shows nor a merge base tells the old side: what is left
    // is the review's own, which stands when the thread's hunk agrees with it.
    assert_eq!(
        placed(&fixture, &mirror, &first.spec()),
        ["anchored unchanged src/api.rs:6-6"]
    );
    let now = fixture.pins(&opened.review);
    assert_eq!(
        placed(&fixture, &mirror, &now.spec()),
        ["anchored unchanged src/api.rs:6-6"]
    );
    // A review whose old side isn't a commit's has none to read it from.
    let unborn = DiffSpec {
        old: Side::EmptyTree,
        new: first.spec().new,
    };
    assert_eq!(placed(&fixture, &mirror, &unborn), ["unplaced outdated -"]);
}

#[test]
fn an_outdated_thread_is_placed_only_at_the_commit_it_was_made_on() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let first = fixture.pins(&opened.review);
    push(&mut fixture, files(&[("src/api.rs", "modified")]), |repo| {
        repo.write("src/api.rs", SECOND);
    });
    let hunk =
        "@@ -3,7 +3,7 @@ fn alpha() {\n }\n \n fn beta() {\n-    two();\n+    two_and_a_half();";
    let thread = outdated(
        on_line("T_was", "src/api.rs", 6, &first.head),
        6,
        &first.head,
        hunk,
    );
    let mirror = mirror_with(&mut fixture, &opened.review, vec![thread]);
    let now = fixture.pins(&opened.review);
    assert_ne!(now.head, first.head);

    // The review shows a later head: no place in the diff, the hunk's line as its code.
    assert_eq!(
        placed(&fixture, &mirror, &now.spec()),
        ["outdated-on-github outdated -"]
    );
    let anchors = threads::anchor_threads(
        &fixture.git(),
        &mirror,
        &now.spec(),
        &ThreadTexts::default(),
    );
    let anchor = &anchors[0].anchor;
    assert_eq!(
        (anchor.path.as_str(), anchor.start_line, anchor.end_line),
        ("src/api.rs", 6, 6)
    );
    assert_eq!(anchor.text, ["    two_and_a_half();"]);
    assert!(anchor.context_before.is_empty() && anchor.context_after.is_empty());

    // A review of the commit it was made on (a submitted one): at its original line.
    assert_eq!(
        placed(&fixture, &mirror, &first.spec()),
        ["anchored unchanged src/api.rs:6-6"]
    );
}

#[test]
fn a_thread_follows_its_file_through_a_rename_either_way() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let first = fixture.pins(&opened.review);
    let renamed = vec![json!({
        "filename": "src/service.rs", "previous_filename": "src/api.rs", "status": "renamed",
        "patch": "@@ -1 +1 @@"
    })];
    let second = push(&mut fixture, renamed, |repo| {
        repo.rename("src/api.rs", "src/service.rs");
    });
    let mirror = mirror_with(
        &mut fixture,
        &opened.review,
        vec![
            on_line("T_new", "src/service.rs", 6, &second),
            with(
                on_line("T_old", "src/service.rs", 6, &second),
                json!({"diffSide": "LEFT"}),
            ),
        ],
    );
    let now = fixture.pins(&opened.review);
    assert_eq!(
        placed(&fixture, &mirror, &now.spec()),
        [
            "anchored unchanged src/service.rs:6-6",
            "anchored unchanged src/service.rs:6-6",
        ]
    );
    let anchors = threads::anchor_threads(
        &fixture.git(),
        &mirror,
        &now.spec(),
        &ThreadTexts::default(),
    );
    assert_eq!(
        (
            anchors[0].anchor.path.as_str(),
            anchors[0].anchor.old_path.as_deref()
        ),
        ("src/service.rs", Some("src/api.rs"))
    );
    // The old side is read, and named, under the file's old name.
    assert_eq!(
        (
            anchors[1].anchor.path.as_str(),
            anchors[1].anchor.text.as_slice()
        ),
        ("src/api.rs", &["    two();".to_string()][..])
    );
    // The head before the rename lists the file under its old name.
    assert_eq!(
        placed(&fixture, &mirror, &first.spec()),
        [
            "anchored unchanged src/api.rs:6-6",
            "anchored unchanged src/api.rs:6-6",
        ]
    );
}

#[test]
fn a_thread_on_a_file_the_review_doesnt_list_is_read_in_that_file() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let first = fixture.pins(&opened.review);
    let second = push(
        &mut fixture,
        files(&[
            ("src/api.rs", "modified"),
            ("README.md", "modified"),
            ("src/new.rs", "added"),
        ]),
        |repo| {
            repo.write("README.md", "# threads\nMore.\n")
                .write("src/new.rs", "fn new() {}\n");
        },
    );
    let mirror = mirror_with(
        &mut fixture,
        &opened.review,
        vec![
            on_line("T_readme", "README.md", 1, &second),
            on_line("T_added", "README.md", 2, &second),
            on_line("T_new", "src/new.rs", 1, &second),
            with(
                on_line("T_file", "README.md", 1, &second),
                json!({"subjectType": "FILE", "line": null, "originalLine": null}),
            ),
        ],
    );
    // The first head changes neither file: one stands in it, unchanged; the other isn't
    // there.
    assert_eq!(
        placed(&fixture, &mirror, &first.spec()),
        [
            "anchored unlisted README.md:1-1",
            "anchored outdated -",
            "anchored outdated -",
            "anchored unlisted-file README.md",
        ]
    );
}

#[test]
fn a_thread_whose_commit_isnt_here_or_whose_line_isnt_there_has_no_place() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let head = fixture.head();
    let elsewhere = oid(&"f".repeat(40));
    let mirror = mirror_with(
        &mut fixture,
        &opened.review,
        vec![
            // A commit the clone doesn't hold, whose line 6 was other code than the head's.
            with(
                on_line("T_gone", "src/api.rs", 6, &elsewhere),
                json!({"comments": fixture_gh::connection(vec![json!({
                    "id": "T_gone_C1", "fullDatabaseId": "1001", "author": {"login": "octocat"},
                    "body": "Why?", "createdAt": "2026-10-19T08:00:00Z",
                    "url": "https://github.com/dv-fixture/threads/pull/7#discussion_r1001",
                    "diffHunk": "@@ -3,4 +3,4 @@\n }\n \n fn beta() {\n+    two_and_a_quarter();",
                    "outdated": false, "commit": {"oid": elsewhere.as_str()},
                    "originalCommit": {"oid": elsewhere.as_str()}, "replyTo": null,
                    "pullRequestReview": {"id": "PRR_1", "fullDatabaseId": "2001", "state": "COMMENTED"},
                })])}),
            ),
            // The same commit, its line the head's own: GitHub doesn't call it outdated, so
            // its line counts in the head.
            with(
                on_line("T_there", "src/api.rs", 6, &elsewhere),
                json!({"comments": fixture_gh::connection(vec![json!({
                    "id": "T_there_C1", "fullDatabaseId": "1001", "author": {"login": "octocat"},
                    "body": "Why?", "createdAt": "2026-10-19T08:00:00Z",
                    "url": "https://github.com/dv-fixture/threads/pull/7#discussion_r1001",
                    "diffHunk": "@@ -3,4 +3,4 @@\n }\n \n fn beta() {\n+    two_and_a_half();",
                    "outdated": false, "commit": {"oid": elsewhere.as_str()},
                    "originalCommit": {"oid": elsewhere.as_str()}, "replyTo": null,
                    "pullRequestReview": {"id": "PRR_1", "fullDatabaseId": "2001", "state": "COMMENTED"},
                })])}),
            ),
            on_line("T_past", "src/api.rs", 400, &head),
            on_line("T_nowhere", "src/none.rs", 1, &head),
            with(
                on_line("T_unsaid", "src/api.rs", 6, &head),
                json!({"line": null}),
            ),
        ],
    );
    let spec = fixture.pins(&opened.review).spec();
    assert_eq!(
        placed(&fixture, &mirror, &spec),
        [
            "unplaced outdated -",
            "anchored unchanged src/api.rs:6-6",
            "unplaced outdated -",
            "unplaced outdated -",
            "unplaced outdated -",
        ]
    );
}

/// PLAN §9.5: the pins, never the mirror's head, are what a review shows.
#[test]
fn a_review_whose_pins_stayed_behind_its_mirror_relocates() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let first = fixture.pins(&opened.review);
    ops::set_status(
        &fixture.store(),
        &opened.review.id,
        ReviewStatus::Closed,
        &reviewer(),
        None,
    )
    .unwrap();
    let closed = fixture.store().load(&opened.review.id).unwrap().review;
    let second = push(&mut fixture, files(&[("src/api.rs", "modified")]), |repo| {
        repo.write("src/api.rs", SECOND);
    });
    let mirror = mirror_with(
        &mut fixture,
        &closed,
        vec![on_line("T_beta", "src/api.rs", 9, &second)],
    );
    assert_eq!(mirror.pr.head_oid, second);
    assert_eq!(fixture.pins(&closed), first);
    let spec = pins::resolve(&fixture.git(), &closed.id, &closed.target).unwrap();
    assert_eq!(spec.new, Side::Commit(first.head));
    assert_eq!(
        placed(&fixture, &mirror, &spec),
        ["anchored moved src/api.rs:6-6"]
    );
}

#[test]
fn the_code_github_shows_ends_at_the_commented_lines() {
    let hunk = "@@ -3,7 +3,8 @@ fn alpha() {\n }\n \n fn beta() {\n-    two();\n+    two_and_a_half();\n+    more();\n\\ No newline at end of file";
    let rows = |side, count| -> Vec<String> {
        hunk_rows(hunk, side, count)
            .into_iter()
            .map(|row| {
                let mark = match row.kind {
                    crate::diff::LineKind::Added => '+',
                    crate::diff::LineKind::Removed => '-',
                    crate::diff::LineKind::Context => ' ',
                };
                format!(
                    "{}{mark}{}",
                    if row.commented { '>' } else { ' ' },
                    row.text
                )
            })
            .collect()
    };
    assert_eq!(
        rows(AnchorSide::New, 1),
        [
            "  fn beta() {",
            " -    two();",
            " +    two_and_a_half();",
            ">+    more();",
        ]
    );
    // A range's lines are its side's: the removed one between them isn't commented.
    assert_eq!(
        rows(AnchorSide::New, 3),
        [
            "  }",
            "  ",
            "> fn beta() {",
            " -    two();",
            ">+    two_and_a_half();",
            ">+    more();",
        ]
    );
    assert_eq!(
        rows(AnchorSide::Old, 1),
        [
            "  }",
            "  ",
            "  fn beta() {",
            ">-    two();",
            " +    two_and_a_half();",
            " +    more();",
        ]
    );
    // Fewer lines than asked for, no header, nothing at all.
    assert_eq!(rows(AnchorSide::New, 40).len(), 6);
    assert_eq!(hunk_rows("+only", AnchorSide::New, 1).len(), 1);
    assert_eq!(hunk_rows("", AnchorSide::New, 1), []);
}

/// The calls `gh` took, as `<what it was asked>`: a mutation by its name, a query by the
/// field it reads, anything else by its arguments.
fn asked(fixture: &PrFixture) -> Vec<String> {
    fixture
        .mock
        .calls()
        .iter()
        .map(|call| {
            let stdin = call["stdin"].as_str().unwrap_or_default();
            for name in [
                "unresolveReviewThread",
                "resolveReviewThread",
                "reviewThreads",
            ] {
                if stdin.contains(name) {
                    return name.to_string();
                }
            }
            call["argv"][0].as_str().unwrap().to_string()
        })
        .collect()
}

#[test]
fn resolving_changes_github_then_reads_the_mirror_again() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let head = fixture.head();
    let thread = on_line("PRRT_1", "src/api.rs", 6, &head);
    let mirror = mirror_with(&mut fixture, &opened.review, vec![thread.clone()]);
    assert!(!mirror.threads[0].resolved);
    let events = fixture
        .store()
        .events(&opened.review.id)
        .unwrap()
        .events
        .len();

    // GitHub takes it, and answers the refresh with the thread resolved.
    fixture.pr.threads = vec![with(
        thread.clone(),
        json!({"isResolved": true, "viewerCanResolve": false, "viewerCanUnresolve": true}),
    )];
    fixture.answer(vec![fixture_gh::resolve("PRRT_1", true)]);
    let refreshed = threads::set_resolved(
        &fixture.backend(),
        &fixture.mock.gh,
        &opened.review,
        &mirror.key,
        "PRRT_1",
        true,
    )
    .unwrap();
    assert!(refreshed.resolved);
    let refreshed = refreshed.refreshed;
    assert!(refreshed.stale.is_none(), "{:?}", refreshed.stale);
    assert!(refreshed.fresh);
    let now = refreshed.mirror.unwrap();
    assert!(now.threads[0].resolved && now.threads[0].viewer_can_unresolve);
    let asked_now = asked(&fixture);
    assert_eq!(asked_now[0], "resolveReviewThread");
    assert!(asked_now[1..].contains(&"reviewThreads".to_string()));
    let request: Value =
        serde_json::from_str(fixture.mock.calls()[0]["stdin"].as_str().unwrap()).unwrap();
    assert_eq!(request["variables"], json!({"thread": "PRRT_1"}));
    // GitHub's state, not the review's: nothing in the log, whatever the review's status.
    assert_eq!(
        fixture
            .store()
            .events(&opened.review.id)
            .unwrap()
            .events
            .len(),
        events
    );

    // Unresolve is the other mutation.
    fixture.pr.threads = vec![thread];
    fixture.answer(vec![fixture_gh::resolve("PRRT_1", false)]);
    let refreshed = threads::set_resolved(
        &fixture.backend(),
        &fixture.mock.gh,
        &opened.review,
        &mirror.key,
        "PRRT_1",
        false,
    )
    .unwrap();
    assert!(!refreshed.resolved);
    assert!(!refreshed.refreshed.mirror.unwrap().threads[0].resolved);
    assert_eq!(asked(&fixture)[0], "unresolveReviewThread");
}

#[test]
fn a_resolve_github_refuses_changes_nothing_and_says_which_kind() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let head = fixture.head();
    let thread = on_line("PRRT_1", "src/api.rs", 6, &head);
    let mirror = mirror_with(&mut fixture, &opened.review, vec![thread]);
    let graphql = |kind: &str, message: &str| {
        json!({
            "argv": ["api", "graphql", "**"],
            "stdin_contains": fixture_gh::resolve_request("PRRT_1", true),
            "stdout": {
                "data": {"resolveReviewThread": null},
                "errors": [{"type": kind, "path": ["resolveReviewThread"], "message": message}]
            },
            "stderr": format!("gh: {message}\n"),
            "exit": 1
        })
    };
    let failing = |stderr: &str, exit: u8| {
        json!({
            "argv": ["api", "graphql", "**"],
            "stdin_contains": fixture_gh::resolve_request("PRRT_1", true),
            "stderr": stderr,
            "exit": exit
        })
    };
    let cases = [
        (failing("gh auth login\n", 4), "not_authenticated"),
        (
            graphql(
                "NOT_FOUND",
                "Could not resolve to a node with the global id of 'PRRT_1'",
            ),
            "not_found",
        ),
        (
            graphql("FORBIDDEN", "Resource not accessible by integration"),
            "graphql",
        ),
        (
            graphql("RATE_LIMITED", "API rate limit exceeded for user ID 1."),
            "rate_limited",
        ),
        (failing("gh: HTTP 502\n", 1), "unavailable"),
        (failing("something else\n", 1), "gh"),
        // An answer without the thread's state isn't taken for a success.
        (
            json!({
                "argv": ["api", "graphql", "**"],
                "stdin_contains": fixture_gh::resolve_request("PRRT_1", true),
                "stdout": {"data": {"resolveReviewThread": {"thread": null}}}
            }),
            "unexpected_output",
        ),
    ];
    for (entry, kind) in cases {
        fixture.answer(vec![entry]);
        let error = threads::set_resolved(
            &fixture.backend(),
            &fixture.mock.gh,
            &opened.review,
            &mirror.key,
            "PRRT_1",
            true,
        )
        .unwrap_err();
        assert_eq!(error.kind(), kind, "{error:?}");
        // The mutation alone was asked: no refresh follows a refusal.
        assert_eq!(asked(&fixture), ["resolveReviewThread"], "{kind}");
    }
    match fixture.mirrors().read(&mirror.key).unwrap() {
        MirrorRead::Found(stored) => assert_eq!(*stored, mirror),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_resolve_whose_refresh_fails_keeps_githubs_answer() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let head = fixture.head();
    let thread = on_line("PRRT_1", "src/api.rs", 6, &head);
    let mirror = mirror_with(&mut fixture, &opened.review, vec![thread]);
    // The mutation goes through; the threads can't be read after it.
    fixture.answer(vec![
        fixture_gh::resolve("PRRT_1", true),
        json!({
            "argv": ["api", "graphql", "**"],
            "stdin_contains": "reviewThreads(first",
            "stderr": "gh: HTTP 502\n",
            "exit": 1
        }),
    ]);
    let refreshed = threads::set_resolved(
        &fixture.backend(),
        &fixture.mock.gh,
        &opened.review,
        &mirror.key,
        "PRRT_1",
        true,
    )
    .unwrap();
    assert!(refreshed.resolved);
    let refreshed = refreshed.refreshed;
    assert!(!refreshed.fresh);
    assert_eq!(
        refreshed.stale.as_ref().map(GithubError::kind),
        Some("unavailable")
    );
    let shown = refreshed.mirror.unwrap();
    assert!(shown.threads[0].resolved);
    assert!(!shown.threads[0].viewer_can_resolve && shown.threads[0].viewer_can_unresolve);
    // The stored mirror says the same, as of when it was read.
    match fixture.mirrors().read(&mirror.key).unwrap() {
        MirrorRead::Found(stored) => {
            assert!(stored.threads[0].resolved);
            assert_eq!(stored.read_at, mirror.read_at);
        }
        other => panic!("{other:?}"),
    }
}

/// PLAN §9.4: a refresh that read the mirror and failed after it (a draft's pins) has what
/// GitHub holds; nothing of GitHub's answer is written over it.
#[test]
fn a_resolve_whose_refresh_read_the_mirror_keeps_githubs_own_state() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let head = fixture.head();
    let thread = on_line("PRRT_1", "src/api.rs", 6, &head);
    let mirror = mirror_with(&mut fixture, &opened.review, vec![thread.clone()]);
    // A push, so the refresh has pins to move, which fails.
    let pushed = push(&mut fixture, files(&[("src/api.rs", "modified")]), |repo| {
        repo.write("src/api.rs", SECOND);
    });
    // What GitHub says of the thread after the Resolve, which no guess of dv's would.
    fixture.pr.threads = vec![with(
        thread,
        json!({"isResolved": true, "viewerCanResolve": true, "viewerCanUnresolve": false}),
    )];
    fixture.answer(vec![fixture_gh::resolve("PRRT_1", true)]);
    let failure = crate::backend::BackendError::Git {
        args: vec!["update-ref".to_string()],
        code: Some(128),
        stderr: "fatal: cannot lock ref\n".to_string(),
    };
    let backend = contending::FetchFails::failing_at(
        fixture.github.checkout(),
        |args| matches!(args, ["update-ref", "--stdin"]),
        vec![failure],
    );
    let done = threads::set_resolved(
        &backend,
        &fixture.mock.gh,
        &opened.review,
        &mirror.key,
        "PRRT_1",
        true,
    )
    .unwrap();
    let refreshed = done.refreshed;
    assert!(done.resolved && refreshed.fresh);
    assert_eq!(refreshed.stale.as_ref().map(GithubError::kind), Some("git"));
    let shown = refreshed.mirror.unwrap();
    assert_eq!(shown.pr.head_oid, pushed);
    assert!(shown.read_at >= mirror.read_at);
    assert!(shown.threads[0].resolved);
    assert!(shown.threads[0].viewer_can_resolve && !shown.threads[0].viewer_can_unresolve);
    match fixture.mirrors().read(&mirror.key).unwrap() {
        MirrorRead::Found(stored) => assert_eq!(stored, shown),
        other => panic!("{other:?}"),
    }
}

/// PLAN §9.4: GitHub's answer is written only into the mirror it was worked out from.
#[test]
fn a_resolve_never_writes_over_a_mirror_another_refresh_stored_meanwhile() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let head = fixture.head();
    let thread = on_line("PRRT_1", "src/api.rs", 6, &head);
    let mirror = mirror_with(&mut fixture, &opened.review, vec![thread]);
    // What another process stores between this one's read of the mirror and its write.
    let mut theirs = mirror.clone();
    theirs.read_at += 1_000;
    theirs.pr.title = "Theirs".to_string();
    let path = mirror.key.path();
    fixture.mirrors().write(&theirs).unwrap();
    let bytes = fixture.backend().store_read(&path).unwrap().unwrap();
    fixture.mirrors().write(&mirror).unwrap();
    fixture.answer(vec![
        fixture_gh::resolve("PRRT_1", true),
        json!({
            "argv": ["api", "graphql", "**"],
            "stdin_contains": "reviewThreads(first",
            "stderr": "gh: HTTP 502\n",
            "exit": 1
        }),
    ]);
    let backend = contending::FetchFails::rewriting(fixture.github.checkout(), &path, bytes);
    let done = threads::set_resolved(
        &backend,
        &fixture.mock.gh,
        &opened.review,
        &mirror.key,
        "PRRT_1",
        true,
    )
    .unwrap();
    // What comes back is the mirror this refresh read, with GitHub's answer.
    assert!(done.resolved && !done.refreshed.fresh);
    let shown = done.refreshed.mirror.unwrap();
    assert_eq!(shown.read_at, mirror.read_at);
    assert!(shown.threads[0].resolved);
    // What is stored is theirs, untouched.
    match fixture.mirrors().read(&mirror.key).unwrap() {
        MirrorRead::Found(stored) => assert_eq!(*stored, theirs),
        other => panic!("{other:?}"),
    }
}

/// PLAN §9.4: a refresh that failed before it read the store comes back with no mirror;
/// GitHub's answer goes into the one stored, which the CLI and a later refresh that fails
/// then show.
#[test]
fn a_resolve_whose_refresh_returns_no_mirror_patches_the_stored_one() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let head = fixture.head();
    let thread = on_line("PRRT_1", "src/api.rs", 6, &head);
    let mirror = mirror_with(&mut fixture, &opened.review, vec![thread]);
    fixture.answer(vec![fixture_gh::resolve("PRRT_1", true)]);
    // The review's pins can't be read: the refresh ends before it asks GitHub anything.
    let failure = crate::backend::BackendError::Git {
        args: vec!["for-each-ref".to_string()],
        code: Some(128),
        stderr: "fatal: bad object\n".to_string(),
    };
    let backend = contending::FetchFails::failing_at(
        fixture.github.checkout(),
        |args| {
            args.first() == Some(&"for-each-ref")
                && args.iter().any(|arg| arg.ends_with("/pr-head"))
        },
        vec![failure],
    );
    let done = threads::set_resolved(
        &backend,
        &fixture.mock.gh,
        &opened.review,
        &mirror.key,
        "PRRT_1",
        true,
    )
    .unwrap();
    assert!(done.resolved && !done.refreshed.fresh);
    assert_eq!(asked(&fixture), ["resolveReviewThread"]);
    assert!(done.refreshed.stale.is_some());
    // The stored mirror stands in, with GitHub's answer, as of when it was read.
    let shown = done.refreshed.mirror.unwrap();
    assert!(shown.threads[0].resolved && shown.threads[0].viewer_can_unresolve);
    assert_eq!(shown.read_at, mirror.read_at);
    match fixture.mirrors().read(&mirror.key).unwrap() {
        MirrorRead::Found(stored) => assert_eq!(stored, shown),
        other => panic!("{other:?}"),
    }
    // A later refresh that fails hands the same thread back.
    fixture.answer(vec![json!({
        "argv": ["api", "graphql", "**"],
        "stdin_contains": "reviewThreads(first",
        "stderr": "gh: HTTP 502\n",
        "exit": 1
    })]);
    let later = fixture.refresh(&opened.review);
    assert!(later.stale.is_some() && later.mirror.unwrap().threads[0].resolved);
}
