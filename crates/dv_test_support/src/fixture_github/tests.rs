use super::*;

/// A branch `feat` in the author's repository with one commit on top of `main`, returning it.
fn feature_commit(github: &FixtureGithub) -> String {
    let author = github.author();
    author
        .branch("feat")
        .checkout("feat")
        .write("src/lib.rs", "pub fn answer() -> u32 {\n    42\n}\n");
    let oid = author.commit("Add the answer");
    author.checkout("main");
    oid
}

#[test]
fn the_clone_sees_a_github_origin_and_starts_like_a_fresh_clone() {
    let github = FixtureGithub::new("api");
    let clone = github.checkout();
    assert_eq!(github.url(), "https://github.com/dv-fixture/api.git");
    assert_eq!(
        clone.git(&["config", "remote.origin.url"]).trim(),
        github.url()
    );
    assert_eq!(
        clone
            .git(&["symbolic-ref", "refs/remotes/origin/HEAD"])
            .trim(),
        "refs/remotes/origin/main"
    );
    assert_eq!(
        clone.git(&["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    assert_eq!(
        clone.git(&["rev-parse", "HEAD"]),
        github.author().git(&["rev-parse", "main"])
    );
    assert_eq!(
        clone
            .git(&["rev-parse", "--abbrev-ref", "main@{upstream}"])
            .trim(),
        "origin/main"
    );
    assert_eq!(
        std::fs::read_to_string(clone.path().join("README.md")).unwrap(),
        "# api\n"
    );
}

#[test]
fn commit_ids_are_the_same_on_every_machine() {
    let github = FixtureGithub::new("api");
    assert_eq!(
        github.checkout().git(&["rev-parse", "HEAD"]).trim(),
        "15ced05b6eeefb27e5092d5b9fe0876cf642a38e"
    );
    assert_eq!(
        feature_commit(&github),
        "5746f3cb7598128a528c413ffe03e23666d1e9d6"
    );
}

#[test]
fn a_pull_request_reaches_the_clone_only_through_a_fetch_of_its_ref() {
    let github = FixtureGithub::new("api");
    let head = feature_commit(&github);
    assert_eq!(github.open_pr(7, "feat"), head);
    let clone = github.checkout();

    // GitHub has the pull request's ref and no branch for it.
    let refs = clone.git(&["ls-remote", "origin"]);
    assert!(
        refs.contains(&format!("{head}\trefs/pull/7/head")),
        "{refs}"
    );
    assert!(!refs.contains("refs/heads/feat"), "{refs}");

    // An ordinary fetch leaves the commit out; fetching the ref brings it.
    clone.git(&["fetch", "-q", "origin"]);
    let has_head = || {
        clone
            .git_command()
            .args(["cat-file", "-e", &format!("{head}^{{commit}}")])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(!has_head());
    clone.git(&[
        "fetch",
        "-q",
        "--no-tags",
        "origin",
        "+refs/pull/7/head:refs/dv/pr/dv-fixture/api/7/head",
    ]);
    assert!(has_head());
    assert_eq!(
        clone
            .git(&["rev-parse", "refs/dv/pr/dv-fixture/api/7/head"])
            .trim(),
        head
    );
}

#[test]
fn a_pull_request_and_its_base_branch_move() {
    let github = FixtureGithub::new("api");
    let first = feature_commit(&github);
    github.open_pr(7, "feat");
    let author = github.author();

    // A push to the pull request, then a force-push that drops it.
    author
        .checkout("feat")
        .write("src/lib.rs", "pub fn answer() -> u32 {\n    43\n}\n");
    let second = author.commit("Correct the answer");
    assert_eq!(github.open_pr(7, "feat"), second);
    assert_eq!(github.open_pr(7, &first), first);

    // The base branch moves on.
    author.checkout("main").write("CHANGELOG.md", "- answers\n");
    let base = author.commit("Start a changelog");
    github.publish("main");

    let refs = github.checkout().git(&["ls-remote", "origin"]);
    assert!(
        refs.contains(&format!("{first}\trefs/pull/7/head")),
        "{refs}"
    );
    assert!(refs.contains(&format!("{base}\trefs/heads/main")), "{refs}");
}

#[test]
fn file_urls_are_what_git_fetches_from() {
    assert_eq!(
        file_url(Path::new("/tmp/dv fixture/100%/github.git")),
        "file:///tmp/dv%20fixture/100%25/github.git"
    );
    assert_eq!(
        file_url(Path::new(r"C:\Users\dv\github.git")),
        "file:///C:/Users/dv/github.git"
    );
}

#[test]
fn a_persistent_fixture_is_kept_and_its_github_side_found_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repos").join("api");
    let head = {
        let github = FixtureGithub::persistent(&path);
        assert_eq!(github.name(), "api");
        let head = feature_commit(&github);
        github.open_pr(7, "feat");
        head
    };
    // The same script, the same commits, wherever the fixture is built.
    assert_eq!(head, "5746f3cb7598128a528c413ffe03e23666d1e9d6");

    // A later process finds both sides again and goes on from where they stand.
    let clone = FixtureRepo::reopen(&path);
    let origin = GithubOrigin::reopen(&path).unwrap();
    assert_eq!(origin.name(), "api");
    let refs = clone.git(&["ls-remote", "origin"]);
    assert!(
        refs.contains(&format!("{head}\trefs/pull/7/head")),
        "{refs}"
    );
    origin
        .author()
        .checkout("feat")
        .write("src/lib.rs", "pub fn answer() -> u32 {\n    43\n}\n");
    let next = origin.author().commit("Change the answer");
    assert_eq!(origin.open_pr(7, "feat"), next);
    clone.git(&["fetch", "-q", "origin", "+refs/pull/7/head:refs/dv/pr/7"]);
    assert_eq!(clone.git(&["rev-parse", "refs/dv/pr/7"]).trim(), next);

    // The parts come apart for whoever holds them separately.
    let (clone, origin) =
        FixtureGithub::persistent(dir.path().join("repos").join("web")).into_parts();
    assert_eq!(origin.name(), "web");
    assert!(clone.path().ends_with("repos/web"));
    // A repository seeded without a GitHub side has none to find.
    let plain = FixtureRepo::persistent(dir.path().join("repos").join("plain"));
    assert!(GithubOrigin::reopen(plain.path()).is_none());
}
