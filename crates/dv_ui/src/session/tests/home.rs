//! A review opens bound to its home worktree (PLAN §2.2, §2.4, `Reviews::open`): from any
//! worktree's row it lists, places and snapshots the worktree its target resolves in, as the
//! CLI does.

use dv_core::Location;
use dv_core::review::ReviewId;
use pretty_assertions::assert_eq;

use super::*;
use crate::services::reviews::{OpenRequest, Reviews};
use crate::services::{EnvOverlay, Locations, WslAccess};

/// The fixture with a linked worktree on `feature` whose working tree edits `src/a.rs` its own
/// way and adds a file of its own.
fn with_linked() -> (FixtureRepo, std::path::PathBuf) {
    let repo = fixture();
    let linked = repo.add_worktree("wt", "feature");
    std::fs::write(linked.join("src/a.rs"), "fn a() -> u32 {\n    3\n}\n").unwrap();
    std::fs::write(linked.join("linked.txt"), "only here\n").unwrap();
    (repo, linked)
}

/// The services a review opens through, with the fixture's environment for both worktrees.
fn services(
    repo: &FixtureRepo,
    linked: &std::path::Path,
    data: &std::path::Path,
    cx: &mut TestAppContext,
) -> Entity<Reviews> {
    let locations = cx.new(|cx| Locations::new(WslAccess::scripted(Vec::new()), cx));
    locations.update(cx, |locations, _| {
        let env: EnvOverlay = repo.env_overlay();
        locations.register_env(repo.path().to_path_buf(), env.clone());
        locations.register_env(linked.to_path_buf(), env);
    });
    let github = cx.new(|_| crate::services::GitHub::without_gh());
    cx.new(|cx| Reviews::new(locations, github, data, cx))
}

/// Opens `request` from the main work tree's row, as the switcher would.
async fn open_from_main(
    reviews: &Entity<Reviews>,
    repo: &FixtureRepo,
    request: OpenRequest,
    cx: &mut TestAppContext,
) -> Result<crate::services::reviews::OpenedReview, crate::failure::Failure> {
    let main = Location::Local(repo.path().to_path_buf());
    reviews
        .update(cx, |reviews, cx| reviews.open(main, request, cx))
        .await
}

#[gpui::test]
async fn another_worktrees_review_opens_in_its_own_worktree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let data = tempfile::tempdir().unwrap();
    let (repo, linked) = with_linked();
    let linked_store = ReviewStore::new(Arc::new(
        LocalBackend::open_with(
            &linked,
            LocalBackendOptions {
                env: repo.env_overlay(),
            },
        )
        .unwrap(),
    ));
    let working = ops::open_review(&linked_store, Target::Working, reviewer())
        .unwrap()
        .review;
    let reviews = services(&repo, &linked, data.path(), cx);

    // Asked for by id from the main work tree's row (the switcher's rows of other targets,
    // `dv open <main> --review <id>`): its own worktree's backend, files and labels.
    let opened = open_from_main(&reviews, &repo, OpenRequest::Review(working.id.clone()), cx)
        .await
        .expect("it opens");
    assert!(dv_core::review::same_worktree(
        &opened.backend.repo().location,
        &opened.backend.repo().toplevel,
        &linked.display().to_string()
    ));
    assert_eq!(opened.origin, Location::Local(repo.path().to_path_buf()));
    assert_eq!(opened.labels.head.as_deref(), Some("feature"));
    let session = cx.new(|cx| ReviewSession::new(opened, cx));
    wait(cx, &session, "the list", |session| !session.is_loading());
    session.read_with(cx, |session, _| {
        assert_eq!(paths(session), ["linked.txt", "src/a.rs"]);
        assert!(!session.is_home_origin());
    });
    // The row it was opened from is the repository used, not the linked worktree.
    reviews.read_with(cx, |reviews, _| {
        let known: Vec<&Location> = reviews.known().repos().map(|repo| &repo.location).collect();
        assert_eq!(known, [&Location::Local(repo.path().to_path_buf())]);
    });

    // Finishing its round snapshots the linked worktree, not the main work tree.
    let finished = session.update(cx, |session, cx| {
        session.finish_round(Some(Verdict::RequestChanges), None, cx)
    });
    wait(cx, &session, "the round", |session| {
        session.review().status == ReviewStatus::ChangesRequested
    });
    cx.foreground_executor().block_test(finished).unwrap();
    let snapshot = format!("refs/dv/reviews/{}/1", working.id);
    assert_eq!(
        repo.git(&["show", &format!("{snapshot}:src/a.rs")]),
        "fn a() -> u32 {\n    3\n}\n"
    );
    assert_eq!(
        repo.git(&["show", &format!("{snapshot}:linked.txt")]),
        "only here\n"
    );
}

#[gpui::test]
async fn a_branch_review_opens_where_the_branch_is_checked_out(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let data = tempfile::tempdir().unwrap();
    let (repo, linked) = with_linked();
    let reviews = services(&repo, &linked, data.path(), cx);
    let branch = Target::Branch {
        branch: "feature".to_string(),
        base: Some("main".to_string()),
    };
    // Opened by target from the main work tree's row (the source picker, `open_review`).
    let opened = open_from_main(&reviews, &repo, OpenRequest::Target(branch), cx)
        .await
        .unwrap();
    assert!(opened.created);
    let id: ReviewId = opened.loaded.review.id.clone();
    let session = cx.new(|cx| ReviewSession::new(opened, cx));
    wait(cx, &session, "the list", |session| !session.is_loading());
    session.read_with(cx, |session, _| {
        // The branch's files on disk where it is checked out, not its tip.
        assert_eq!(paths(session), ["linked.txt", "src/a.rs"]);
        assert_eq!(session.spec().unwrap().new, dv_core::Side::Worktree);
    });
    drop(session);
    // Its worktree gone, a working review has nowhere to open; the branch review is its tip.
    let linked_store = ReviewStore::new(Arc::new(
        LocalBackend::open_with(
            &linked,
            LocalBackendOptions {
                env: repo.env_overlay(),
            },
        )
        .unwrap(),
    ));
    let working = ops::open_review(&linked_store, Target::Working, reviewer())
        .unwrap()
        .review;
    repo.git(&[
        std::ffi::OsStr::new("worktree"),
        std::ffi::OsStr::new("remove"),
        std::ffi::OsStr::new("--force"),
        linked.as_os_str(),
    ]);
    let failure = open_from_main(&reviews, &repo, OpenRequest::Review(working.id), cx)
        .await
        .err()
        .expect("nowhere to open it");
    assert!(
        failure
            .message
            .ends_with("its worktree is gone (removed, moved or deleted)"),
        "{failure:?}"
    );
    assert!(
        failure.detail.as_deref().is_some_and(|detail| {
            detail.ends_with("is no longer one of the repository's worktrees")
        }),
        "{failure:?}"
    );
    let opened = open_from_main(&reviews, &repo, OpenRequest::Review(id), cx)
        .await
        .unwrap();
    assert!(dv_core::review::same_worktree(
        &opened.backend.repo().location,
        &opened.backend.repo().toplevel,
        &repo.path().display().to_string()
    ));
}
