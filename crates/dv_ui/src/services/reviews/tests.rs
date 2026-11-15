use std::path::Path;
use std::time::{Duration, Instant};

use dv_core::github::RepoUrl;
use dv_core::review::ReviewStatus;
use dv_test_support::FixtureRepo;
use gpui::TestAppContext;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use dv_backend::wsl::Distro;

use super::*;
use crate::services::WslAccess;
use crate::services::locations::wsl::REFUSED;

fn object(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(object) => object,
        other => panic!("not an object: {other}"),
    }
}

fn local(path: &str) -> Location {
    Location::Local(path.into())
}

#[test]
fn known_repos_read_leniently_and_keep_what_they_dont_know() {
    let (file, problems) = KnownReposFile::from_object(object(json!({
        "v": 1,
        "repos": [
            {"location": {"local": "D:\\code\\api"}, "added_at": 5, "opened_at": 9, "pinned": true},
            {"location": {"wsl": {"distro": "Ubuntu", "path": "/home/kyle/web"}}, "added_at": 7},
            {"location": {"local": "D:\\code\\api"}, "added_at": 1},
            {"place": "somewhere else"},
        ],
        "window": {"x": 1},
    })))
    .unwrap();
    assert_eq!(problems.len(), 2, "{problems:?}");
    let repos: Vec<&KnownRepo> = file.repos().collect();
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0].location, local("D:\\code\\api"));
    assert_eq!(repos[0].opened_at, Some(9));
    assert_eq!(repos[0].last_used(), 9);
    assert_eq!(repos[1].last_used(), 7);
    let written: Value = serde_json::from_str(&file.to_json()).unwrap();
    assert_eq!(
        written,
        json!({
            "v": 1,
            "repos": [
                {"location": {"local": "D:\\code\\api"}, "added_at": 5, "opened_at": 9, "pinned": true},
                {"location": {"wsl": {"distro": "Ubuntu", "path": "/home/kyle/web"}}, "added_at": 7},
                {"place": "somewhere else"},
            ],
            "window": {"x": 1},
        })
    );
    // A newer version is read as far as it goes.
    let (file, problems) =
        KnownReposFile::from_object(object(json!({"v": 4, "repos": []}))).unwrap();
    assert_eq!(file.repos().count(), 0);
    assert!(problems[0].contains("newer dv"), "{problems:?}");
    // Repos that aren't a list make a file this version can't read.
    assert!(KnownReposFile::from_object(object(json!({"repos": {"a": 1}}))).is_err());
}

#[test]
fn a_known_repos_github_name_is_remembered_and_read_leniently() {
    // A file written before `github` existed loads, and so does one whose `github` isn't a
    // repository's name: it reads as none, the entry kept.
    let (mut file, problems) = KnownReposFile::from_object(object(json!({
        "v": 1,
        "repos": [
            {"location": {"local": "D:\\code\\old"}, "added_at": 1},
            {"location": {"local": "D:\\code\\api"}, "added_at": 2,
             "github": {"host": "github.com", "owner": "Acme", "repo": "API"}},
            {"location": {"local": "D:\\code\\odd"}, "added_at": 3, "github": "acme/odd"},
            {"location": {"local": "D:\\code\\evil"}, "added_at": 4,
             "github": {"host": "github.com", "owner": "a\nb", "repo": "x y"}},
        ],
    })))
    .unwrap();
    assert_eq!(problems, Vec::<String>::new());
    let github = |file: &KnownReposFile, path: &str| {
        file.get(&local(path))
            .expect("the entry is kept")
            .github
            .as_ref()
            .map(|github| format!("{}/{}/{}", github.host, github.owner, github.repo.repo))
    };
    assert_eq!(github(&file, "D:\\code\\old"), None);
    assert_eq!(
        github(&file, "D:\\code\\api").as_deref(),
        Some("github.com/Acme/API")
    );
    assert_eq!(github(&file, "D:\\code\\odd"), None);
    assert_eq!(github(&file, "D:\\code\\evil"), None);

    let named = |owner: &str, repo: &str| RepoUrl {
        host: "github.com".into(),
        owner: owner.into(),
        repo: repo.into(),
    };
    // The same name again changes nothing; another spelling of it, or another name, does.
    let api = local("D:\\code\\api");
    assert!(!file.set_github(&api, Some(named("Acme", "API"))));
    assert!(file.set_github(&api, Some(named("acme", "api"))));
    assert!(file.set_github(&local("D:\\code\\old"), Some(named("acme", "old"))));
    assert!(!file.set_github(&local("D:\\code\\odd"), None));
    assert!(!file.set_github(&local("D:\\nowhere"), Some(named("a", "b"))));
    let written: Value = serde_json::from_str(&file.to_json()).unwrap();
    assert_eq!(
        written["repos"],
        json!([
            {"location": {"local": "D:\\code\\old"}, "added_at": 1,
             "github": {"host": "github.com", "owner": "acme", "repo": "old"}},
            {"location": {"local": "D:\\code\\api"}, "added_at": 2,
             "github": {"host": "github.com", "owner": "acme", "repo": "api"}},
            {"location": {"local": "D:\\code\\odd"}, "added_at": 3},
            {"location": {"local": "D:\\code\\evil"}, "added_at": 4},
        ])
    );
    // A clone that left GitHub is remembered as having none.
    assert!(file.set_github(&api, None));
    assert_eq!(github(&file, "D:\\code\\api"), None);
}

/// A newer dv may keep more under `github` than this one reads: what it wrote there stays
/// through a load and a save, and through a name set again.
#[test]
fn keys_this_version_doesnt_read_inside_github_are_kept() {
    let repos = json!([
        {"location": {"local": "D:\\code\\api"}, "added_at": 2, "later": true,
         "github": {"host": "github.com", "owner": "acme", "repo": "api",
                    "default_branch": "main", "seen": {"at": 7}}},
    ]);
    let (mut file, problems) =
        KnownReposFile::from_object(object(json!({"v": 1, "repos": repos.clone()}))).unwrap();
    assert_eq!(problems, Vec::<String>::new());
    let written: Value = serde_json::from_str(&file.to_json()).unwrap();
    assert_eq!(written["repos"], repos);

    // Another spelling of the name: the name changes, the rest stays.
    let api = local("D:\\code\\api");
    assert!(file.set_github(
        &api,
        Some(RepoUrl {
            host: "github.com".into(),
            owner: "Acme".into(),
            repo: "API".into(),
        })
    ));
    let written: Value = serde_json::from_str(&file.to_json()).unwrap();
    assert_eq!(
        written["repos"][0]["github"],
        json!({"host": "github.com", "owner": "Acme", "repo": "API",
               "default_branch": "main", "seen": {"at": 7}})
    );
    // A clone that left GitHub has nothing under `github` to keep.
    assert!(file.set_github(&api, None));
    let written: Value = serde_json::from_str(&file.to_json()).unwrap();
    assert_eq!(written["repos"][0].get("github"), None);
    assert_eq!(written["repos"][0]["later"], json!(true));
}

#[test]
fn known_repos_add_mark_and_remove() {
    let mut file = KnownReposFile::default();
    assert!(file.add(local("D:\\a"), 10));
    assert!(!file.add(local("D:\\a"), 11), "a location is known once");
    assert!(file.mark_opened(&local("D:\\a"), 20));
    assert!(!file.mark_opened(&local("D:\\b"), 20));
    assert_eq!(file.get(&local("D:\\a")).unwrap().last_used(), 20);
    let (again, problems) =
        KnownReposFile::from_object(object(serde_json::from_str(&file.to_json()).unwrap()))
            .unwrap();
    assert!(problems.is_empty());
    assert_eq!(again, file);
    assert!(file.remove(&local("D:\\a")));
    assert_eq!(file.repos().count(), 0);
}

fn wsl_at(distro: &str, path: &str) -> Location {
    Location::Wsl {
        distro: distro.into(),
        path: path.into(),
    }
}

#[test]
fn a_wsl_repo_is_known_once_however_its_distro_is_spelled() {
    // Two runs that spelled the distro differently.
    let (mut file, problems) = KnownReposFile::from_object(object(json!({
        "v": 1,
        "repos": [
            {"location": {"wsl": {"distro": "ubuntu", "path": "/srv/api"}}, "added_at": 1},
            {"location": {"wsl": {"distro": "Ubuntu", "path": "/srv/api"}}, "added_at": 2},
        ],
    })))
    .unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(file.repos().count(), 1);
    assert!(!file.add(wsl_at("UBUNTU", "/srv/api"), 3));
    assert_eq!(
        file.find_place(&wsl_at("Ubuntu", "/srv/api"))
            .map(|repo| &repo.location),
        Some(&wsl_at("ubuntu", "/srv/api"))
    );
    // WSL's own spelling, once known, replaces it.
    assert!(file.rekey(&wsl_at("ubuntu", "/srv/api"), &wsl_at("Ubuntu", "/srv/api")));
    let repos: Vec<&KnownRepo> = file.repos().collect();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].location, wsl_at("Ubuntu", "/srv/api"));
    assert_eq!(repos[0].added_at, 1, "its times stay");
}

#[test]
fn a_subfolder_known_as_a_repo_becomes_its_root_once_resolved() {
    let mut file = KnownReposFile::default();
    assert!(file.add(local("D:\\api\\src"), 10));
    assert!(file.rekey(&local("D:\\api\\src"), &local("D:\\api")));
    assert_eq!(
        file.repos().map(|repo| &repo.location).collect::<Vec<_>>(),
        [&local("D:\\api")]
    );
    assert_eq!(file.get(&local("D:\\api")).unwrap().added_at, 10);
    // The root known already: the subfolder goes, and the later open is kept.
    assert!(file.add(local("D:\\api\\lib"), 11));
    assert!(file.mark_opened(&local("D:\\api\\lib"), 30));
    assert!(file.rekey(&local("D:\\api\\lib"), &local("D:\\api")));
    let repos: Vec<&KnownRepo> = file.repos().collect();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].location, local("D:\\api"));
    assert_eq!(repos[0].opened_at, Some(30));
    assert!(!file.rekey(&local("D:\\nowhere"), &local("D:\\api")));
    assert!(!file.rekey(&local("D:\\api"), &local("D:\\api")));
}

fn services(cx: &mut TestAppContext, data_dir: &Path) -> (Entity<Locations>, Entity<Reviews>) {
    services_with(cx, data_dir, Vec::new())
}

/// [`services`] with WSL's stand-in knowing `distros`.
fn services_with(
    cx: &mut TestAppContext,
    data_dir: &Path,
    distros: Vec<Distro>,
) -> (Entity<Locations>, Entity<Reviews>) {
    let locations = cx.new(|cx| Locations::new(WslAccess::scripted(distros), cx));
    let github = cx.new(|_| crate::services::GitHub::without_gh());
    let reviews = cx.new(|cx| Reviews::new(locations.clone(), github, data_dir, cx));
    (locations, reviews)
}

fn settle(cx: &mut TestAppContext, reviews: &Entity<Reviews>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if reviews.read_with(cx, |reviews, cx| reviews.is_settled(cx)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the reviews service never settled"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn fixture() -> FixtureRepo {
    let repo = FixtureRepo::new();
    repo.write("src/a.ts", "export const a = 1;\n");
    repo.write("src/b.ts", "export const b = 1;\n");
    repo.commit("base");
    repo.write("src/a.ts", "export const a = 2;\nexport const z = 0;\n");
    repo.write("notes.md", "# notes\n");
    repo
}

#[gpui::test]
async fn a_repo_is_added_listed_and_saved(cx: &mut TestAppContext) {
    let data = tempfile::tempdir().unwrap();
    let repo = fixture();
    let (locations, reviews) = services(cx, data.path());
    locations.update(cx, |locations, _| {
        locations.register_env(repo.path().to_path_buf(), repo.env_overlay())
    });
    let added = reviews
        .update(cx, |reviews, cx| {
            reviews.add_repo(Location::Local(repo.path().join("src")), cx)
        })
        .await
        .expect("a repo");
    assert_eq!(
        added,
        Location::Local(repo.path().to_path_buf()),
        "its root is known"
    );
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let rows = reviews.rows();
        assert_eq!(rows.len(), 1);
        let row = rows[0];
        assert!(!row.loading);
        assert_eq!(row.error, None);
        assert_eq!(row.name, "repo");
        assert_eq!(
            row.working,
            Some(WorkingSummary {
                files: 2,
                additions: 3,
                deletions: 1,
            })
        );
        assert!(row.reviews.is_empty());
        assert_eq!(row.labels.head.as_deref(), Some("main"));
    });

    let opened = reviews
        .update(cx, |reviews, cx| {
            reviews.open_review(added.clone(), Target::Working, cx)
        })
        .await
        .expect("a review");
    assert!(opened.created);
    assert_eq!(
        opened.reviewer,
        Author::reviewer(dv_test_support::AUTHOR_NAME)
    );
    // The labels come with the review, so its session names the target in full at once.
    assert_eq!(opened.labels.head.as_deref(), Some("main"));
    let resumed = reviews
        .update(cx, |reviews, cx| {
            reviews.open_review(added.clone(), Target::Working, cx)
        })
        .await
        .unwrap();
    assert!(!resumed.created);
    assert_eq!(resumed.loaded.review.id, opened.loaded.review.id);
    // A review asked for by id loads as it is.
    let by_id = reviews
        .update(cx, |reviews, cx| {
            reviews.open(
                added.clone(),
                OpenRequest::Review(opened.loaded.review.id.clone()),
                cx,
            )
        })
        .await
        .unwrap();
    assert!(!by_id.created);
    assert_eq!(by_id.loaded.review.id, opened.loaded.review.id);
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let row = reviews.row(&added).unwrap();
        assert_eq!(row.unreachable, None);
        assert_eq!(
            row.toplevel.as_deref(),
            Some(repo.path().to_str().unwrap()),
            "the worktree its reviews record"
        );
        assert_eq!(
            row.open_review_of(&Target::Working)
                .map(|summary| &summary.id),
            Some(&opened.loaded.review.id)
        );
        assert_eq!(row.open_review_of(&Target::Staged), None);
        assert_eq!(row.reviews.len(), 1);
        let ReviewRow::Ok { summary, label, .. } = &row.reviews[0] else {
            panic!("a readable review");
        };
        assert_eq!(summary.status, ReviewStatus::Open);
        assert_eq!(label, "working tree vs main");
        assert!(reviews.known().get(&added).unwrap().opened_at.is_some());
    });

    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(data.path().join(KNOWN_REPOS_FILE)).unwrap())
            .unwrap();
    assert_eq!(saved["v"], 1);
    assert_eq!(saved["repos"].as_array().unwrap().len(), 1);
    assert_eq!(
        saved["repos"][0]["location"],
        serde_json::to_value(&added).unwrap()
    );

    // A new service reads them back.
    let (_, again) = services(cx, data.path());
    again.read_with(cx, |again, _| {
        assert_eq!(again.rows().len(), 1);
        assert_eq!(again.rows()[0].location, added);
    });
}

#[gpui::test]
fn a_dropped_add_still_adds_and_settles(cx: &mut TestAppContext) {
    let data = tempfile::tempdir().unwrap();
    let repo = fixture();
    let (locations, reviews) = services(cx, data.path());
    locations.update(cx, |locations, _| {
        locations.register_env(repo.path().to_path_buf(), repo.env_overlay())
    });
    let location = Location::Local(repo.path().to_path_buf());
    let dropped = reviews.update(cx, |reviews, cx| reviews.add_repo(location.clone(), cx));
    drop(dropped);
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let row = reviews.row(&location).expect("added");
        assert!(row.working.is_some());
    });
}

#[gpui::test]
async fn an_unreadable_review_log_is_a_row_of_its_own(cx: &mut TestAppContext) {
    let data = tempfile::tempdir().unwrap();
    let repo = fixture();
    let (locations, reviews) = services(cx, data.path());
    locations.update(cx, |locations, _| {
        locations.register_env(repo.path().to_path_buf(), repo.env_overlay())
    });
    let location = Location::Local(repo.path().to_path_buf());
    let opened = reviews
        .update(cx, |reviews, cx| {
            reviews.open_review(location.clone(), Target::Staged, cx)
        })
        .await
        .unwrap();
    let broken = dv_core::review::ReviewId::generate();
    std::fs::write(
        repo.path().join(format!(".git/dv/reviews/{broken}.jsonl")),
        "not json\n",
    )
    .unwrap();
    reviews.update(cx, |reviews, cx| reviews.refresh(location.clone(), cx));
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let row = reviews.row(&location).unwrap();
        assert_eq!(row.error, None, "one bad log doesn't fail the repo");
        let ids: Vec<&ReviewId> = row.reviews.iter().map(ReviewRow::id).collect();
        assert!(ids.contains(&&opened.loaded.review.id));
        let bad = row
            .reviews
            .iter()
            .find(|review| review.id() == &broken)
            .expect("the broken log is listed");
        assert!(matches!(bad, ReviewRow::Err { .. }), "{bad:?}");
    });
}

#[gpui::test]
async fn a_stopped_distros_repo_matches_a_pull_request_by_its_remembered_name(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let data = tempfile::tempdir().unwrap();
    let wsl = wsl_at("Ubuntu", "/home/kyle/api");
    let elsewhere = wsl_at("Ubuntu", "/home/kyle/web");
    // Known from an earlier run, with the GitHub repository each clone was then.
    std::fs::write(
        data.path().join(KNOWN_REPOS_FILE),
        json!({"v": 1, "repos": [
            {"location": wsl, "added_at": 1, "opened_at": 5,
             "github": {"host": "github.com", "owner": "Acme", "repo": "Api"}},
            {"location": elsewhere, "added_at": 2,
             "github": {"host": "ghe.example", "owner": "acme", "repo": "api"}},
        ]})
        .to_string(),
    )
    .unwrap();
    let stopped = vec![Distro {
        name: "Ubuntu".into(),
        running: false,
    }];
    let (locations, reviews) = services_with(cx, data.path(), stopped);
    let found = |reviews: &Reviews, host: Option<&str>, owner: &str, repo: &str| {
        reviews
            .clones_of(host, owner, repo)
            .into_iter()
            .map(|row| row.location.clone())
            .collect::<Vec<_>>()
    };
    // Before anything is listed, and whatever the case: nothing is opened to tell.
    reviews.read_with(cx, |reviews, _| {
        assert_eq!(
            found(reviews, Some("github.com"), "acme", "API"),
            std::slice::from_ref(&wsl)
        );
        // A reference without a host names the repository on any: the most recently used
        // first.
        assert_eq!(
            found(reviews, None, "ACME", "api"),
            [wsl.clone(), elsewhere.clone()]
        );
        assert_eq!(
            found(reviews, Some("ghe.example"), "acme", "api"),
            std::slice::from_ref(&elsewhere)
        );
        assert_eq!(found(reviews, None, "acme", "web"), Vec::<Location>::new());
    });
    // A listing leaves the distro stopped and the names as they were remembered.
    reviews.update(cx, |reviews, cx| reviews.refresh_all(cx));
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        assert_eq!(
            reviews.row(&wsl).unwrap().stopped.as_deref(),
            Some("Ubuntu")
        );
        assert_eq!(found(reviews, None, "acme", "api").len(), 2);
    });
    locations.read_with(cx, |locations, _| {
        assert!(
            locations.connections().all().is_empty(),
            "the listing started something"
        );
    });
}

#[gpui::test]
async fn a_wsl_repo_lists_without_starting_its_distro_and_start_connects(cx: &mut TestAppContext) {
    // A connection reports its state from a thread of its own.
    cx.executor().allow_parking();
    let data = tempfile::tempdir().unwrap();
    let wsl = Location::Wsl {
        distro: "Ubuntu".into(),
        path: "/home/kyle/api".into(),
    };
    // Known from an earlier run; its distro has stopped since.
    std::fs::write(
        data.path().join(KNOWN_REPOS_FILE),
        json!({"v": 1, "repos": [{"location": wsl, "added_at": 1}]}).to_string(),
    )
    .unwrap();
    let stopped = vec![Distro {
        name: "Ubuntu".into(),
        running: false,
    }];
    let (locations, reviews) = services_with(cx, data.path(), stopped);
    reviews.update(cx, |reviews, cx| reviews.refresh_all(cx));
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let row = reviews.row(&wsl).unwrap();
        assert_eq!(row.unreachable.as_deref(), Some("Ubuntu isn't running"));
        assert_eq!(row.stopped.as_deref(), Some("Ubuntu"));
        assert!(!row.is_readable(), "no targets until it runs");
        assert_eq!(row.name, "api");
        assert_eq!(row.working, None);
    });
    locations.read_with(cx, |locations, _| {
        assert!(
            locations.connections().all().is_empty(),
            "the listing started nothing"
        )
    });

    // Start connects, booting the distro; the stand-in refuses, and the row says why.
    let started = reviews.update(cx, |reviews, cx| reviews.start(wsl.clone(), cx));
    reviews.read_with(cx, |reviews, _| {
        let row = reviews.row(&wsl).unwrap();
        assert!(row.starting);
        assert_eq!(row.stopped.as_deref(), Some("Ubuntu"), "Starting Ubuntu…");
    });
    let failure = started.await.expect_err("nothing starts in a test");
    assert!(failure.message.contains(REFUSED), "{failure:?}");
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let row = reviews.row(&wsl).unwrap();
        assert!(!row.starting);
        // Its distro still isn't running (the stand-in boots nothing): Start again, whatever
        // its connection did, the start's failure kept as the row's error.
        assert_eq!(row.stopped.as_deref(), Some("Ubuntu"));
        assert!(!row.retry);
        assert_eq!(row.unreachable.as_deref(), Some("Ubuntu isn't running"));
        let error = row.error.as_ref().expect("the connection failed");
        assert!(error.message.contains(REFUSED), "{error:?}");
    });
    // Running (as a real start would leave it), the failed connection is one to retry.
    locations.read_with(cx, |locations, _| {
        locations.wsl().set_distros(vec![Distro {
            name: "Ubuntu".into(),
            running: true,
        }])
    });
    reviews.update(cx, |reviews, cx| reviews.refresh(wsl.clone(), cx));
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let row = reviews.row(&wsl).unwrap();
        assert_eq!(row.stopped, None);
        assert!(row.retry);
        assert!(!row.is_readable());
        let error = row.error.as_ref().expect("the connection failed");
        assert!(error.message.contains(REFUSED), "{error:?}");
        assert!(
            row.unreachable
                .as_deref()
                .is_some_and(|reason| reason.starts_with("the connection to dv failed")),
            "{row:?}"
        );
    });
    // Opening its review tries again, and fails the same way: a one-line headline, the
    // connection's error and diagnostics in the detail (DESIGN §4.4).
    let failure = reviews
        .update(cx, |reviews, cx| {
            reviews.open_review(wsl.clone(), Target::Working, cx)
        })
        .await
        .err()
        .expect("refused");
    assert_eq!(failure.message, "couldn't open Ubuntu:/home/kyle/api");
    assert!(!failure.disconnected);
    let detail = failure.detail.as_deref().expect("why, and the diagnostics");
    assert!(
        detail.starts_with("Ubuntu can't be opened: the connection to dv failed: ")
            && detail.contains(REFUSED)
            && detail.contains(
                "

connection: Ubuntu
launcher: automation's stand-in"
            ),
        "{detail}"
    );
    // A WSL repository whose connection fails is known as given, its row saying why.
    let web = Location::Wsl {
        distro: "Ubuntu".into(),
        path: "/srv/web".into(),
    };
    let added = reviews
        .update(cx, |reviews, cx| reviews.add_repo(web.clone(), cx))
        .await
        .unwrap();
    assert_eq!(added, web);

    let missing = Location::Local(data.path().join("nowhere"));
    let failure = reviews
        .update(cx, |reviews, cx| reviews.add_repo(missing.clone(), cx))
        .await
        .expect_err("a missing path isn't added");
    assert!(failure.message.contains("no such directory"), "{failure:?}");
    reviews.read_with(cx, |reviews, _| assert!(reviews.row(&missing).is_none()));
}

#[gpui::test]
async fn a_file_dv_cant_read_is_never_saved_over(cx: &mut TestAppContext) {
    // The WSL repo's connection reports its state from a thread of its own.
    cx.executor().allow_parking();
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join(KNOWN_REPOS_FILE);
    for original in ["[\"a list\"]\n", "{\"repos\": \"text\"}\n"] {
        std::fs::write(&path, original).unwrap();
        let (_, reviews) = services(cx, data.path());
        let wsl = Location::Wsl {
            distro: "Ubuntu".into(),
            path: "/srv/x".into(),
        };
        reviews
            .update(cx, |reviews, cx| reviews.add_repo(wsl, cx))
            .await
            .unwrap();
        settle(cx, &reviews);
        reviews.read_with(cx, |reviews, cx| {
            assert_eq!(reviews.rows().len(), 1, "known for this session");
            assert!(
                reviews
                    .save_error(cx)
                    .is_some_and(|error| error.contains("left as it is")),
                "{:?}",
                reviews.save_error(cx)
            );
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}

#[gpui::test]
async fn another_worktrees_working_review_is_named_by_its_own_head(cx: &mut TestAppContext) {
    let data = tempfile::tempdir().unwrap();
    let repo = fixture();
    // A working review made in a linked worktree on `feature`.
    let linked = repo.add_worktree("wt", "feature");
    std::fs::write(linked.join("notes.md"), "# linked\n").unwrap();
    let backend: Arc<dyn Backend> = Arc::new(
        dv_core::LocalBackend::open_with(
            &linked,
            dv_core::backend::LocalBackendOptions {
                env: repo.env_overlay(),
            },
        )
        .unwrap(),
    );
    let made = ops::open_review(
        &ReviewStore::new(backend),
        Target::Working,
        Author::reviewer(dv_test_support::AUTHOR_NAME),
    )
    .unwrap();
    let (locations, reviews) = services(cx, data.path());
    locations.update(cx, |locations, _| {
        locations.register_env(repo.path().to_path_buf(), repo.env_overlay())
    });
    let added = reviews
        .update(cx, |reviews, cx| {
            reviews.add_repo(Location::Local(repo.path().to_path_buf()), cx)
        })
        .await
        .expect("a repo");
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let row = reviews.row(&added).unwrap();
        let labels: Vec<(&ReviewId, &str)> = row
            .reviews
            .iter()
            .filter_map(|review| match review {
                ReviewRow::Ok { summary, label, .. } => Some((&summary.id, label.as_str())),
                ReviewRow::Err { .. } => None,
            })
            .collect();
        // Listed from the main checkout (on main), it is still the feature worktree's.
        assert_eq!(labels, [(&made.review.id, "working tree vs feature")]);
    });
}

#[gpui::test]
async fn a_row_follows_its_connection_reconnecting_failed_and_retried(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicBool, Ordering};

    use dv_backend::{Connection, ConnectionState, Launcher};

    use crate::services::Route;

    cx.executor().allow_parking();
    let data = tempfile::tempdir().unwrap();
    let repo = fixture();
    let blocked = Arc::new(AtomicBool::new(false));
    let launcher = Launcher::custom("an in-process server tests block", {
        let blocked = blocked.clone();
        move || {
            if blocked.load(Ordering::SeqCst) {
                return Err(std::io::Error::other("blocked by the test"));
            }
            Launcher::InProcess.start()
        }
    });
    let connection =
        Connection::with_backoff("loopback", launcher, vec![Duration::from_millis(1500)]);
    let (locations, reviews) = services(cx, data.path());
    locations.update(cx, |locations, _| {
        locations.register(
            repo.path().to_path_buf(),
            Route {
                env: repo.env_overlay(),
                connection: Some(connection.clone()),
            },
        )
    });
    let location = Location::Local(repo.path().to_path_buf());
    reviews
        .update(cx, |reviews, cx| reviews.add_repo(location.clone(), cx))
        .await
        .expect("added");
    settle(cx, &reviews);
    let listed = reviews.read_with(cx, |reviews, _| reviews.row(&location).unwrap().working);
    assert!(listed.is_some());

    // Reconnecting: the row says so and keeps what it listed.
    connection.kill();
    reviews.update(cx, |reviews, cx| reviews.refresh(location.clone(), cx));
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let row = reviews.row(&location).unwrap();
        assert!(row.reconnecting, "{row:?}");
        assert_eq!(row.working, listed, "the rows it had stay");
        assert_eq!(row.error, None);
        assert!(row.is_readable());
    });

    // Failed: why, and Retry.
    blocked.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(connection.state(), ConnectionState::Failed { .. }) {
        assert!(Instant::now() < deadline, "{:?}", connection.state());
        std::thread::sleep(Duration::from_millis(20));
    }
    reviews.update(cx, |reviews, cx| reviews.refresh(location.clone(), cx));
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let row = reviews.row(&location).unwrap();
        assert!(!row.reconnecting);
        assert!(row.retry, "{row:?}");
        assert_eq!(row.stopped, None, "this machine never stops");
        assert_eq!(row.working, None);
        assert!(
            row.unreachable.as_deref().is_some_and(|reason| reason
                == "the connection to dv failed: couldn't start dv: blocked by the test"),
            "{row:?}"
        );
    });

    // Retry: the explicit open connects, and the row lists as before.
    blocked.store(false, Ordering::SeqCst);
    reviews
        .update(cx, |reviews, cx| reviews.start(location.clone(), cx))
        .await
        .expect("retried");
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let row = reviews.row(&location).unwrap();
        assert!(!row.retry && !row.reconnecting && !row.starting, "{row:?}");
        assert_eq!(row.working, listed);
        assert_eq!(row.error, None);
    });
    assert_eq!(connection.state(), ConnectionState::Connected);
}

#[gpui::test]
async fn a_row_follows_its_connection_with_the_switcher_closed(cx: &mut TestAppContext) {
    use std::sync::atomic::{AtomicBool, Ordering};

    use dv_backend::{Connection, ConnectionState, Launcher};

    use crate::services::Route;

    cx.executor().allow_parking();
    let data = tempfile::tempdir().unwrap();
    let repo = fixture();
    // A host the test stops, as the user stops a distro.
    let up = Arc::new(AtomicBool::new(true));
    let launcher = Launcher::custom("an in-process server on a host the test stops", {
        let up = up.clone();
        move || {
            up.store(true, Ordering::SeqCst);
            Launcher::InProcess.start()
        }
    })
    .with_host_check({
        let up = up.clone();
        move |_| up.load(Ordering::SeqCst)
    });
    let connection =
        Connection::with_backoff("loopback", launcher, vec![Duration::from_millis(20)]);
    let (locations, reviews) = services(cx, data.path());
    locations.update(cx, |locations, _| {
        locations.register(
            repo.path().to_path_buf(),
            Route {
                env: repo.env_overlay(),
                connection: Some(connection.clone()),
            },
        )
    });
    let location = Location::Local(repo.path().to_path_buf());
    reviews
        .update(cx, |reviews, cx| reviews.add_repo(location.clone(), cx))
        .await
        .expect("added");
    settle(cx, &reviews);
    let listed = reviews.read_with(cx, |reviews, _| reviews.row(&location).unwrap().working);
    assert!(listed.is_some());

    // Stopped: no switcher asks, yet the row hears it and offers Start.
    up.store(false, Ordering::SeqCst);
    connection.kill();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        settle(cx, &reviews);
        let stopped = reviews.read_with(cx, |reviews, _| {
            reviews.row(&location).unwrap().stopped.clone()
        });
        if stopped.is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "the row never heard the stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(connection.state(), ConnectionState::Stopped);

    // Started elsewhere (the backend-down banner's Start): the row is live again before any
    // switcher opens.
    connection.retry().expect("started");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        settle(cx, &reviews);
        let row = reviews.read_with(cx, |reviews, _| reviews.row(&location).unwrap().clone());
        if row.working.is_some() {
            assert_eq!(row.stopped, None);
            assert!(!row.retry && !row.reconnecting, "{row:?}");
            assert_eq!(row.unreachable, None);
            assert_eq!(row.error, None);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the row never came back: {row:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The locations `known_repos.json` holds.
fn saved_locations(data_dir: &Path) -> Vec<Location> {
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(data_dir.join(KNOWN_REPOS_FILE)).unwrap())
            .unwrap();
    saved["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| serde_json::from_value(repo["location"].clone()).unwrap())
        .collect()
}

#[gpui::test]
async fn a_subfolder_added_while_its_connection_is_down_becomes_its_repos_row(
    cx: &mut TestAppContext,
) {
    use std::sync::atomic::{AtomicBool, Ordering};

    use dv_backend::{Connection, Launcher};

    use crate::services::Route;

    cx.executor().allow_parking();
    // Resolved by the switcher's Retry, by a listing once the connection is back, or by
    // opening a review from its row.
    for how in ["retry", "listing", "open"] {
        let data = tempfile::tempdir().unwrap();
        let repo = fixture();
        let blocked = Arc::new(AtomicBool::new(true));
        let launcher = Launcher::custom("an in-process server tests block", {
            let blocked = blocked.clone();
            move || {
                if blocked.load(Ordering::SeqCst) {
                    return Err(std::io::Error::other("blocked by the test"));
                }
                Launcher::InProcess.start()
            }
        });
        let connection =
            Connection::with_backoff("loopback", launcher, vec![Duration::from_millis(10)]);
        let (locations, reviews) = services(cx, data.path());
        locations.update(cx, |locations, _| {
            locations.register(
                repo.path().to_path_buf(),
                Route {
                    env: repo.env_overlay(),
                    connection: Some(connection.clone()),
                },
            )
        });
        let root = Location::Local(repo.path().to_path_buf());
        let sub = Location::Local(repo.path().join("src"));
        let added = reviews
            .update(cx, |reviews, cx| reviews.add_repo(sub.clone(), cx))
            .await
            .expect("known as given, its row saying why");
        assert_eq!(added, sub, "{how}");
        settle(cx, &reviews);
        reviews.read_with(cx, |reviews, _| {
            assert!(reviews.row(&sub).is_some_and(|row| row.retry), "{how}")
        });

        blocked.store(false, Ordering::SeqCst);
        match how {
            "retry" => reviews
                .update(cx, |reviews, cx| reviews.start(sub.clone(), cx))
                .await
                .expect("it connects"),
            "listing" => connection.retry().expect("it connects"),
            _ => {
                reviews
                    .update(cx, |reviews, cx| {
                        reviews.open_review(sub.clone(), Target::Working, cx)
                    })
                    .await
                    .expect("its review opens");
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            settle(cx, &reviews);
            let listed = reviews.read_with(cx, |reviews, _| {
                reviews.row(&root).is_some_and(|row| row.working.is_some())
            });
            if listed {
                break;
            }
            assert!(Instant::now() < deadline, "{how}: never listed by its root");
            std::thread::sleep(Duration::from_millis(20));
        }
        // Opening a review from the row keeps one row, known once.
        reviews
            .update(cx, |reviews, cx| {
                reviews.open_review(root.clone(), Target::Staged, cx)
            })
            .await
            .expect("its review opens");
        settle(cx, &reviews);
        reviews.read_with(cx, |reviews, _| {
            let rows: Vec<&Location> = reviews.rows().iter().map(|row| &row.location).collect();
            assert_eq!(rows, [&root], "{how}");
            let row = reviews.row(&root).unwrap();
            assert_eq!(row.name, "repo", "{how}");
            assert!(!row.retry && !row.starting, "{how}: {row:?}");
            assert_eq!(
                reviews
                    .known()
                    .repos()
                    .map(|repo| &repo.location)
                    .collect::<Vec<_>>(),
                [&root],
                "{how}"
            );
        });
        assert_eq!(saved_locations(data.path()), [root], "{how}");
    }
}

#[gpui::test]
fn a_known_repo_spelled_otherwise_becomes_the_spelling_wsl_resolves(cx: &mut TestAppContext) {
    let data = tempfile::tempdir().unwrap();
    let lower = wsl_at("ubuntu", "/srv/api");
    let upper = wsl_at("Ubuntu", "/srv/api");
    // Known from a run that spelled it in lower case.
    std::fs::write(
        data.path().join(KNOWN_REPOS_FILE),
        json!({"v": 1, "repos": [{"location": lower, "added_at": 1}]}).to_string(),
    )
    .unwrap();
    let (_, reviews) = services(cx, data.path());
    // A listing (or an open) of it reached the repository WSL names `Ubuntu:/srv/api`.
    reviews.update(cx, |reviews, cx| reviews.resolved(&lower, &upper, cx));
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let rows: Vec<&Location> = reviews.rows().iter().map(|row| &row.location).collect();
        assert_eq!(rows, [&upper]);
        assert_eq!(reviews.row(&upper).unwrap().path, "Ubuntu:/srv/api");
    });
    assert_eq!(saved_locations(data.path()), std::slice::from_ref(&upper));
    // Another spelling added later is the same repository.
    reviews.update(cx, |reviews, cx| {
        assert_eq!(reviews.add_known(lower.clone(), cx), upper);
    });
    assert_eq!(reviews.read_with(cx, |reviews, _| reviews.rows().len()), 1);
}

#[gpui::test]
async fn a_removed_repo_leaves_the_list_and_the_file_keeping_its_reviews(cx: &mut TestAppContext) {
    let data = tempfile::tempdir().unwrap();
    let repo = fixture();
    let other = fixture();
    let (locations, reviews) = services(cx, data.path());
    locations.update(cx, |locations, _| {
        locations.register_env(repo.path().to_path_buf(), repo.env_overlay());
        locations.register_env(other.path().to_path_buf(), other.env_overlay());
    });
    let location = Location::Local(repo.path().to_path_buf());
    let kept = Location::Local(other.path().to_path_buf());
    for added in [&location, &kept] {
        reviews
            .update(cx, |reviews, cx| reviews.add_repo(added.clone(), cx))
            .await
            .expect("a repo");
    }
    let opened = reviews
        .update(cx, |reviews, cx| {
            reviews.open_review(location.clone(), Target::Working, cx)
        })
        .await
        .expect("a review");
    settle(cx, &reviews);

    // Removed while a refresh of it runs: the row goes at once, and the refresh lands nowhere.
    let removed = reviews.update(cx, |reviews, cx| {
        reviews.refresh(location.clone(), cx);
        reviews.remove_repo(&location, cx)
    });
    assert!(removed);
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        assert!(reviews.row(&location).is_none());
        assert!(reviews.known().get(&location).is_none());
        assert_eq!(reviews.rows().len(), 1);
        assert_eq!(reviews.rows()[0].location, kept);
    });
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(data.path().join(KNOWN_REPOS_FILE)).unwrap())
            .unwrap();
    assert_eq!(
        saved["repos"].as_array().unwrap().len(),
        1,
        "saved without it"
    );
    assert_eq!(
        saved["repos"][0]["location"],
        serde_json::to_value(&kept).unwrap()
    );
    // Not known any more, so nothing to remove.
    assert!(!reviews.update(cx, |reviews, cx| reviews.remove_repo(&location, cx)));

    // Added again, it lists its review: nothing in the repository was touched.
    reviews
        .update(cx, |reviews, cx| reviews.add_repo(location.clone(), cx))
        .await
        .expect("added again");
    settle(cx, &reviews);
    reviews.read_with(cx, |reviews, _| {
        let row = reviews.row(&location).expect("listed again");
        assert_eq!(
            row.open_review_of(&Target::Working)
                .map(|summary| &summary.id),
            Some(&opened.loaded.review.id)
        );
    });
}
