//! The PR mirror's format and its store (PLAN §9.4).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use dv_test_support::FixtureRepo;
use serde_json::{Value, json};

use super::*;
use crate::backend::Backend;
use crate::git::Oid;
use crate::review::AnchorSide;
use crate::testing::{open, open_at};

fn oid(digit: char) -> Oid {
    Oid::parse(&digit.to_string().repeat(40)).unwrap()
}

fn key() -> MirrorKey {
    MirrorKey::new(&github_repo("github.com", "acme", "api"), 482, "kylekz")
}

fn comment(id: &str, review: Option<(&str, ReviewState)>) -> GithubComment {
    GithubComment {
        id: id.to_string(),
        database_id: 2_000_000_001,
        author: "octocat".to_string(),
        body: "Why not `?` here?".to_string(),
        created_at: 1_792_398_615_000,
        url: format!("https://github.com/acme/api/pull/482#discussion_r{id}"),
        diff_hunk: "@@ -38,3 +38,4 @@ fn load()\n     let a = 1;\n+    let b = a.unwrap();"
            .to_string(),
        outdated: false,
        commit: Some(oid('a')),
        original_commit: Some(oid('b')),
        reply_to: None,
        review: review.map(|(id, state)| CommentReview {
            id: id.to_string(),
            database_id: 3_000_000_001,
            state,
        }),
    }
}

/// A mirror with one of everything: a line thread on a range, an outdated one, a file thread
/// with the viewer's pending comment, a renamed file without a patch, every Viewed state.
fn mirror() -> Mirror {
    let thread = |id: &str, subject, comments| GithubThread {
        id: id.to_string(),
        path: "src/api.rs".to_string(),
        resolved: false,
        outdated: false,
        subject,
        viewer_can_resolve: true,
        viewer_can_unresolve: false,
        viewer_can_reply: true,
        comments,
    };
    Mirror {
        key: key(),
        read_at: 1_792_398_700_000,
        pr: PullRequest {
            id: "PR_kwDOAbc123".to_string(),
            title: "Load the config once".to_string(),
            body: "Fixes #400.\n".to_string(),
            url: "https://github.com/acme/api/pull/482".to_string(),
            state: PrState::Draft,
            author: "octocat".to_string(),
            base_ref: "main".to_string(),
            head_ref: "load-once".to_string(),
            head_oid: oid('a'),
            head_repo: HeadRepo::Fork {
                owner: Some("octocat".to_string()),
            },
            updated_at: 1_792_398_615_000,
            review_decision: Some(ReviewDecision::ChangesRequested),
            changed_files: 2,
            additions: 12,
            deletions: 3,
        },
        checks: Checks {
            passing: 4,
            failing: 1,
            pending: 2,
        },
        threads: vec![
            thread(
                "PRRT_1",
                ThreadSubject::Lines(ThreadLines {
                    side: AnchorSide::New,
                    line: Some(41),
                    original_line: Some(40),
                    start: Some(ThreadStart {
                        side: AnchorSide::New,
                        line: Some(39),
                        original_line: Some(38),
                    }),
                }),
                vec![
                    comment("PRRC_1", Some(("PRR_1", ReviewState::ChangesRequested))),
                    GithubComment {
                        reply_to: Some("PRRC_1".to_string()),
                        author: "ghost".to_string(),
                        ..comment("PRRC_2", Some(("PRR_2", ReviewState::Commented)))
                    },
                ],
            ),
            GithubThread {
                resolved: true,
                outdated: true,
                viewer_can_resolve: false,
                viewer_can_unresolve: true,
                ..thread(
                    "PRRT_2",
                    ThreadSubject::Lines(ThreadLines {
                        side: AnchorSide::Old,
                        line: None,
                        original_line: Some(7),
                        start: None,
                    }),
                    vec![GithubComment {
                        outdated: true,
                        commit: None,
                        review: None,
                        ..comment("PRRC_3", None)
                    }],
                )
            },
            thread(
                "PRRT_3",
                ThreadSubject::File,
                vec![comment("PRRC_4", Some(("PRR_9", ReviewState::Pending)))],
            ),
        ],
        pending_review: Some(PendingReview {
            id: "PRR_9".to_string(),
            database_id: 3_000_000_009,
            body: "<!-- dv:r-01jb2m5k8z0000000000000000 -->".to_string(),
            commit: Some(oid('a')),
        }),
        files: vec![
            FilePatch {
                path: "src/api.rs".to_string(),
                previous_path: None,
                status: FileStatus::Modified,
                patch: Some(
                    "@@ -38,3 +38,4 @@ fn load()\n     let a = 1;\n+    let b = 2;".to_string(),
                ),
            },
            FilePatch {
                path: "docs/new.md".to_string(),
                previous_path: Some("docs/old.md".to_string()),
                status: FileStatus::Renamed,
                patch: None,
            },
        ],
        viewed: BTreeMap::from([
            (
                "src/api.rs".to_string(),
                ViewedFile {
                    state: ViewedState::Dismissed,
                    synced: Some(true),
                },
            ),
            (
                "docs/new.md".to_string(),
                ViewedFile {
                    state: ViewedState::Unviewed,
                    synced: None,
                },
            ),
            (
                "docs/seen.md".to_string(),
                ViewedFile {
                    state: ViewedState::Viewed,
                    synced: Some(false),
                },
            ),
        ]),
    }
}

fn store(repo: &FixtureRepo) -> MirrorStore {
    MirrorStore::new(Arc::new(open(repo)))
}

/// The mirror file of [`key`] as stored JSON.
fn stored(repo: &FixtureRepo) -> Value {
    let bytes = open(repo).store_read(&key().path()).unwrap().unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Replaces the mirror file of [`key`] with `bytes`.
fn overwrite(repo: &FixtureRepo, bytes: &[u8]) {
    open(repo).store_write_atomic(&key().path(), bytes).unwrap();
}

fn refetch(read: MirrorRead) -> Refetch {
    match read {
        MirrorRead::Refetch(why) => why,
        MirrorRead::Found(mirror) => panic!("found {:?}", mirror.key),
    }
}

#[test]
fn a_mirror_round_trips_through_the_store() {
    let repo = FixtureRepo::new();
    let store = store(&repo);
    assert_eq!(refetch(store.read(&key()).unwrap()), Refetch::Missing);

    let mirror = mirror();
    store.write(&mirror).unwrap();
    assert_eq!(
        store.read(&key()).unwrap(),
        MirrorRead::Found(Box::new(mirror.clone()))
    );
    assert!(
        repo.path()
            .join(".git/dv/github/github.com/acme/api/pr-482/kylekz.json")
            .is_file()
    );

    // A refresh rewrites it whole.
    let moved = Mirror {
        read_at: mirror.read_at + 60_000,
        threads: Vec::new(),
        pending_review: None,
        ..mirror
    };
    store.write(&moved).unwrap();
    assert_eq!(
        store.read(&key()).unwrap(),
        MirrorRead::Found(Box::new(moved))
    );
}

#[test]
fn the_format_is_versioned_snake_case_json() {
    let repo = FixtureRepo::new();
    store(&repo).write(&mirror()).unwrap();
    let stored = stored(&repo);
    assert_eq!(stored["v"], json!(MIRROR_VERSION));
    assert_eq!(MIRROR_VERSION, 1);
    assert_eq!(
        stored["key"],
        json!({"host": "github.com", "owner": "acme", "repo": "api", "number": 482, "login": "kylekz"})
    );
    assert_eq!(stored["read_at"], json!(1_792_398_700_000u64));
    assert_eq!(stored["pr"]["state"], json!("draft"));
    assert_eq!(stored["pr"]["head_oid"], json!("a".repeat(40)));
    assert_eq!(
        stored["pr"]["head_repo"],
        json!({"fork": {"owner": "octocat"}})
    );
    assert_eq!(stored["pr"]["review_decision"], json!("changes_requested"));
    assert_eq!(
        stored["checks"],
        json!({"passing": 4, "failing": 1, "pending": 2})
    );
    assert_eq!(
        stored["threads"][0]["subject"],
        json!({"lines": {
            "side": "new",
            "line": 41,
            "original_line": 40,
            "start": {"side": "new", "line": 39, "original_line": 38}
        }})
    );
    assert_eq!(stored["threads"][2]["subject"], json!("file"));
    assert_eq!(
        stored["threads"][0]["comments"][0]["review"],
        json!({"id": "PRR_1", "database_id": 3_000_000_001u64, "state": "changes_requested"})
    );
    assert_eq!(stored["files"][1]["status"], json!("renamed"));
    assert_eq!(stored["files"][1]["patch"], Value::Null);
    assert_eq!(
        stored["viewed"]["src/api.rs"],
        json!({"state": "dismissed", "synced": true})
    );
}

#[test]
fn a_file_that_does_not_parse_is_fetched_again() {
    let repo = FixtureRepo::new();
    let store = store(&repo);
    store.write(&mirror()).unwrap();
    let current = stored(&repo);
    let whole = serde_json::to_vec(&current).unwrap();

    for bytes in [
        &b""[..],
        b"not json",
        // Cut off mid-write by something other than the atomic store write.
        &whole[..whole.len() / 2],
        b"[]",
        b"{}",
        b"{\"v\": \"1\"}",
        // This version's number on another shape.
        b"{\"v\": 1}",
        b"{\"v\": 1, \"key\": {\"host\": \"github.com\"}}",
    ] {
        overwrite(&repo, bytes);
        assert!(
            matches!(
                refetch(store.read(&key()).unwrap()),
                Refetch::Unparseable { .. }
            ),
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }

    // A field of the wrong type deep inside is no mirror either.
    let mut wrong = current;
    wrong["threads"][0]["comments"][0]["commit"] = json!("not an oid");
    overwrite(&repo, &serde_json::to_vec(&wrong).unwrap());
    assert!(matches!(
        refetch(store.read(&key()).unwrap()),
        Refetch::Unparseable { .. }
    ));

    // Writing again puts it right.
    store.write(&mirror()).unwrap();
    assert!(matches!(store.read(&key()).unwrap(), MirrorRead::Found(_)));
}

#[test]
fn another_version_is_fetched_again_and_unknown_fields_are_not_one() {
    let repo = FixtureRepo::new();
    let store = store(&repo);
    store.write(&mirror()).unwrap();
    let current = stored(&repo);

    for found in [0u64, 2, 99] {
        let mut other = current.clone();
        other["v"] = json!(found);
        overwrite(&repo, &serde_json::to_vec(&other).unwrap());
        assert_eq!(
            refetch(store.read(&key()).unwrap()),
            Refetch::OtherVersion { found }
        );
    }
    // A newer version need not have this one's fields at all.
    overwrite(&repo, b"{\"v\": 2, \"pull_request\": {}}");
    assert_eq!(
        refetch(store.read(&key()).unwrap()),
        Refetch::OtherVersion { found: 2 }
    );

    // An optional addition needs no version bump: this build ignores it.
    let mut grown = current;
    grown["labels"] = json!(["bug"]);
    grown["pr"]["milestone"] = json!("v2");
    grown["threads"][0]["comments"][0]["reactions"] = json!({"+1": 2});
    overwrite(&repo, &serde_json::to_vec(&grown).unwrap());
    assert_eq!(
        store.read(&key()).unwrap(),
        MirrorRead::Found(Box::new(mirror()))
    );
}

#[test]
fn a_mirror_is_per_pull_request_and_login() {
    let repo = FixtureRepo::new();
    let store = store(&repo);
    store.write(&mirror()).unwrap();

    let other_login = MirrorKey {
        login: "octocat".to_string(),
        ..key()
    };
    let other_pr = MirrorKey {
        number: 483,
        ..key()
    };
    let other_repo = MirrorKey {
        repo: "web".to_string(),
        ..key()
    };
    let other_host = MirrorKey {
        host: "ghe.example.com".to_string(),
        ..key()
    };
    for other in [&other_login, &other_pr, &other_repo, &other_host] {
        assert_ne!(*other, key());
        assert_ne!(other.path(), key().path());
        assert_eq!(refetch(store.read(other).unwrap()), Refetch::Missing);
    }

    // GitHub's names have no case: the same key, and so the same mirror, however they are
    // spelled.
    let shouted = MirrorKey {
        host: "GitHub.com".to_string(),
        owner: "ACME".to_string(),
        repo: "Api".to_string(),
        login: "KyleKZ".to_string(),
        ..key()
    };
    assert_eq!(shouted, key());
    assert_eq!(shouted.path(), key().path());
    assert_eq!(HashSet::from([shouted.clone(), key()]).len(), 1);
    assert_eq!(
        store.read(&shouted).unwrap(),
        MirrorRead::Found(Box::new(mirror()))
    );

    // A file that holds another key's mirror, as only a copy by hand makes one, isn't it.
    let copied = serde_json::to_vec(&stored(&repo)).unwrap();
    open(&repo)
        .store_write_atomic(&other_login.path(), &copied)
        .unwrap();
    assert_eq!(
        refetch(store.read(&other_login).unwrap()),
        Refetch::OtherKey
    );
}

#[test]
fn names_are_spelled_for_a_store_path() {
    let path = |host: &str, owner: &str, repo: &str, login: &str| {
        MirrorKey::new(&github_repo(host, owner, repo), 7, login).path()
    };
    assert_eq!(
        path("github.com", "acme", "api", "kylekz"),
        "github/github.com/acme/api/pr-7/kylekz.json"
    );
    assert_eq!(
        path(
            "ghe.example.com:8443",
            "Acme-Inc",
            ".github",
            "dependabot[bot]"
        ),
        "github/ghe.example.com%3a8443/acme-inc/%2egithub/pr-7/dependabot%5bbot%5d.json"
    );

    // Names that would share a looser spelling have files of their own, which the store
    // takes.
    let repo = FixtureRepo::new();
    let store = store(&repo);
    let keys = [".github", "_github", "%2egithub", "x.lock", "x_lock", "con"]
        .map(|name| MirrorKey::new(&github_repo("github.com", "acme", name), 482, "nul"));
    for key in &keys {
        store
            .write(&Mirror {
                key: key.clone(),
                ..mirror()
            })
            .unwrap();
    }
    for key in &keys {
        match store.read(key).unwrap() {
            MirrorRead::Found(found) => assert_eq!(found.key.repo, key.repo),
            MirrorRead::Refetch(why) => panic!("{}: {why:?}", key.repo),
        }
    }
}

#[test]
fn the_latest_mirror_is_the_most_recently_read_of_any_login() {
    let repo = FixtureRepo::new();
    let store = store(&repo);
    let pr = github_repo("GitHub.com", "Acme", "API");
    assert_eq!(store.latest(&pr, 482).unwrap(), None);

    let read_by = |login: &str, read_at: u64| Mirror {
        key: MirrorKey {
            login: login.to_string(),
            ..key()
        },
        read_at,
        ..mirror()
    };
    store.write(&read_by("kylekz", 1_000)).unwrap();
    store.write(&read_by("octocat", 3_000)).unwrap();
    store.write(&read_by("hubot", 2_000)).unwrap();
    // Other pull requests' mirrors, and files that aren't this build's mirrors, don't count.
    store
        .write(&Mirror {
            key: MirrorKey {
                number: 483,
                ..key()
            },
            read_at: 9_000,
            ..mirror()
        })
        .unwrap();
    let backend = open(&repo);
    let beside = |name: &str| key().path().replace("kylekz.json", name);
    backend
        .store_write_atomic(&beside("torn.json"), b"{\"v\": 1, \"key\"")
        .unwrap();
    backend
        .store_write_atomic(&beside("newer.json"), b"{\"v\": 2, \"read_at\": 9000}")
        .unwrap();
    let mut misplaced = serde_json::to_value(read_by("kylekz", 8_000)).unwrap();
    misplaced["v"] = json!(MIRROR_VERSION);
    misplaced["key"]["number"] = json!(999);
    backend
        .store_write_atomic(
            &beside("misplaced.json"),
            &serde_json::to_vec(&misplaced).unwrap(),
        )
        .unwrap();

    // Strays under names the store itself refuses to read, as a file manager leaves them.
    let dir = repo
        .path()
        .join(".git/dv/github/github.com/acme/api/pr-482");
    let newest = serde_json::to_vec(&misplaced).unwrap();
    for name in ["kylekz copy.json", "ünï.json", "notes (1).txt"] {
        std::fs::write(dir.join(name), &newest).unwrap();
    }

    let latest = store.latest(&pr, 482).unwrap().unwrap();
    assert_eq!(latest.key.login, "octocat");
    assert_eq!(latest.read_at, 3_000);
    assert_eq!(store.latest(&pr, 484).unwrap(), None);
}

#[test]
fn every_worktree_reads_the_same_mirror() {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
    let worktree = repo.add_worktree("second", "second");
    let linked = MirrorStore::new(Arc::new(open_at(&repo, &worktree)));
    linked.write(&mirror()).unwrap();
    assert_eq!(
        store(&repo).read(&key()).unwrap(),
        MirrorRead::Found(Box::new(mirror()))
    );
}

#[test]
fn the_pending_reviews_comments_are_the_threads_own() {
    let mirror = mirror();
    let pending: Vec<(&str, &str)> = mirror
        .pending_comments()
        .map(|(thread, comment)| (thread.id.as_str(), comment.id.as_str()))
        .collect();
    assert_eq!(pending, [("PRRT_3", "PRRC_4")]);

    // Without a pending review nothing is pending, a comment without a review included.
    let none = Mirror {
        pending_review: None,
        ..mirror
    };
    assert_eq!(none.pending_comments().count(), 0);
}
