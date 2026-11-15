//! GitHub's own hunk as the check on where a thread's line is read (PLAN §9.6): a line that
//! counts in another commit than dv assumes is found there or has no place, never a card on
//! other code; and what anchoring costs.

use dv_test_support::GitTrace;

use super::*;
use crate::backend::{LocalBackend, LocalBackendOptions};

/// GitHub's hunk above a comment on `beta`'s changed line: [`BASE`] against [`FIRST`], cut
/// at that line on the new side (`added`) or the old one.
fn beta_hunk(added: bool) -> &'static str {
    if added {
        "@@ -3,7 +3,7 @@ fn alpha() {\n }\n \n fn beta() {\n-    two();\n+    two_and_a_half();"
    } else {
        "@@ -3,7 +3,7 @@ fn alpha() {\n }\n \n fn beta() {\n-    two();"
    }
}

/// `thread` with `hunk` above its comment.
fn with_hunk(mut thread: Value, hunk: &str) -> Value {
    thread["comments"]["nodes"][0]["diffHunk"] = json!(hunk);
    thread
}

fn anchors(fixture: &PrFixture, mirror: &Mirror, spec: &DiffSpec) -> Vec<GithubAnchor> {
    threads::anchor_threads(&fixture.git(), mirror, spec, &ThreadTexts::default())
}

#[test]
fn a_line_that_counts_in_the_head_is_read_there_whatever_commit_its_comment_names() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let first = fixture.pins(&opened.review);
    push(&mut fixture, files(&[("src/api.rs", "modified")]), |repo| {
        repo.write("src/api.rs", SECOND);
    });
    // GitHub moved the thread's line to where the new head has it (9) and names the commit
    // the comment was made on: line 9 there is `fn gamma() {`.
    let thread = with_hunk(
        on_line("T_beta", "src/api.rs", 9, &first.head),
        beta_hunk(true),
    );
    let mirror = mirror_with(&mut fixture, &opened.review, vec![thread]);
    let now = fixture.pins(&opened.review);
    assert_ne!(now.head, first.head);
    assert_eq!(
        placed(&fixture, &mirror, &now.spec()),
        ["anchored unchanged src/api.rs:9-9"]
    );
    let anchor = &anchors(&fixture, &mirror, &now.spec())[0].anchor;
    assert_eq!(anchor.text, ["    two_and_a_half();"]);
    assert_eq!(anchor.context_before, ["}", "", "fn beta() {"]);
    // A review still at the first head relocates it from there.
    assert_eq!(
        placed(&fixture, &mirror, &first.spec()),
        ["anchored moved src/api.rs:6-6"]
    );
}

#[test]
fn a_line_no_candidate_holds_as_the_hunk_shows_it_has_no_place() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let head = fixture.head();
    let mirror = mirror_with(
        &mut fixture,
        &opened.review,
        vec![
            // An old side counted in another base than the merge base dv knows: line 2 is
            // `one();` in every base here.
            with_hunk(
                with(
                    on_line("T_old", "src/api.rs", 2, &head),
                    json!({"diffSide": "LEFT"}),
                ),
                "@@ -1,3 +1,3 @@\n fn alpha() {\n-    zero();",
            ),
            // A new side whose line is other code in the comment's commit and in the head.
            with_hunk(
                on_line("T_new", "src/api.rs", 2, &head),
                "@@ -1,3 +1,3 @@\n fn alpha() {\n+    uno();",
            ),
            // The same lines, the hunks GitHub cuts for them: the check passes.
            with(
                on_line("T_old_there", "src/api.rs", 2, &head),
                json!({"diffSide": "LEFT"}),
            ),
            on_line("T_new_there", "src/api.rs", 2, &head),
            // A hunk without a row on the thread's side has nothing to check against.
            with_hunk(
                on_line("T_unchecked", "src/api.rs", 2, &head),
                "@@ -1,3 +1,2 @@\n-fn omega() {",
            ),
        ],
    );
    let spec = fixture.pins(&opened.review).spec();
    assert_eq!(
        placed(&fixture, &mirror, &spec),
        [
            "unplaced outdated -",
            "unplaced outdated -",
            "anchored unchanged src/api.rs:2-2",
            "anchored unchanged src/api.rs:2-2",
            "anchored unchanged src/api.rs:2-2",
        ]
    );
    // Without a place, its code is the hunk's.
    let anchors = anchors(&fixture, &mirror, &spec);
    assert_eq!(
        (
            anchors[0].anchor.side,
            anchors[0].anchor.start_line,
            anchors[0].anchor.text.as_slice()
        ),
        (AnchorSide::Old, 2, &["    zero();".to_string()][..])
    );
    assert_eq!(anchors[1].anchor.text, ["    uno();"]);
    assert_eq!(anchors[2].anchor.text, ["    one();"]);
}

#[test]
fn line_endings_and_a_missing_last_newline_are_no_disagreement() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let added = files(&[
        ("src/api.rs", "modified"),
        ("crlf.txt", "added"),
        ("tail.txt", "added"),
        ("logo.bin", "added"),
    ]);
    let head = push(&mut fixture, added, |repo| {
        repo.write_crlf("crlf.txt", "one\ntwo\nthree\n")
            .write("tail.txt", "one\ntwo")
            .write("logo.bin", b"\x89PNG\x00\x01\x02\nmore\x00\n");
    });
    let file = json!({"subjectType": "FILE", "line": null, "originalLine": null});
    let mirror = mirror_with(
        &mut fixture,
        &opened.review,
        vec![
            // Each row of a CRLF file's hunk ends in `\r`, the cut one included.
            with_hunk(
                on_line("T_crlf", "crlf.txt", 2, &head),
                "@@ -0,0 +1,3 @@\n+one\r\n+two\r",
            ),
            // As the fixture GitHub cuts it from git's own patch.
            on_line("T_crlf_cut", "crlf.txt", 3, &head),
            with_hunk(
                on_line("T_tail", "tail.txt", 2, &head),
                "@@ -0,0 +1,2 @@\n+one\n+two\n\\ No newline at end of file",
            ),
            // Not text: no line to read, the file alone.
            on_line("T_binary", "logo.bin", 1, &head),
            with(on_line("T_binary_file", "logo.bin", 1, &head), file),
        ],
    );
    assert!(
        mirror.threads[1].comments[0]
            .diff_hunk
            .ends_with("+three\r"),
        "{:?}",
        mirror.threads[1].comments[0].diff_hunk
    );
    let spec = fixture.pins(&opened.review).spec();
    assert_eq!(
        placed(&fixture, &mirror, &spec),
        [
            "anchored unchanged crlf.txt:2-2",
            "anchored unchanged crlf.txt:3-3",
            "anchored unchanged tail.txt:2-2",
            "unplaced outdated -",
            "anchored file logo.bin",
        ]
    );
}

/// The author's `feat` merged into `main` by `merge` (git's arguments after `merge`) and
/// published: the pull request reads as merged from now on.
fn merge(fixture: &mut PrFixture, merge: &[&str]) {
    let author = fixture.github.author();
    author.checkout("main");
    let mut args = vec!["merge", "-q"];
    args.extend_from_slice(merge);
    args.push("feat");
    author.git(&args);
    fixture.github.publish("main");
    fixture.pr.state = "MERGED".to_string();
    fixture.pr.merge_commit = Some(fixture.main().to_string());
    fixture.answer(Vec::new());
}

#[test]
fn a_merged_pull_requests_old_side_thread_stands_where_it_started() {
    let mut fixture = fixture();
    let before = fixture.main();
    merge(&mut fixture, &["--no-ff", "-m", "Merge pull request #7"]);
    let opened = fixture.open();
    let head = fixture.head();
    let old = with_hunk(
        with(
            on_line("T_old", "src/api.rs", 6, &head),
            json!({"diffSide": "LEFT"}),
        ),
        beta_hunk(false),
    );
    let mirror = mirror_with(&mut fixture, &opened.review, vec![old.clone()]);
    // The merge commit tells where the pull request started.
    assert_eq!(mirror.merge_base_oid, Some(before));
    let spec = fixture.pins(&opened.review).spec();
    assert_eq!(
        placed(&fixture, &mirror, &spec),
        ["anchored unchanged src/api.rs:6-6"]
    );
    assert_eq!(
        anchors(&fixture, &mirror, &spec)[0].anchor.text,
        ["    two();"]
    );

    // Merged by a fast-forward GitHub tells nothing about, the old side dv has is the head
    // itself: its line 6 isn't the hunk's, so the thread has no place rather than that one.
    let mut forwarded = super::fixture();
    merge(&mut forwarded, &["--ff-only"]);
    let opened = forwarded.open();
    let mirror = mirror_with(&mut forwarded, &opened.review, vec![old]);
    assert_eq!(mirror.merge_base_oid, Some(forwarded.head()));
    assert_eq!(
        placed(&forwarded, &mirror, &forwarded.pins(&opened.review).spec()),
        ["unplaced outdated -"]
    );
}

/// PLAN §9.6, "The work": a file's text is read once per commit and path, never per thread;
/// one thread reads its own file alone; and anchoring again after a Resolve reads nothing.
#[test]
fn anchoring_reads_a_file_once_and_nothing_again_until_the_head_moves() {
    const FILES: usize = 6;
    let mut fixture = fixture();
    let opened = fixture.open();
    let paths: Vec<String> = (0..FILES).map(|n| format!("src/f{n}.rs")).collect();
    let listed: Vec<(&str, &str)> = paths.iter().map(|path| (path.as_str(), "added")).collect();
    let head = push(&mut fixture, files(&listed), |repo| {
        for path in &paths {
            repo.write(path, "fn a() {\n    one();\n    two();\n}\n");
        }
    });
    let threads = paths
        .iter()
        .flat_map(|path| {
            [
                on_line(&format!("T_{path}_1"), path, 1, &head),
                on_line(&format!("T_{path}_3"), path, 3, &head),
                with(
                    on_line(&format!("T_{path}_old"), path, 2, &head),
                    json!({"subjectType": "FILE", "line": null, "originalLine": null}),
                ),
            ]
        })
        .collect();
    let mirror = mirror_with(&mut fixture, &opened.review, threads);
    let spec = fixture.pins(&opened.review).spec();

    let clone = fixture.github.checkout();
    let trace = GitTrace::new(&clone.outside_dir("trace"));
    let mut env = clone.env_overlay();
    env.push(trace.env());
    let backend = LocalBackend::open_with(clone.path(), LocalBackendOptions { env }).unwrap();
    let git = GitRepo::new(Arc::new(backend));
    trace.take();

    let texts = ThreadTexts::default();
    let all = threads::anchor_threads(&git, &mirror, &spec, &texts);
    let read = trace.take();
    assert!(
        all.iter()
            .all(|anchored| anchored.standing == Standing::Anchored)
    );
    assert!(!read.is_empty());

    // One line thread a file costs what two and a comment on the file do.
    let mut one_each = mirror.clone();
    one_each.threads.retain(|thread| thread.id.ends_with("_1"));
    threads::anchor_threads(&git, &one_each, &spec, &ThreadTexts::default());
    assert_eq!(trace.take().len(), read.len(), "{read:#?}");

    // One thread reads one file.
    let one = threads::anchor_thread(
        &git,
        &mirror,
        &spec,
        &ThreadTexts::default(),
        &mirror.threads[0],
    );
    assert_eq!(one, all[0]);
    assert_eq!(trace.take().len() * FILES, read.len(), "{read:#?}");

    // A Resolve changes a thread's state and when the mirror was read, no code.
    let mut resolved = mirror.clone();
    resolved.threads[0].resolved = true;
    resolved.read_at += 1;
    assert_eq!(threads::anchor_threads(&git, &resolved, &spec, &texts), all);
    assert_eq!(trace.take(), [""; 0]);

    // Another head or other sides: what was read is read again.
    let mut moved = mirror.clone();
    moved.merge_base_oid = None;
    assert_eq!(threads::anchor_threads(&git, &moved, &spec, &texts), all);
    assert_eq!(trace.take().len(), read.len());
}

/// PLAN §9.6: a last line that two commits share (`}`, a blank) doesn't put the thread in
/// the wrong one. The candidate that agrees with more of the hunk's rows is the anchor.
#[test]
fn a_last_line_two_commits_share_is_told_apart_by_the_rows_above_it() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let first = fixture.pins(&opened.review);
    // Four lines above everything: each block's `}` is where the next one's was.
    let shifted = format!("// One.\n// Two.\n// Three.\n\n{FIRST}");
    push(&mut fixture, files(&[("src/api.rs", "modified")]), |repo| {
        repo.write("src/api.rs", &shifted);
    });
    // GitHub has the thread on `beta`'s `}`, line 11 of the new head, and names the commit
    // the comment was made on, whose line 11 is `gamma`'s `}`.
    let hunk = "@@ -3,7 +3,7 @@ fn alpha() {\n }\n \n fn beta() {\n-    two();\n+    two_and_a_half();\n }";
    let thread = with_hunk(on_line("T_brace", "src/api.rs", 11, &first.head), hunk);
    let mirror = mirror_with(&mut fixture, &opened.review, vec![thread]);
    let now = fixture.pins(&opened.review);
    let anchor = &anchors(&fixture, &mirror, &now.spec())[0].anchor;
    assert_eq!(anchor.text, ["}"]);
    assert_eq!(
        anchor.context_before,
        ["", "fn beta() {", "    two_and_a_half();"]
    );
    assert_eq!(
        placed(&fixture, &mirror, &now.spec()),
        ["anchored unchanged src/api.rs:11-11"]
    );
}

/// PLAN §9.6: bytes that aren't UTF-8 read as U+FFFD here, whatever GitHub shows for them:
/// such a line is held against the hunk by what stands between them.
#[test]
fn a_line_that_isnt_utf8_agrees_by_what_stands_between_its_bytes() {
    let mut fixture = fixture();
    let opened = fixture.open();
    let added = files(&[("src/api.rs", "modified"), ("latin1.txt", "added")]);
    let head = push(&mut fixture, added, |repo| {
        repo.write("latin1.txt", b"one\nun caf\xe9 cr\xe8me, merci\nthree\n");
    });
    let on = |id: &str, row: &str| {
        with_hunk(
            on_line(id, "latin1.txt", 2, &head),
            &format!("@@ -0,0 +1,3 @@\n+one\n+{row}"),
        )
    };
    let mirror = mirror_with(
        &mut fixture,
        &opened.review,
        vec![
            // GitHub transcoding the line, or showing replacement characters as dv does.
            on("T_transcoded", "un café crème, merci"),
            on("T_replaced", "un caf\u{fffd} cr\u{fffd}me, merci"),
            // Other text around the same bytes.
            on("T_other", "un café crème, non"),
            on("T_swapped", "une café crème, merci"),
            on("T_middle", "un café frappé, merci"),
            // A replacement character stands for a character of the row at least.
            on("T_nothing", "un caf crme, merci"),
        ],
    );
    let spec = fixture.pins(&opened.review).spec();
    assert_eq!(
        placed(&fixture, &mirror, &spec),
        [
            "anchored unchanged latin1.txt:2-2",
            "anchored unchanged latin1.txt:2-2",
            "unplaced outdated -",
            "unplaced outdated -",
            "unplaced outdated -",
            "unplaced outdated -",
        ]
    );
}

#[test]
fn a_line_of_replacement_characters_alone_is_no_row() {
    use crate::github::threads::same_line;
    assert!(same_line("\u{fffd}", "é"));
    assert!(same_line("\u{fffd}\u{fffd}", "éè"));
    assert!(same_line("a\u{fffd}\u{fffd}b", "a\u{fffd}\u{fffd}b"));
    assert!(same_line("a\u{fffd}b\u{fffd}c\r", "aébèc"));
    // Nothing of the row for one of them.
    assert!(!same_line("\u{fffd}", ""));
    assert!(!same_line("\u{fffd}\u{fffd}", "é"));
    assert!(!same_line("a\u{fffd}b", "ab"));
    assert!(!same_line("a\u{fffd}b\u{fffd}c", "aébc"));
    // What stands between them is the row's, in order.
    assert!(!same_line("a\u{fffd}b\u{fffd}c", "aécèb"));
    assert!(same_line("plain", "plain\r") && !same_line("plain", "other"));
}

/// PLAN §9.6, "The work": a read that couldn't be made says nothing about the commit, and
/// the next anchoring asks again; a commit the clone doesn't hold is a fact, asked once.
#[test]
fn a_read_that_couldnt_be_made_is_asked_again_and_a_missing_commit_is_not() {
    use crate::backend::BackendError;
    let mut fixture = fixture();
    let opened = fixture.open();
    let head = fixture.head();
    let here = mirror_with(
        &mut fixture,
        &opened.review,
        vec![on_line("T_here", "src/api.rs", 6, &head)],
    );
    // A commit that isn't in the clone, its hunk showing other code than the head's.
    let elsewhere = oid(&"f".repeat(40));
    let gone = mirror_with(
        &mut fixture,
        &opened.review,
        vec![with_hunk(
            on_line("T_gone", "src/api.rs", 6, &elsewhere),
            "@@ -3,4 +3,4 @@\n fn beta() {\n+    two_and_a_quarter();",
        )],
    );
    let spec = fixture.pins(&opened.review).spec();
    let clone = fixture.github.checkout();
    let standing = |git: &GitRepo, mirror: &Mirror, texts: &ThreadTexts| {
        threads::anchor_threads(git, mirror, &spec, texts)[0].standing
    };
    let failures = [
        BackendError::Disconnected {
            location: "Ubuntu".to_string(),
            reason: "the connection closed".to_string(),
        },
        BackendError::Timeout {
            args: vec!["diff".to_string()],
            timeout_ms: 30_000,
        },
        // git itself failing, for another reason than a commit that isn't there: killed,
        // or an object it couldn't read from a disk that failed under it.
        BackendError::Git {
            args: vec!["diff".to_string()],
            code: None,
            stderr: String::new(),
        },
        BackendError::Git {
            args: vec!["diff".to_string()],
            code: Some(128),
            stderr: "error: unable to read sha1 file of src/api.rs (2f9d…)\nfatal: unable to \
                     read 2f9d…: Input/output error\n"
                .to_string(),
        },
        BackendError::Git {
            args: vec!["diff".to_string()],
            code: Some(1),
            stderr: "fatal: bad object in a message of another exit\n".to_string(),
        },
    ];
    for failure in failures {
        let once = contending::FetchFails::failing_at(
            clone,
            |args| args.first() == Some(&"diff"),
            vec![failure],
        );
        let git = GitRepo::new(once);
        let texts = ThreadTexts::default();
        assert_eq!(standing(&git, &here, &texts), Standing::Unplaced);
        // The refresh after it: the same head, the same sides, the same texts.
        assert_eq!(standing(&git, &here, &texts), Standing::Anchored);
    }

    // The commit that isn't in the clone: git says so once.
    let trace = GitTrace::new(&clone.outside_dir("trace"));
    let mut env = clone.env_overlay();
    env.push(trace.env());
    let backend = LocalBackend::open_with(clone.path(), LocalBackendOptions { env }).unwrap();
    let git = GitRepo::new(Arc::new(backend));
    trace.take();
    let texts = ThreadTexts::default();
    assert_eq!(standing(&git, &gone, &texts), Standing::Unplaced);
    assert!(!trace.take().is_empty());
    assert_eq!(standing(&git, &gone, &texts), Standing::Unplaced);
    assert_eq!(trace.take(), [""; 0]);
}
