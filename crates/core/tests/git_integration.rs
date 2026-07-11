//! Integration tests against a real `git` binary and real temp-dir
//! repositories. No mocking: these exercise [`dv_core::GitRepo`] the same
//! way the app will.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use dv_core::{BlobSpec, ChangeStatus, DiffSource, GitRepo, RepoLocation};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A throwaway repo under the system temp dir, built with real `git`
/// invocations. Config flags are fixed so commits are deterministic and
/// never touch the developer's real git identity or GPG setup.
struct TestRepo {
    dir: PathBuf,
}

impl TestRepo {
    fn new(name: &str) -> Self {
        let pid = std::process::id();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dv-test-{pid}-{n}-{name}"));
        std::fs::create_dir_all(&dir).expect("create test repo dir");

        let repo = Self { dir };
        repo.git(&["-c", "init.defaultBranch=main", "init"]);
        repo
    }

    fn path(&self) -> &Path {
        &self.dir
    }

    /// Run a git command scoped to this fixture repo. Panics (with stderr)
    /// on failure — this is test scaffolding, not code under test.
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

    /// Stage everything and commit. Returns the new commit's full SHA.
    fn commit(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        // Windows file locking (the batch `git cat-file` child, antivirus
        // scans, …) can make this fail sporadically; best-effort only.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn open(repo: &TestRepo) -> GitRepo {
    GitRepo::open(RepoLocation::Local(repo.path().to_path_buf())).expect("open fixture repo")
}

#[test]
fn t1_open_normalizes_root_and_rejects_non_repo() {
    let repo = TestRepo::new("t1");
    repo.write("a.txt", b"hello\n");
    repo.commit("seed");
    repo.write("sub/nested.txt", b"nested\n");

    let expected_toplevel = repo.git(&["rev-parse", "--show-toplevel"]);

    let opened = GitRepo::open(RepoLocation::Local(repo.path().join("sub")))
        .expect("open from a subdirectory of the work tree");
    match opened.location() {
        RepoLocation::Local(path) => {
            assert_eq!(path.display().to_string(), expected_toplevel);
        }
        other => panic!("expected Local location, got {other:?}"),
    }

    let non_repo = std::env::temp_dir().join(format!("dv-test-non-repo-{}", std::process::id()));
    std::fs::create_dir_all(&non_repo).expect("create non-repo dir");
    let err = match GitRepo::open(RepoLocation::Local(non_repo.clone())) {
        Ok(_) => panic!("expected non-repo path to fail to open"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("not a git repository"),
        "unexpected error message: {err}"
    );
    let _ = std::fs::remove_dir_all(&non_repo);
}

#[test]
fn t2_working_tree_status_set() {
    let repo = TestRepo::new("t2");
    repo.write("a.txt", b"one\n");
    repo.write("b.txt", b"two\n");
    repo.commit("seed");

    repo.write("a.txt", b"one modified\n");
    std::fs::remove_file(repo.path().join("b.txt")).expect("remove b.txt");
    repo.write("d.txt", b"new\n");
    // `git diff HEAD` (no `--cached`) does pick up unstaged modifications
    // and deletions of already-tracked files, but a brand-new path is
    // invisible to `git diff` itself until it's staged — stage it here so
    // this test isolates the base `diff HEAD --name-status -z -M` behavior.
    // The untracked-file merge (`git ls-files --others`) that also feeds
    // `WorkingTree` is covered separately by t12.
    repo.git(&["add", "d.txt"]);

    let git_repo = open(&repo);
    let files = git_repo.changed_files(&DiffSource::WorkingTree).unwrap();

    let mut got: Vec<(String, ChangeStatus)> =
        files.into_iter().map(|f| (f.path, f.status)).collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));

    let mut expected = vec![
        ("a.txt".to_string(), ChangeStatus::Modified),
        ("b.txt".to_string(), ChangeStatus::Deleted),
        ("d.txt".to_string(), ChangeStatus::Added),
    ];
    expected.sort_by(|a, b| a.0.cmp(&b.0));

    assert_eq!(got, expected);
}

#[test]
fn t3_staged_rename() {
    let repo = TestRepo::new("t3");
    repo.write(
        "a.txt",
        b"content long enough for git to consider this a rename\n",
    );
    repo.commit("seed");
    repo.git(&["mv", "a.txt", "renamed.txt"]);

    let git_repo = open(&repo);
    let files = git_repo.changed_files(&DiffSource::Staged).unwrap();

    assert_eq!(
        files.len(),
        1,
        "expected exactly one staged change: {files:?}"
    );
    let f = &files[0];
    assert_eq!(f.status, ChangeStatus::Renamed);
    assert_eq!(f.path, "renamed.txt");
    assert_eq!(f.old_path.as_deref(), Some("a.txt"));
}

#[test]
fn t4_range_and_merge_base() {
    let repo = TestRepo::new("t4");
    repo.write("base.txt", b"base\n");
    let c1 = repo.commit("c1");

    repo.write("feature.txt", b"feature change\n");
    let c2 = repo.commit("c2");

    let git_repo = open(&repo);

    let files = git_repo
        .changed_files(&DiffSource::Range {
            base: c1.clone(),
            head: c2.clone(),
            merge_base: false,
        })
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "feature.txt");
    assert_eq!(files[0].status, ChangeStatus::Added);

    // Diverge: a branch off c1 with its own commit, and an extra commit on
    // main. `main...branch` (merge-base) must only surface branch's change.
    repo.git(&["checkout", "-b", "branch", &c1]);
    repo.write("branch-only.txt", b"branch only\n");
    repo.commit("branch commit");

    repo.git(&["checkout", "main"]);
    repo.write("main-only.txt", b"main only\n");
    repo.commit("main-only commit");

    let files = git_repo
        .changed_files(&DiffSource::Range {
            base: "main".to_string(),
            head: "branch".to_string(),
            merge_base: true,
        })
        .unwrap();
    assert_eq!(
        files.len(),
        1,
        "expected only branch's own change: {files:?}"
    );
    assert_eq!(files[0].path, "branch-only.txt");
    assert_eq!(files[0].status, ChangeStatus::Added);
}

#[test]
fn t5_commit_source() {
    let repo = TestRepo::new("t5");
    repo.write("a.txt", b"a\n");
    repo.write("b.txt", b"b\n");
    let root_sha = repo.commit("root");

    repo.write("c.txt", b"c\n");
    let second_sha = repo.commit("second");

    let git_repo = open(&repo);

    let root_files = git_repo
        .changed_files(&DiffSource::Commit(root_sha))
        .unwrap();
    let mut root_paths: Vec<String> = root_files.iter().map(|f| f.path.clone()).collect();
    root_paths.sort();
    assert_eq!(root_paths, vec!["a.txt".to_string(), "b.txt".to_string()]);
    assert!(
        root_files.iter().all(|f| f.status == ChangeStatus::Added),
        "root commit files: {root_files:?}"
    );

    let second_files = git_repo
        .changed_files(&DiffSource::Commit(second_sha))
        .unwrap();
    assert_eq!(second_files.len(), 1);
    assert_eq!(second_files[0].path, "c.txt");
    assert_eq!(second_files[0].status, ChangeStatus::Added);
}

#[test]
fn t6_blob_bytes() {
    let repo = TestRepo::new("t6");
    let binary_content: Vec<u8> = vec![0, 1, 2, 3, 0, 255, 0, 10, 0];
    repo.write("bin.dat", &binary_content);
    repo.write("text.txt", b"hello\n");
    repo.write("to-delete.txt", b"gone soon\n");
    let sha = repo.commit("seed");

    let git_repo = open(&repo);

    let got = git_repo
        .blob_bytes(&BlobSpec::Rev {
            rev: sha.clone(),
            path: "bin.dat".to_string(),
        })
        .unwrap();
    assert_eq!(got, Some(binary_content));

    let missing = git_repo
        .blob_bytes(&BlobSpec::Rev {
            rev: sha.clone(),
            path: "does-not-exist.txt".to_string(),
        })
        .unwrap();
    assert_eq!(missing, None);

    repo.write("text.txt", b"hello modified\n");
    let working = git_repo
        .blob_bytes(&BlobSpec::Working {
            path: "text.txt".to_string(),
        })
        .unwrap();
    assert_eq!(working, Some(b"hello modified\n".to_vec()));

    std::fs::remove_file(repo.path().join("to-delete.txt")).unwrap();
    let deleted = git_repo
        .blob_bytes(&BlobSpec::Working {
            path: "to-delete.txt".to_string(),
        })
        .unwrap();
    assert_eq!(deleted, None);

    // Stage new content, then change the worktree again — Index must
    // return what was staged, not the newer working-tree content.
    repo.write("text.txt", b"staged version\n");
    repo.git(&["add", "text.txt"]);
    repo.write("text.txt", b"working version after staging\n");
    let indexed = git_repo
        .blob_bytes(&BlobSpec::Index {
            path: "text.txt".to_string(),
        })
        .unwrap();
    assert_eq!(indexed, Some(b"staged version\n".to_vec()));
}

#[test]
fn t7_head_label() {
    let repo = TestRepo::new("t7");
    repo.write("a.txt", b"a\n");
    repo.commit("seed");

    let git_repo = open(&repo);
    assert_eq!(git_repo.head_label().unwrap(), "main");

    repo.git(&["checkout", "--detach"]);
    let label = git_repo.head_label().unwrap();
    assert!(label.len() >= 7, "detached label too short: {label:?}");
    assert!(
        label.chars().all(|c| c.is_ascii_hexdigit()),
        "detached label not hex: {label:?}"
    );
}

#[test]
fn t8_resolve() {
    let repo = TestRepo::new("t8");
    repo.write("a.txt", b"a\n");
    repo.commit("seed");

    let git_repo = open(&repo);
    let sha = git_repo.resolve("HEAD").unwrap();
    assert_eq!(sha.len(), 40, "sha {sha:?} is not 40 hex chars");
    assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));

    let err = git_repo.resolve("does-not-exist").unwrap_err();
    assert!(
        err.to_string().contains("does-not-exist"),
        "error should name the rev: {err}"
    );
}

#[test]
fn t9_unicode_and_spaces() {
    let repo = TestRepo::new("t9");
    let rel_path = "sp ace/\u{fc}n\u{ef}code.txt"; // "sp ace/ünïcode.txt"
    let content = "hello \u{fc}n\u{ef}code\n".as_bytes().to_vec();
    repo.write(rel_path, &content);
    let sha = repo.commit("unicode file");

    let git_repo = open(&repo);
    let files = git_repo
        .changed_files(&DiffSource::Commit(sha.clone()))
        .unwrap();
    assert_eq!(files.len(), 1, "files: {files:?}");
    assert_eq!(files[0].path, rel_path);

    let got = git_repo
        .blob_bytes(&BlobSpec::Rev {
            rev: sha,
            path: rel_path.to_string(),
        })
        .unwrap();
    assert_eq!(got, Some(content));
}

#[test]
fn t10_batch_process_reuse_and_large_blob() {
    let repo = TestRepo::new("t10");
    let mut expected = Vec::new();
    for i in 0..50 {
        let name = format!("file{i}.txt");
        let content = format!("content number {i}\n");
        repo.write(&name, content.as_bytes());
        expected.push((name, content));
    }
    let large_content: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    repo.write("large.bin", &large_content);

    let sha = repo.commit("many files");
    let git_repo = open(&repo);

    for (name, content) in &expected {
        let got = git_repo
            .blob_bytes(&BlobSpec::Rev {
                rev: sha.clone(),
                path: name.clone(),
            })
            .unwrap();
        assert_eq!(
            got.as_deref(),
            Some(content.as_bytes()),
            "mismatch for {name}"
        );
    }

    let got_large = git_repo
        .blob_bytes(&BlobSpec::Rev {
            rev: sha,
            path: "large.bin".to_string(),
        })
        .unwrap();
    assert_eq!(got_large, Some(large_content));
}

// A merge commit as DiffSource::Commit must diff against its FIRST parent
// (the single-arg diff-tree form silently prints nothing for merges).
#[test]
fn t11_merge_commit_diffs_against_first_parent() {
    let repo = TestRepo::new("merge-commit");
    repo.write("a.txt", b"a\n");
    repo.commit("root");

    repo.git(&["switch", "-c", "side"]);
    repo.write("side.txt", b"s\n");
    repo.commit("side");

    repo.git(&["switch", "main"]);
    repo.write("main.txt", b"m\n");
    repo.commit("mainline");

    repo.git(&["merge", "--no-ff", "side", "-m", "merge"]);
    let merge_sha = repo.git(&["rev-parse", "HEAD"]);

    let git_repo = GitRepo::open(RepoLocation::Local(repo.dir.clone())).unwrap();
    let files = git_repo
        .changed_files(&DiffSource::Commit(merge_sha))
        .unwrap();

    // Only the side branch's file: the merge vs its first parent.
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "side.txt");
    assert_eq!(files[0].status, ChangeStatus::Added);
}

#[test]
fn t12_untracked_files_in_working_tree() {
    let repo = TestRepo::new("t12");
    repo.write("a.txt", b"a\n");
    repo.write(".gitignore", b"ignored.txt\n");
    repo.commit("seed");

    repo.write("b.txt", b"untracked content\n");
    repo.write("ignored.txt", b"should not appear\n");

    let git_repo = open(&repo);
    let files = git_repo.changed_files(&DiffSource::WorkingTree).unwrap();

    assert!(
        files
            .iter()
            .any(|f| f.path == "b.txt" && f.status == ChangeStatus::Added && f.old_path.is_none()),
        "expected untracked b.txt as Added: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.path == "ignored.txt"),
        "ignored.txt should be excluded by .gitignore: {files:?}"
    );

    let content = git_repo
        .blob_bytes(&BlobSpec::Working {
            path: "b.txt".to_string(),
        })
        .unwrap();
    assert_eq!(content, Some(b"untracked content\n".to_vec()));
}

#[test]
fn t13_unborn_head() {
    let repo = TestRepo::new("t13");
    // No commits yet: HEAD does not resolve.
    repo.write("a.txt", b"a\n");

    let git_repo = open(&repo);

    let working = git_repo.changed_files(&DiffSource::WorkingTree).unwrap();
    assert_eq!(working.len(), 1, "working: {working:?}");
    assert_eq!(working[0].path, "a.txt");
    assert_eq!(working[0].status, ChangeStatus::Added);

    repo.git(&["add", "a.txt"]);

    let staged = git_repo.changed_files(&DiffSource::Staged).unwrap();
    assert_eq!(staged.len(), 1, "staged: {staged:?}");
    assert_eq!(staged[0].path, "a.txt");
    assert_eq!(staged[0].status, ChangeStatus::Added);

    // Once staged, a.txt is no longer "untracked" (git ls-files --others),
    // so it must surface exactly once from the diff itself — no duplicate
    // from the untracked-file merge.
    let working_after_stage = git_repo.changed_files(&DiffSource::WorkingTree).unwrap();
    assert_eq!(
        working_after_stage.len(),
        1,
        "no duplicate once staged: {working_after_stage:?}"
    );
    assert_eq!(working_after_stage[0].path, "a.txt");
    assert_eq!(working_after_stage[0].status, ChangeStatus::Added);

    assert_eq!(git_repo.head_label().unwrap(), "main");
}

#[test]
fn t14_merge_base() {
    let repo = TestRepo::new("t14");
    repo.write("base.txt", b"base\n");
    let c1 = repo.commit("c1");

    repo.git(&["checkout", "-b", "branch", &c1]);
    repo.write("branch-only.txt", b"branch only\n");
    repo.commit("branch commit");

    repo.git(&["checkout", "main"]);
    repo.write("main-only.txt", b"main only\n");
    repo.commit("main-only commit");

    let git_repo = open(&repo);

    let base = git_repo.merge_base("main", "branch").unwrap();
    assert_eq!(base, c1, "merge base should be c1's sha");
    assert_eq!(base.len(), 40, "sha {base:?} is not 40 hex chars");
    assert!(base.chars().all(|c| c.is_ascii_hexdigit()));

    let err = git_repo.merge_base("main", "nonexistent").unwrap_err();
    assert!(
        err.to_string().contains("nonexistent"),
        "error should name the rev: {err}"
    );
}

/// `current_branch`/`default_branch`/`config` back `dv pr create`'s
/// detached-HEAD and default-branch guards and `resolve_author`
/// (crates/app/src/author.rs) — phase 3 additions.
#[test]
fn t15_current_branch_named_and_detached() {
    let repo = TestRepo::new("t15");
    repo.write("a.txt", b"hello\n");
    let sha = repo.commit("seed");
    let git_repo = open(&repo);

    assert_eq!(git_repo.current_branch().unwrap(), Some("main".to_string()));

    repo.git(&["checkout", "--detach", &sha]);
    assert_eq!(git_repo.current_branch().unwrap(), None);
}

#[test]
fn t16_default_branch_reads_origin_head_from_a_real_clone() {
    let origin = TestRepo::new("t16-origin");
    origin.write("a.txt", b"hello\n");
    origin.commit("seed");

    let clone_dir = std::env::temp_dir().join(format!("dv-test-{}-t16-clone", std::process::id()));
    let _ = std::fs::remove_dir_all(&clone_dir);
    let output = Command::new("git")
        .args([
            "clone",
            origin.path().to_str().unwrap(),
            clone_dir.to_str().unwrap(),
        ])
        .output()
        .expect("failed to spawn git clone");
    assert!(
        output.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let clone_repo =
        GitRepo::open(RepoLocation::Local(clone_dir.clone())).expect("open cloned repo");
    assert_eq!(clone_repo.default_branch(), Some("main".to_string()));

    let _ = std::fs::remove_dir_all(&clone_dir);
}

#[test]
fn t16b_default_branch_is_none_without_an_origin_remote() {
    let repo = TestRepo::new("t16b");
    repo.write("a.txt", b"hello\n");
    repo.commit("seed");
    let git_repo = open(&repo);

    assert_eq!(git_repo.default_branch(), None);
}

#[test]
fn t17_config_get_and_missing_key() {
    let repo = TestRepo::new("t17");
    repo.write("a.txt", b"hi\n");
    repo.commit("seed");
    let git_repo = open(&repo);

    assert_eq!(git_repo.config("dv.author"), None);

    repo.git(&["config", "dv.author", "kyle"]);
    assert_eq!(git_repo.config("dv.author"), Some("kyle".to_string()));
}
