//! What the fetch brings and leaves alone, and where a pull request's changes start (PLAN
//! §9.3 steps 2–3): submodules, a retargeted pull request, a base branch that is gone,
//! merged pull requests, histories with nothing in common.

use std::time::{Duration, Instant};

use dv_test_support::FixtureRepo;
use serde_json::json;

use super::*;
use crate::diff::file_list;
use crate::review::ReviewStatus;

/// A second pull request of the fixture's repository: `number`, whose head is the author's
/// `rev`, into `base`, which `gh` answers about from now on beside pull request 7.
fn another(fixture: &PrFixture, number: u64, rev: &str, base: &str) -> GhPr {
    let head = fixture.github.open_pr(number, rev);
    let mut pr = GhPr::new(number, &head);
    pr.base_branch = base.to_string();
    answer(fixture, &pr);
    pr
}

/// Has `gh` answer about `pr` (and the viewer) alone.
fn answer(fixture: &PrFixture, pr: &GhPr) {
    let mut calls = vec![fixture_gh::viewer(VIEWER)];
    calls.extend(pr.calls(fixture.github.name()));
    fixture.mock.answer(Value::Array(calls));
}

/// The paths the review's two sides differ in.
fn changed(fixture: &PrFixture, review: &Review) -> Vec<String> {
    let git = fixture.git();
    let spec = pins::resolve(&git, &review.id, &review.target).unwrap();
    file_list(&git, &spec)
        .unwrap()
        .into_iter()
        .map(|entry| entry.path)
        .collect()
}

#[test]
fn the_fetch_stays_out_of_submodules() {
    let fixture = PrFixture::new();
    let (author, clone) = (fixture.github.author(), fixture.github.checkout());
    // A library the repository holds as a submodule, checked out in the clone.
    let library = FixtureRepo::new();
    library.write("lib.rs", "pub fn one() {}\n");
    library.commit("One");
    author.checkout("main");
    author.git(&[
        "submodule".as_ref(),
        "add".as_ref(),
        "-q".as_ref(),
        library.path().as_os_str(),
        "lib".as_ref(),
    ]);
    author.commit("Add the library");
    fixture.github.publish("main");
    clone.git(&["pull", "-q", "--ff-only", "origin", "main"]);
    clone.git(&["submodule", "update", "--init", "-q"]);

    // The library moves on, and a pull request moves the repository's pointer to it.
    library.write("lib.rs", "pub fn one() {}\npub fn two() {}\n");
    let next = library.commit("Two");
    author.branch("bump").checkout("bump");
    author.git(&["-C", "lib", "pull", "-q", "origin", "main"]);
    author.commit("Bump the library");
    another(&fixture, 8, "bump", "main");

    let submodule = || {
        (
            clone.git(&["-C", "lib", "for-each-ref"]),
            clone
                .git_command()
                .args(["-C", "lib", "cat-file", "-e", &next])
                .output()
                .unwrap()
                .status
                .success(),
        )
    };
    let before = submodule();
    assert!(
        !before.1,
        "the clone's submodule doesn't have the new commit"
    );
    let opened = fixture.open_ref(&PrRef::Hash(8)).unwrap();
    assert_eq!(changed(&fixture, &opened.review), ["lib"]);
    // Nothing in the submodule was fetched: its refs stand, its new commit isn't there.
    assert_eq!(submodule(), before);
    fixture.refresh(&opened.review);
    assert_eq!(submodule(), before);
}

#[test]
fn a_retargeted_pull_request_follows_its_new_base() {
    // `upper` is stacked on `lower`; its pull request first targets `main`.
    let fixture = PrFixture::new();
    let author = fixture.github.author();
    author.checkout("main").branch("lower").checkout("lower");
    author.write("lower.rs", "pub fn lower() {}\n");
    let lower = oid(&author.commit("Lower"));
    author.branch("upper").checkout("upper");
    author.write("upper.rs", "pub fn upper() {}\n");
    author.commit("Upper");
    fixture.github.publish("lower");
    let mut pr = another(&fixture, 12, "upper", "main");

    let opened = fixture.open_ref(&PrRef::Hash(12)).unwrap();
    let before = fixture.pins(&opened.review);
    assert_eq!(before.base, fixture.main());
    assert_eq!(changed(&fixture, &opened.review), ["lower.rs", "upper.rs"]);

    // Retargeted to `lower` on GitHub: the head is the one it was, the old side isn't.
    pr.base_branch = "lower".to_string();
    answer(&fixture, &pr);
    let refreshed = fixture.refresh(&opened.review);
    assert!(refreshed.stale.is_none(), "{:?}", refreshed.stale);
    let pins = fixture.pins(&opened.review);
    assert_eq!(
        pins,
        Pins {
            head: before.head.clone(),
            base: lower.clone()
        }
    );
    assert_eq!(refreshed.pins, Some(pins));
    assert_eq!(refreshed.moved_from, Some(before));
    assert_eq!(refreshed.mirror.unwrap().pr.base_ref, "lower");
    assert_eq!(changed(&fixture, &opened.review), ["upper.rs"]);
    assert!(fixture.refs("refs/dv/pr/dv-fixture/api/12/").contains(&(
        "refs/dv/pr/dv-fixture/api/12/base".to_string(),
        lower.to_string()
    )));
}

/// The fixture with pull request 9: `top`, one commit on `stack`, itself one commit on
/// `main`, into `stack`, whose commit `gh` reports as `baseRefOid`. Returns `stack`'s commit.
fn stacked(fixture: &PrFixture) -> (GhPr, Oid) {
    let author = fixture.github.author();
    author.checkout("main").branch("stack").checkout("stack");
    author.write("stack.rs", "pub fn stack() {}\n");
    let stack = oid(&author.commit("Stack"));
    author.branch("top").checkout("top");
    author.write("top.rs", "pub fn top() {}\n");
    author.commit("Top");
    fixture.github.publish("stack");
    let mut pr = another(fixture, 9, "top", "stack");
    pr.base_oid = Some(stack.to_string());
    answer(fixture, &pr);
    (pr, stack)
}

/// Deletes the fixture GitHub's branch `stack`, and says whether it hands out a commit asked
/// for by its id, as GitHub does.
fn delete_stack(fixture: &PrFixture, serves_ids: bool) {
    let author = fixture.github.author();
    author.git(&[
        "push".as_ref(),
        "-q".as_ref(),
        fixture.github.bare().as_os_str(),
        ":refs/heads/stack".as_ref(),
    ]);
    let bare = fixture.github.bare().to_string_lossy().into_owned();
    for key in [
        "uploadpack.allowAnySHA1InWant",
        "uploadpack.allowReachableSHA1InWant",
    ] {
        author.git(&[
            "--git-dir",
            &bare,
            "config",
            key,
            if serves_ids { "true" } else { "false" },
        ]);
    }
}

#[test]
fn a_deleted_base_branch_is_fetched_by_its_commit() {
    let fixture = PrFixture::new();
    let (_, stack) = stacked(&fixture);
    delete_stack(&fixture, true);
    let opened = fixture.open_ref(&PrRef::Hash(9)).unwrap();
    assert!(opened.created);
    assert_eq!(fixture.pins(&opened.review).base, stack);
    assert_eq!(changed(&fixture, &opened.review), ["top.rs"]);
    assert!(opened.refreshed.stale.is_none(), "{:?}", opened.refreshed);
    assert!(fixture.refs("refs/dv/pr/dv-fixture/api/9/").contains(&(
        "refs/dv/pr/dv-fixture/api/9/base".to_string(),
        stack.to_string()
    )));
    // And again, the base ref an earlier fetch left standing in for one GitHub won't serve.
    let mut pr = fixture.pr.clone();
    pr.number = 9;
    pr.head = fixture.pins(&opened.review).head.to_string();
    pr.base_branch = "stack".to_string();
    pr.base_oid = None;
    answer(&fixture, &pr);
    let refreshed = fixture.refresh(&opened.review);
    assert!(refreshed.stale.is_none(), "{:?}", refreshed.stale);
    assert_eq!(fixture.pins(&opened.review).base, stack);
}

#[test]
fn a_deleted_base_branch_nobody_serves_fails_by_its_name() {
    let fixture = PrFixture::new();
    let (mut pr, _) = stacked(&fixture);
    delete_stack(&fixture, false);
    // GitHub's record of the commit is one this server doesn't have.
    pr.base_oid = Some("f".repeat(40));
    answer(&fixture, &pr);
    let error = fixture.open_ref(&PrRef::Hash(9)).unwrap_err();
    assert!(
        matches!(&error, GithubError::BaseGone { number: 9, branch } if branch == "stack"),
        "{error:?}"
    );
    assert_eq!(error.kind(), "base_gone");
    assert_eq!(
        error.to_string(),
        "#9's base branch stack was deleted on GitHub and its last commit can't be fetched, \
         so there is nothing to compare the pull request with"
    );
    assert!(fixture.store().list().unwrap().is_empty());
    // An open that fails makes nothing: what it fetched of the pull request went again.
    assert_eq!(fixture.refs("refs/dv/"), []);
    // With no record of it at all, the same.
    pr.base_oid = None;
    answer(&fixture, &pr);
    let error = fixture.open_ref(&PrRef::Hash(9)).unwrap_err();
    assert!(matches!(error, GithubError::BaseGone { .. }), "{error:?}");
}

#[test]
fn a_deleted_base_branch_whose_commit_is_here_already_needs_no_server() {
    let fixture = PrFixture::new();
    let (_, stack) = stacked(&fixture);
    // The clone fetched the branch while it lived.
    fixture
        .github
        .checkout()
        .git(&["fetch", "-q", "origin", "stack"]);
    delete_stack(&fixture, false);
    let opened = fixture.open_ref(&PrRef::Hash(9)).unwrap();
    assert_eq!(fixture.pins(&opened.review).base, stack);
    assert_eq!(changed(&fixture, &opened.review), ["top.rs"]);
    assert!(fixture.refs("refs/dv/pr/dv-fixture/api/9/").contains(&(
        "refs/dv/pr/dv-fixture/api/9/base".to_string(),
        stack.to_string()
    )));
}

#[test]
fn a_base_branch_gh_names_must_be_a_branchs_name() {
    use crate::github::pr::is_branch_name;
    for name in [
        "main",
        "release/1.2",
        "feat/x.y",
        "a@b",
        "+plus",
        "ünï",
        "x.locked",
    ] {
        assert!(is_branch_name(name), "{name:?}");
    }
    for name in [
        "",
        "@",
        "-x",
        "--upload-pack=touch /tmp/x",
        "main:refs/heads/evil",
        "a b",
        "a..b",
        "a^",
        "a~1",
        "a?",
        "a*",
        "a[b",
        "a\\b",
        "a\tb",
        "a\u{7f}",
        "a/",
        "/a",
        "a//b",
        "a.",
        "a.lock",
        "a.lock/b",
        ".a",
        "a/.b",
        "a@{1}",
    ] {
        assert!(!is_branch_name(name), "{name:?}");
    }

    // One that isn't is gh's answer gone wrong, and nothing is fetched by it.
    let fixture = PrFixture::new();
    let mut pr = fixture.pr.clone();
    pr.base_branch = "main:refs/heads/evil".to_string();
    answer(&fixture, &pr);
    let error = fixture.open_ref(&PrRef::Hash(NUMBER)).unwrap_err();
    assert!(
        matches!(&error, GithubError::UnexpectedOutput { command, .. } if command == "pr view"),
        "{error:?}"
    );
    assert_eq!(error.kind(), "unexpected_output");
    assert_eq!(fixture.refs("refs/dv/"), []);
}

/// An answer about another pull request than the one asked for is no answer: nothing is
/// kept under the number asked for.
#[test]
fn an_answer_about_another_pull_request_is_refused() {
    let fixture = PrFixture::new();
    let mut view = fixture.pr.view("api");
    view["number"] = json!(NUMBER + 1);
    fixture.answer(vec![json!({
        "argv": ["pr", "view", "7", "--json", crate::github::pr::VIEW_FIELDS, "**"],
        "stdout": view,
    })]);
    let error = fixture.open_ref(&PrRef::Hash(NUMBER)).unwrap_err();
    assert!(
        matches!(&error, GithubError::UnexpectedOutput { command, .. } if command == "pr view"),
        "{error:?}"
    );
    assert!(
        error.to_string().contains("pull request 8, asked for 7"),
        "{error}"
    );
    assert_eq!(fixture.refs("refs/dv/"), []);
    assert_eq!(fixture.store().list().unwrap().len(), 0);
}

/// The author's `feat` merged into `main` by `merge` (git's arguments after `merge`), the
/// result published; the fixture's pull request then reads as merged at `main`'s new tip.
/// Returns `main` as it was before, and after.
fn merged(fixture: &mut PrFixture, merge: &[&str]) -> (Oid, Oid) {
    let author = fixture.github.author();
    let before = fixture.main();
    author.checkout("main");
    let mut args = vec!["merge", "-q"];
    args.extend_from_slice(merge);
    args.push("feat");
    author.git(&args);
    if merge.contains(&"--squash") {
        author.commit("Add the answer (#7)");
    }
    fixture.github.publish("main");
    let after = fixture.main();
    fixture.pr.state = "MERGED".to_string();
    fixture.pr.merge_commit = Some(after.to_string());
    fixture.answer(Vec::new());
    (before, after)
}

#[test]
fn a_pull_request_merged_by_a_merge_commit_shows_what_it_merged() {
    let mut fixture = PrFixture::new();
    let (before, after) = merged(&mut fixture, &["--no-ff", "-m", "Merge pull request #7"]);
    // GitHub's record of the base is the merged branch, whichever it means by it.
    fixture.pr.base_oid = Some(after.to_string());
    fixture.answer(Vec::new());
    let opened = fixture.open();
    assert_eq!(
        fixture.pins(&opened.review),
        Pins {
            head: fixture.head(),
            base: before
        }
    );
    assert_eq!(changed(&fixture, &opened.review), ["src/lib.rs"]);
    assert_eq!(opened.review.status, ReviewStatus::Draft);
    assert_eq!(opened.refreshed.mirror.unwrap().pr.state, PrState::Merged);
}

#[test]
fn a_squash_merged_pull_request_resolves_as_an_open_one() {
    // Its head never enters the base branch: three dots, as ever.
    let mut fixture = PrFixture::new();
    let (before, _) = merged(&mut fixture, &["--squash"]);
    let opened = fixture.open();
    assert_eq!(fixture.pins(&opened.review).base, before);
    assert_eq!(changed(&fixture, &opened.review), ["src/lib.rs"]);
}

#[test]
fn a_fast_forwarded_pull_request_starts_where_github_says_its_base_was() {
    let mut fixture = PrFixture::new();
    let (before, after) = merged(&mut fixture, &["--ff-only"]);
    assert_eq!(after, fixture.head(), "the base branch is the head now");
    // No merge commit to read: the base before the merge is GitHub's `baseRefOid`.
    fixture.pr.base_oid = Some(before.to_string());
    fixture.answer(Vec::new());
    let opened = fixture.open();
    assert_eq!(fixture.pins(&opened.review).base, before);
    assert_eq!(changed(&fixture, &opened.review), ["src/lib.rs"]);

    // A record that holds the head, or none, tells nothing: the old side is the head, an
    // empty diff, never a guess at a parent.
    let mut other = PrFixture::new();
    let (_, after) = merged(&mut other, &["--ff-only"]);
    for base in [Some(after.to_string()), None] {
        other.pr.base_oid = base;
        other.answer(Vec::new());
        let opened = other.open();
        assert_eq!(other.pins(&opened.review).base, other.head());
        assert_eq!(changed(&other, &opened.review), Vec::<String>::new());
    }
}

#[test]
fn an_open_pull_request_whose_head_is_in_its_base_has_nothing_to_show() {
    // Not merged as far as GitHub says: its rule alone, whatever else gh reports.
    let mut fixture = PrFixture::new();
    let (before, after) = merged(&mut fixture, &["--no-ff", "-m", "Merge by hand"]);
    fixture.pr.state = "OPEN".to_string();
    fixture.pr.base_oid = Some(before.to_string());
    fixture.pr.merge_commit = Some(after.to_string());
    fixture.answer(Vec::new());
    let opened = fixture.open();
    assert_eq!(fixture.pins(&opened.review).base, fixture.head());
}

#[test]
fn histories_with_nothing_in_common_fail_the_open() {
    let fixture = PrFixture::new();
    let author = fixture.github.author();
    author.git(&["checkout", "-q", "--orphan", "lone"]);
    author.write("lone.rs", "pub fn lone() {}\n");
    author.commit("Alone");
    another(&fixture, 13, "lone", "main");
    let error = fixture.open_ref(&PrRef::Hash(13)).unwrap_err();
    assert!(
        matches!(
            error,
            GithubError::Target(crate::TargetError::NoMergeBase { .. })
        ),
        "{error:?}"
    );
    assert_eq!(error.to_string(), "main and #13 have no common history");
    assert_eq!(error.kind(), "git");
    assert!(fixture.store().list().unwrap().is_empty());
    assert_eq!(fixture.refs("refs/dv/"), []);
}

#[test]
fn a_head_gh_reports_before_git_has_it_is_read_again_after_a_pause() {
    let mut fixture = PrFixture::new();
    fixture.mock = MockGh::with(Value::Array(Vec::new()), |options| {
        options.settle = Duration::from_millis(400)
    });
    let old = fixture.head();
    let head = fixture.push("pub fn answer() -> u32 {\n    43\n}\n");
    // gh's first answer is the head before the push; git already fetches the new one.
    let mut stale = fixture.pr.clone();
    stale.head = old.to_string();
    fixture.answer(vec![json!({
        "argv": ["pr", "view", "7", "--json", crate::github::pr::VIEW_FIELDS, "**"],
        "stdout": stale.view("api"),
        "times": 1
    })]);
    let started = Instant::now();
    let opened = fixture.open();
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(fixture.pins(&opened.review).head, head);
    // The real pause is long enough for GitHub's refs to catch up with its API.
    assert_eq!(
        GhOptions::new("gh".into()).settle,
        Duration::from_millis(1500)
    );
}

#[test]
fn a_bot_and_a_deleted_account_are_read_as_gh_writes_them() {
    let mut fixture = PrFixture::new();
    let author_of = |fixture: &PrFixture| fixture.open().refreshed.mirror.unwrap().pr.author;
    // gh: `{"is_bot": true, "login": "app/dependabot"}`; GraphQL names the same bot
    // `dependabot` on its comments.
    fixture.pr.author = Some("dependabot".to_string());
    fixture.pr.bot = true;
    fixture.answer(Vec::new());
    assert_eq!(
        fixture.pr.view("api")["author"],
        json!({"is_bot": true, "login": "app/dependabot"})
    );
    assert_eq!(author_of(&fixture), "dependabot");
    // What is left of a deleted account, a bot's or a person's.
    for bot in [true, false] {
        fixture.pr.author = None;
        fixture.pr.bot = bot;
        fixture.answer(Vec::new());
        assert_eq!(author_of(&fixture), "ghost", "bot: {bot}");
    }
    // A person's login is taken as it is.
    fixture.pr.author = Some("octocat".to_string());
    fixture.pr.bot = false;
    fixture.answer(Vec::new());
    assert_eq!(
        fixture.pr.view("api")["author"],
        json!({"id": "U_1", "is_bot": false, "login": "octocat", "name": ""})
    );
    assert_eq!(author_of(&fixture), "octocat");
    // No author at all, as GraphQL's null would come through.
    fixture.answer(vec![{
        let mut view = fixture.pr.view("api");
        view["author"] = Value::Null;
        json!({
            "argv": ["pr", "view", "7", "--json", crate::github::pr::VIEW_FIELDS, "**"],
            "stdout": view
        })
    }]);
    assert_eq!(author_of(&fixture), "ghost");
}

#[test]
fn a_remote_on_a_real_host_is_never_reached_from_a_fixture() {
    let fixture = PrFixture::new();
    let clone = fixture.github.checkout();
    // The clone's `origin` without the rewrite that points it at the fixture GitHub: a real
    // `https://github.com/…` URL, as a scratch clone made by hand has.
    let rewrites = clone.git(&["config", "--get-regexp", r"^url\..*\.insteadof$"]);
    for line in rewrites.lines() {
        let key = line.split(' ').next().unwrap();
        clone.git(&["config", "--unset-all", key]);
    }
    assert_eq!(
        clone.git(&["remote", "get-url", "origin"]).trim(),
        "https://github.com/dv-fixture/api.git"
    );
    // The fixture's git takes no transport but a local path (`GIT_ALLOW_PROTOCOL=file`), so
    // the fetch is refused before anything connects.
    let started = Instant::now();
    let error = fixture.open_ref(&PrRef::Hash(NUMBER)).unwrap_err();
    assert_eq!(error.kind(), "git");
    assert_eq!(error.git_said(), Some("transport 'https' not allowed"));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(fixture.refs("refs/dv/"), []);
}
