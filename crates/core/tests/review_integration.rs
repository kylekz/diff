//! Integration tests for [`dv_core::ReviewStore`] and
//! [`dv_core::GitRepo::blob_sha`] against real `git` repos in a temp dir —
//! same style as `git_integration.rs` (no mocking).
//!
//! Deliberately no WSL-hitting tests here: CI has no WSL, and the
//! WSL-specific risk (shell-escaping paths for `sh -c`) is already covered
//! by the unit tests in `crates/core/src/review/io.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use dv_core::{BlobSpec, DiffSource, GitRepo, RepoLocation, ReviewStore, Side};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A throwaway repo under the system temp dir, built with real `git`
/// invocations. Mirrors `git_integration.rs`'s `TestRepo` — kept as a
/// separate copy rather than shared test-support code because neither
/// integration-test binary can depend on the other's `tests/` module.
struct TestRepo {
    dir: PathBuf,
}

impl TestRepo {
    fn new(name: &str) -> Self {
        let pid = std::process::id();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dv-review-test-{pid}-{n}-{name}"));
        std::fs::create_dir_all(&dir).expect("create test repo dir");

        let repo = Self { dir };
        repo.git(&["-c", "init.defaultBranch=main", "init"]);
        repo
    }

    fn path(&self) -> &Path {
        &self.dir
    }

    fn git(&self, args: &[&str]) -> String {
        let dir_str = self.dir.to_str().expect("temp dir path is not valid UTF-8");
        let mut full_args = vec![
            "-C",
            dir_str,
            "-c",
            "user.name=dv-test",
            "-c",
            "user.email=dv@test",
            "-c",
            "core.autocrlf=false",
            "-c",
            "commit.gpgsign=false",
        ];
        full_args.extend_from_slice(args);

        let output = Command::new("git")
            .args(&full_args)
            .output()
            .expect("failed to spawn git");
        if !output.status.success() {
            panic!(
                "git {:?} failed:\n{}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    }

    fn write(&self, rel_path: &str, bytes: &[u8]) {
        let full = self.dir.join(rel_path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir for fixture file");
        }
        std::fs::write(full, bytes).expect("write fixture file");
    }

    fn commit(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        // Best-effort: Windows file locking / AV scans can make this fail
        // sporadically, same caveat as `git_integration.rs`.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn open(repo: &TestRepo) -> GitRepo {
    GitRepo::open(RepoLocation::Local(repo.path().to_path_buf())).expect("open fixture repo")
}

#[test]
fn r1_create_save_list_load_round_trip() {
    let repo = TestRepo::new("r1");
    repo.write("a.txt", b"hello\n");
    repo.commit("seed");

    let git_repo = open(&repo);
    let store = ReviewStore::open(git_repo.location().clone());

    let mut review = store
        .create(DiffSource::WorkingTree)
        .expect("create review");
    review
        .add_comment("a.txt", Side::New, 1, 1, None, "why hello there", "kyle")
        .expect("add comment");
    store.save(&review).expect("save review");

    let listed = store.list().expect("list reviews");
    assert_eq!(listed.len(), 1, "expected exactly one review: {listed:?}");
    assert_eq!(listed[0].id, review.id);
    assert_eq!(listed[0].comments.len(), 1);
    assert_eq!(listed[0].comments[0].body, "why hello there");

    let loaded = store
        .load(&review.id)
        .expect("load review")
        .expect("review should exist");
    assert_eq!(loaded.id, review.id);
    assert_eq!(loaded.source, DiffSource::WorkingTree);
    assert_eq!(loaded.comments.len(), 1);

    // The store file must live under .git/, never in the worktree.
    let on_disk = repo
        .path()
        .join(".git")
        .join("dv")
        .join("reviews")
        .join(format!("{}.json", review.id));
    assert!(
        on_disk.is_file(),
        "expected review json at {}",
        on_disk.display()
    );
}

#[test]
fn r2_list_is_empty_when_no_reviews_dir() {
    let repo = TestRepo::new("r2");
    repo.write("a.txt", b"hello\n");
    repo.commit("seed");

    let git_repo = open(&repo);
    let store = ReviewStore::open(git_repo.location().clone());

    assert!(store.list().expect("list on empty store").is_empty());
    assert!(store.load("r-nonexistent").expect("load missing").is_none());
}

#[test]
fn r3_delete_removes_review() {
    let repo = TestRepo::new("r3");
    repo.write("a.txt", b"hello\n");
    repo.commit("seed");

    let git_repo = open(&repo);
    let store = ReviewStore::open(git_repo.location().clone());

    let review = store
        .create(DiffSource::WorkingTree)
        .expect("create review");
    assert!(store.load(&review.id).unwrap().is_some());

    store.delete(&review.id).expect("delete review");
    assert!(store.load(&review.id).unwrap().is_none());
    assert!(store.list().unwrap().is_empty());

    // Deleting an already-gone review is not an error.
    store.delete(&review.id).expect("delete is idempotent");
    store
        .delete("r-never-existed")
        .expect("delete of unknown id is not an error");
}

#[test]
fn r4_five_hundred_comments_save_and_load_under_bound() {
    let repo = TestRepo::new("r4");
    repo.write("a.txt", b"hello\n");
    repo.commit("seed");

    let git_repo = open(&repo);
    let store = ReviewStore::open(git_repo.location().clone());

    let mut review = store
        .create(DiffSource::WorkingTree)
        .expect("create review");
    for i in 0..500u32 {
        review
            .add_comment(
                format!("file{}.rs", i % 20),
                if i % 2 == 0 { Side::Old } else { Side::New },
                i + 1,
                i + 1,
                Some(format!("{i:040x}")),
                format!("comment body number {i} with a little more text to be realistic"),
                "agent",
            )
            .expect("add comment");
    }
    assert_eq!(review.comments.len(), 500);

    let save_start = Instant::now();
    store.save(&review).expect("save 500-comment review");
    let save_ms = save_start.elapsed().as_secs_f64() * 1000.0;

    let load_start = Instant::now();
    let loaded = store
        .load(&review.id)
        .expect("load 500-comment review")
        .expect("review should exist");
    let load_ms = load_start.elapsed().as_secs_f64() * 1000.0;

    assert_eq!(loaded.comments.len(), 500);

    println!("r4: 500-comment review save={save_ms:.3}ms load={load_ms:.3}ms (bound 250ms each)");
    // Generous bound so CI noise doesn't flake this; the acceptance target
    // in docs/phase-2-review-layer.md is 50ms, printed above so real
    // numbers are visible without loosening the assertion.
    assert!(
        save_ms < 250.0,
        "save took {save_ms:.3}ms, expected < 250ms"
    );
    assert!(
        load_ms < 250.0,
        "load took {load_ms:.3}ms, expected < 250ms"
    );
}

#[test]
fn r5_blob_sha_rev_index_working_and_missing() {
    let repo = TestRepo::new("r5");
    repo.write("a.txt", b"hello\n");
    repo.write("to-delete.txt", b"gone soon\n");
    let sha = repo.commit("seed");

    let git_repo = open(&repo);

    // Rev: existing path.
    let expected_blob_sha = repo.git(&["rev-parse", &format!("{sha}:a.txt")]);
    let got = git_repo
        .blob_sha(&BlobSpec::Rev {
            rev: sha.clone(),
            path: "a.txt".to_string(),
        })
        .unwrap();
    assert_eq!(got, Some(expected_blob_sha.clone()));

    // Rev: missing path -> None, not an error.
    let missing = git_repo
        .blob_sha(&BlobSpec::Rev {
            rev: sha.clone(),
            path: "does-not-exist.txt".to_string(),
        })
        .unwrap();
    assert_eq!(missing, None);

    // Rev: a genuinely bad rev is a real Err.
    let err = git_repo
        .blob_sha(&BlobSpec::Rev {
            rev: "not-a-rev-at-all".to_string(),
            path: "a.txt".to_string(),
        })
        .unwrap_err();
    assert!(!err.to_string().is_empty());

    // Index: stage the same content and check it matches the committed sha
    // (unchanged content -> same blob sha).
    let indexed = git_repo
        .blob_sha(&BlobSpec::Index {
            path: "a.txt".to_string(),
        })
        .unwrap();
    assert_eq!(indexed, Some(expected_blob_sha.clone()));

    // Index: untracked path -> None.
    repo.write("untracked.txt", b"never staged\n");
    let untracked = git_repo
        .blob_sha(&BlobSpec::Index {
            path: "untracked.txt".to_string(),
        })
        .unwrap();
    assert_eq!(untracked, None);

    // Working: hashing the working file must match `git hash-object`.
    let expected_working_sha = repo.git(&["hash-object", "a.txt"]);
    let working = git_repo
        .blob_sha(&BlobSpec::Working {
            path: "a.txt".to_string(),
        })
        .unwrap();
    assert_eq!(working, Some(expected_working_sha));

    // Working: modified content changes the sha vs. the committed blob.
    repo.write("a.txt", b"hello modified\n");
    let modified_working = git_repo
        .blob_sha(&BlobSpec::Working {
            path: "a.txt".to_string(),
        })
        .unwrap();
    assert_ne!(modified_working, Some(expected_blob_sha));

    // Working: missing file -> None.
    std::fs::remove_file(repo.path().join("to-delete.txt")).unwrap();
    let deleted = git_repo
        .blob_sha(&BlobSpec::Working {
            path: "to-delete.txt".to_string(),
        })
        .unwrap();
    assert_eq!(deleted, None);
}

#[test]
fn r6_linked_worktree_dot_git_is_a_file() {
    let repo = TestRepo::new("r6-main");
    repo.write("a.txt", b"hello\n");
    repo.commit("seed");

    // A linked worktree's `.git` is a file with a `gitdir:` pointer, not a
    // directory — exactly the layout the main dv tree itself uses for
    // agent worktrees (see CLAUDE.md). ReviewStore must resolve through
    // it to the real per-worktree git dir under the main repo's `.git/`.
    let worktree_dir =
        std::env::temp_dir().join(format!("dv-review-test-{}-r6-worktree", std::process::id()));
    let _ = std::fs::remove_dir_all(&worktree_dir);
    repo.git(&[
        "worktree",
        "add",
        worktree_dir.to_str().unwrap(),
        "-b",
        "r6-branch",
    ]);

    let dot_git = worktree_dir.join(".git");
    assert!(
        dot_git.is_file(),
        ".git should be a file in a linked worktree"
    );

    let wt_repo =
        GitRepo::open(RepoLocation::Local(worktree_dir.clone())).expect("open worktree repo");
    let store = ReviewStore::open(wt_repo.location().clone());

    let review = store
        .create(DiffSource::WorkingTree)
        .expect("create review in linked worktree");
    let loaded = store
        .load(&review.id)
        .expect("load review in linked worktree")
        .expect("review should exist");
    assert_eq!(loaded.id, review.id);

    // The json must land under the *main* repo's .git/worktrees/<name>/dv,
    // not a nonexistent .git/dv inside the worktree directory itself.
    assert!(
        !worktree_dir.join(".git").join("dv").exists(),
        "the review store must not create a dv/ dir where .git is a gitlink file"
    );

    let _ = std::fs::remove_dir_all(&worktree_dir);
    repo.git(&["worktree", "prune"]);
}

#[test]
fn r7_delete_of_unknown_review_when_store_dir_absent_is_ok() {
    let repo = TestRepo::new("r7");
    repo.write("a.txt", b"hello\n");
    repo.commit("seed");

    let git_repo = open(&repo);
    let store = ReviewStore::open(git_repo.location().clone());

    // No review has ever been created, so `.git/dv/` doesn't exist yet.
    store
        .delete("r-whatever")
        .expect("delete against an absent store dir is not an error");
}

/// Durable-concurrency slice (docs/backlog.md: "Review store: durable
/// concurrency answer ... the real fix is a lock file around
/// load-mutate-save", Phase-2 review P1 residual): two threads hammering
/// interleaved `comment add`-shaped load-mutate-save cycles against ONE
/// review, through the real public `ReviewStore`/`Review` API (not the
/// lower-level `StoreIo` primitive `crates/core/src/review/lock.rs`'s own
/// unit tests exercise) — the actual shape every real call site (CLI
/// `cmd_comment_add`, the GUI's `submit_comment`) uses. Asserts zero lost
/// updates across 2 × 50 = 100 comments.
#[test]
fn r9_with_lock_contention_two_threads_hammering_comment_adds() {
    let repo = TestRepo::new("r9");
    repo.write("a.txt", b"hello\n");
    repo.commit("seed");

    let git_repo = open(&repo);
    let store = std::sync::Arc::new(ReviewStore::open(git_repo.location().clone()));
    let review_id = store
        .create(DiffSource::WorkingTree)
        .expect("create draft")
        .id;

    const PER_THREAD: usize = 50;
    let mut handles = Vec::new();
    for t in 0..2 {
        let store = std::sync::Arc::clone(&store);
        let review_id = review_id.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..PER_THREAD {
                store
                    .with_lock(|| -> anyhow::Result<()> {
                        let mut review = store
                            .load(&review_id)?
                            .expect("review must still exist mid-test");
                        review.add_comment(
                            "a.txt",
                            Side::New,
                            1,
                            1,
                            None,
                            format!("t{t}-c{i}"),
                            format!("worker-{t}"),
                        )?;
                        store.save(&review)?;
                        Ok(())
                    })
                    .expect("with_lock critical section");
            }
        }));
    }
    for h in handles {
        h.join().expect("worker thread panicked");
    }

    let review = store
        .load(&review_id)
        .expect("final load")
        .expect("review still exists");
    assert_eq!(
        review.comments.len(),
        2 * PER_THREAD,
        "every comment from both threads must have landed — got {}",
        review.comments.len()
    );
    let unique_ids: std::collections::HashSet<_> =
        review.comments.iter().map(|c| c.id.clone()).collect();
    assert_eq!(
        unique_ids.len(),
        2 * PER_THREAD,
        "every comment id must be unique — no overwritten/duplicated entries"
    );
}

/// Exercises the real WSL branch of StoreIo (sh -c write pipeline, cat
/// read, ls list, rm delete) against an actual distro. Requires WSL with
/// an Ubuntu distro and a git repo at ~/zed-perf — run manually:
/// `cargo test -p dv-core --test review_integration wsl_ -- --ignored`
#[test]
#[ignore = "requires WSL Ubuntu with a repo at ~/zed-perf"]
fn wsl_store_round_trip() {
    let location = RepoLocation::Wsl {
        distro: "Ubuntu".to_string(),
        path: "/home/kyle/zed-perf".to_string(),
    };
    let store = ReviewStore::open(location);

    let mut review = store.create(DiffSource::WorkingTree).expect("create");
    review
        .add_comment(
            "crates/gpui/src/window.rs",
            Side::New,
            10,
            12,
            None,
            "wsl smoke test — path with 'quotes' & spaces",
            "smoke-test",
        )
        .expect("add comment");
    store.save(&review).expect("save over WSL");

    let listed = store.list().expect("list over WSL");
    assert!(listed.iter().any(|r| r.id == review.id), "review listed");

    let loaded = store
        .load(&review.id)
        .expect("load over WSL")
        .expect("review exists");
    assert_eq!(loaded.comments.len(), 1);
    assert_eq!(loaded.comments[0].body, review.comments[0].body);

    store.delete(&review.id).expect("delete over WSL");
    assert!(store.load(&review.id).expect("reload").is_none());
}

/// Phase-2 acceptance: comment anchors are blob shas, so an amend that
/// doesn't touch the commented file leaves the anchor intact, while one
/// that rewrites the commented content makes the stored sha diverge — the
/// signal the GUI renders as a "stale" badge.
#[test]
fn r8_anchor_survives_unrelated_amend_and_flags_content_drift() {
    let repo = TestRepo::new("r8");
    repo.write("file.txt", b"line one\nline two\n");
    repo.commit("base");

    let git = open(&repo);
    let spec = BlobSpec::Rev {
        rev: "HEAD".to_string(),
        path: "file.txt".to_string(),
    };
    let anchored = git
        .blob_sha(&spec)
        .expect("blob_sha")
        .expect("file.txt exists at HEAD");

    // Amend that does NOT touch the commented file → anchor intact.
    repo.write("other.txt", b"unrelated\n");
    repo.git(&["add", "other.txt"]);
    repo.git(&["commit", "--amend", "--no-edit"]);
    assert_eq!(
        git.blob_sha(&spec).expect("blob_sha").as_deref(),
        Some(anchored.as_str()),
        "an amend not touching the file must keep the anchor"
    );

    // Amend that rewrites the commented content → anchor drifts (stale).
    repo.write("file.txt", b"line one CHANGED\nline two\n");
    repo.git(&["add", "file.txt"]);
    repo.git(&["commit", "--amend", "--no-edit"]);
    assert_ne!(
        git.blob_sha(&spec).expect("blob_sha").as_deref(),
        Some(anchored.as_str()),
        "an amend rewriting the file must change the anchor"
    );
}
