use dv_test_support::FixtureRepo;

use super::*;
use crate::testing::git_repo;

#[test]
fn head_is_none_on_an_unborn_branch() {
    let repo = FixtureRepo::new();
    assert_eq!(git_repo(&repo).head().unwrap(), None);
    repo.write("a.txt", "a\n");
    let first = repo.commit("first");
    assert_eq!(
        git_repo(&repo).head().unwrap().map(String::from),
        Some(first)
    );
}

#[test]
fn resolve_peels_revisions_to_commits() {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    let first = repo.commit("first");
    repo.write("a.txt", "b\n");
    let second = repo.commit("second");
    repo.git(&["tag", "-a", "-m", "annotated", "v1", &first]);
    let git = git_repo(&repo);

    for (rev, expected) in [
        ("HEAD", &second),
        ("main", &second),
        ("HEAD~1", &first),
        ("v1", &first),
        (&first[..10], &first),
        (second.as_str(), &second),
    ] {
        assert_eq!(git.resolve(rev).unwrap().as_str(), expected, "{rev}");
    }
}

#[test]
fn resolve_rejects_what_names_no_commit() {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("first");
    repo.git(&["tag", "tree-tag", "HEAD^{tree}"]);
    let git = git_repo(&repo);
    for rev in [
        "no-such-branch",
        "tree-tag",
        "HEAD~5",
        "",
        "--output=written.txt",
        "-n1",
    ] {
        let err = git.resolve(rev).unwrap_err();
        assert!(
            matches!(&err, GitError::UnknownRevision { rev: r } if r == rev),
            "{rev}: {err:?}"
        );
    }
    assert!(
        !repo.path().join("written.txt").exists(),
        "a revision is never read as an option"
    );
}

#[test]
fn merge_base_of_diverged_and_unrelated_histories() {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    let base = repo.commit("base");
    repo.branch("feat").checkout("feat").write("f.txt", "f\n");
    let feat = repo.commit("feature");
    repo.checkout("main").write("m.txt", "m\n");
    repo.commit("main moves on");
    repo.git(&["checkout", "-q", "--orphan", "lonely"]);
    repo.git(&["rm", "-rqf", "."]);
    repo.write("z.txt", "z\n");
    repo.commit("unrelated root");
    let git = git_repo(&repo);

    assert_eq!(
        git.merge_base("main", "feat").unwrap().map(String::from),
        Some(base.clone())
    );
    assert_eq!(
        git.merge_base(&feat, "feat").unwrap().map(String::from),
        Some(feat)
    );
    assert_eq!(git.merge_base("main", "lonely").unwrap(), None);
    let err = git.merge_base("main", "no-such-branch").unwrap_err();
    assert!(
        matches!(&err, GitError::UnknownRevision { rev } if rev == "no-such-branch"),
        "{err:?}"
    );
}

#[test]
fn current_branch_on_a_branch_detached_and_unborn() {
    let repo = FixtureRepo::new();
    assert_eq!(
        git_repo(&repo).current_branch().unwrap().as_deref(),
        Some("main")
    );
    repo.write("a.txt", "a\n");
    let first = repo.commit("first");
    repo.branch("feat/x").checkout("feat/x");
    assert_eq!(
        git_repo(&repo).current_branch().unwrap().as_deref(),
        Some("feat/x")
    );
    repo.checkout(&first);
    assert_eq!(git_repo(&repo).current_branch().unwrap(), None);
}

#[test]
fn empty_tree_follows_the_object_format() {
    for (repo, format, expected) in [
        (FixtureRepo::new(), ObjectFormat::Sha1, EMPTY_TREE_SHA1),
        (
            FixtureRepo::new_sha256(),
            ObjectFormat::Sha256,
            EMPTY_TREE_SHA256,
        ),
    ] {
        let git = git_repo(&repo);
        assert_eq!(git.object_format(), format);
        assert_eq!(git.empty_tree().as_str(), expected);
        assert_eq!(git.empty_tree().format(), format);
        // git's own empty tree in this repo: `mktree` with no entries (the fixture's stdin is
        // null).
        assert_eq!(repo.git(&["mktree"]).trim(), expected);
    }
}

#[test]
fn oids_are_full_lowercase_hex() {
    let sha1 = "0123456789abcdef0123456789abcdef01234567";
    let sha256 = EMPTY_TREE_SHA256;
    assert_eq!(Oid::parse(sha1).unwrap().format(), ObjectFormat::Sha1);
    assert_eq!(Oid::parse(sha256).unwrap().format(), ObjectFormat::Sha256);
    for bad in [
        "",
        "0123456",
        "0123456789ABCDEF0123456789ABCDEF01234567",
        "0123456789abcdef0123456789abcdef0123456g",
        "0123456789abcdef0123456789abcdef012345678",
    ] {
        assert_eq!(Oid::parse(bad), None, "{bad}");
        assert!(bad.parse::<Oid>().is_err(), "{bad}");
    }

    let oid = Oid::parse(sha1).unwrap();
    let json = serde_json::to_string(&oid).unwrap();
    assert_eq!(json, format!("\"{sha1}\""));
    assert_eq!(serde_json::from_str::<Oid>(&json).unwrap(), oid);
    assert!(serde_json::from_str::<Oid>("\"HEAD\"").is_err());
    assert_eq!(
        serde_json::to_string(&ObjectFormat::Sha256).unwrap(),
        "\"sha256\""
    );
}

#[test]
fn parents_of_root_ordinary_and_merge_commits() {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    let root = repo.commit("root");
    repo.branch("side").checkout("side").write("s.txt", "s\n");
    let side = repo.commit("side");
    repo.checkout("main").write("m.txt", "m\n");
    let main = repo.commit("main");
    repo.git(&["merge", "--no-ff", "-q", "-m", "merge", "side"]);
    let merge = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    let git = git_repo(&repo);
    let oid = |text: &str| Oid::parse(text).unwrap();

    assert_eq!(git.parents(&oid(&root)).unwrap(), []);
    assert_eq!(git.parents(&oid(&main)).unwrap(), [oid(&root)]);
    assert_eq!(git.parents(&oid(&merge)).unwrap(), [oid(&main), oid(&side)]);
    let missing = oid("0123456789abcdef0123456789abcdef01234567");
    assert!(git.parents(&missing).is_err());
}

#[test]
fn commits_apart_counts_each_side_of_two_commits() {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    let root = repo.commit("root");
    repo.write("a.txt", "b\n");
    repo.commit("second");
    repo.write("a.txt", "c\n");
    let third = repo.commit("third");
    repo.branch("side")
        .checkout("side")
        .git(&["reset", "-q", "--hard", &root]);
    repo.write("s.txt", "s\n");
    let side = repo.commit("side");
    let git = git_repo(&repo);
    let oid = |text: &str| Oid::parse(text).unwrap();

    assert_eq!(git.commits_apart(&oid(&root), &oid(&root)).unwrap(), (0, 0));
    // A fast-forward: two new commits.
    assert_eq!(
        git.commits_apart(&oid(&root), &oid(&third)).unwrap(),
        (0, 2)
    );
    // History rewritten: `third`'s two commits are gone from `side`, which has one of its own.
    assert_eq!(
        git.commits_apart(&oid(&third), &oid(&side)).unwrap(),
        (2, 1)
    );
    let missing = oid("0123456789abcdef0123456789abcdef01234567");
    assert!(git.commits_apart(&missing, &oid(&root)).is_err());
}

#[test]
fn full_ref_name_follows_gits_ref_rules() {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    let first = repo.commit("first");
    repo.branch("feat");
    repo.git(&["tag", "v1"]);
    let git = git_repo(&repo);
    let full = |name: &str| git.full_ref_name(name).unwrap();

    for name in ["feat", "heads/feat", "refs/heads/feat"] {
        assert_eq!(full(name).as_deref(), Some("refs/heads/feat"), "{name}");
    }
    assert_eq!(full("v1").as_deref(), Some("refs/tags/v1"));
    assert_eq!(full("HEAD").as_deref(), Some("refs/heads/main"));
    // Revisions that aren't refs.
    assert_eq!(full("HEAD~0"), None);
    assert_eq!(full(&first), None);
    for missing in ["nope", ""] {
        assert!(
            matches!(
                git.full_ref_name(missing),
                Err(GitError::UnknownRevision { .. })
            ),
            "{missing:?}"
        );
    }
    // A tag named like a branch: the branch, as `git switch` picks it, where git's revision
    // rules would pick the tag.
    repo.git(&["tag", "feat"]);
    assert_eq!(full("feat").as_deref(), Some("refs/heads/feat"));
    assert_eq!(full("tags/feat").as_deref(), Some("refs/tags/feat"));
}

#[test]
fn the_fixture_overlay_drops_every_variable_the_policy_drops() {
    // Tests compare dv's output with the fixture's own git calls, so those must drop the same
    // inherited variables to be hermetic (PLAN §14.1).
    let overlay = FixtureRepo::new().env_overlay();
    for name in command::DROPPED_ENV {
        assert!(
            overlay
                .iter()
                .any(|(key, value)| key == name && value.is_none()),
            "FixtureRepo::env_overlay keeps {name}"
        );
    }
}
