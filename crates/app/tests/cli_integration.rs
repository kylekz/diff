//! End-to-end tests for `dv review`/`dv comment`
//! (docs/phase-2-review-layer.md § Agent CLI): drive the COMPILED BINARY
//! (`env!("CARGO_BIN_EXE_dv")`) against a real git fixture repo, exactly the
//! way a headless agent would.
//!
//! `TestRepo` mirrors `crates/core/tests/review_integration.rs`'s helper of
//! the same name — kept as its own copy rather than shared code because
//! neither integration-test binary can depend on the other's `tests/`
//! module (and this one additionally needs to spawn the built `dv` binary,
//! not just call `dv_core` in-process).
//!
//! IMPORTANT (CLAUDE.md § automation): every invocation below passes
//! `--repo <fixture>` explicitly and never relies on cwd.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestRepo {
    dir: PathBuf,
}

impl TestRepo {
    fn new(name: &str) -> Self {
        let pid = std::process::id();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dv-cli-test-{pid}-{n}-{name}"));
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
        // sporadically, same caveat as `review_integration.rs`.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn seed_repo(name: &str) -> TestRepo {
    let repo = TestRepo::new(name);
    repo.write("a.rs", b"fn main() {\n    println!(\"hi\");\n}\n");
    repo.commit("seed");
    repo
}

/// Run the compiled `dv` binary with `args`, always scoped to `repo` via
/// `--repo` (CLAUDE.md § automation: never rely on cwd).
///
/// `--repo` is appended *after* `args` rather than before: `main.rs` only
/// branches into the headless CLI path when argv[1] (the very first token)
/// is literally `"review"` or `"comment"`, so the subcommand name must lead
/// — global flags are accepted anywhere after it, not before it.
fn dv(repo: &TestRepo, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_dv");
    let mut full_args: Vec<&str> = args.to_vec();
    full_args.push("--repo");
    full_args.push(repo.path().to_str().expect("repo path is UTF-8"));
    Command::new(bin)
        .args(&full_args)
        .output()
        .expect("failed to run dv binary")
}

fn stdout_json(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&text).unwrap_or_else(|err| {
        panic!(
            "stdout was not a single JSON document: {err}\nstdout: {text:?}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn full_review_and_comment_lifecycle() {
    let repo = seed_repo("lifecycle");
    // `dv.author` pins the author-resolution chain (crates/app/src/author.rs)
    // to a deterministic value for this test — the machine's real git
    // identity or a cached `gh` login must not leak into asserted output.
    repo.git(&["config", "dv.author", "test-author"]);

    // review create
    let out = dv(&repo, &["review", "create", "--json"]);
    assert!(
        out.status.success(),
        "review create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = stdout_json(&out);
    let review_id = json["review"]["id"]
        .as_str()
        .expect("review.id present")
        .to_string();
    assert_eq!(json["review"]["state"], "draft");
    // DiffSource has no serde rename attributes, so unit variants serialize
    // as their bare Rust names (see dv_core::git::DiffSource).
    assert_eq!(json["review"]["source"], "WorkingTree");

    // comment add — verify json shape, exit 0, and a real blob_sha.
    let out = dv(
        &repo,
        &[
            "comment",
            "add",
            "--file",
            "a.rs",
            "--lines",
            "2",
            "--body",
            "why print?",
            "--review",
            &review_id,
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "comment add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = stdout_json(&out);
    assert_eq!(json["review_id"], review_id);
    assert_eq!(json["review_created"], false);
    let comment_id = json["comment"]["id"]
        .as_str()
        .expect("comment.id present")
        .to_string();
    assert!(
        json["comment"]["blob_sha"].is_string(),
        "expected a real blob_sha (a.rs exists on the new/working side): {json}"
    );
    assert_eq!(json["comment"]["path"], "a.rs");
    assert_eq!(json["comment"]["status"], "open");

    // comment list --status open
    let out = dv(&repo, &["comment", "list", "--status", "open", "--json"]);
    assert!(out.status.success());
    let json = stdout_json(&out);
    let comments = json["comments"].as_array().expect("comments array");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["comment"]["id"], comment_id);
    assert_eq!(comments[0]["review_id"], review_id);

    // reply — returns the whole updated comment, not just the reply.
    let out = dv(
        &repo,
        &[
            "comment",
            "reply",
            &comment_id,
            "--body",
            "good question",
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "reply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = stdout_json(&out);
    assert_eq!(json["review_created"], false);
    let replies = json["comment"]["replies"]
        .as_array()
        .expect("replies array");
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["body"], "good question");
    assert_eq!(replies[0]["author"], "test-author");

    // resolve
    let out = dv(&repo, &["comment", "resolve", &comment_id, "--json"]);
    assert!(out.status.success());
    let json = stdout_json(&out);
    assert_eq!(json["review_id"], review_id);
    assert_eq!(json["comment_id"], comment_id);
    assert_eq!(json["status"], "resolved");

    // comment list --status open is now empty
    let out = dv(&repo, &["comment", "list", "--status", "open", "--json"]);
    assert!(out.status.success());
    let json = stdout_json(&out);
    assert_eq!(json["comments"].as_array().unwrap().len(), 0);

    // review show
    let out = dv(&repo, &["review", "show", &review_id, "--json"]);
    assert!(out.status.success());
    let json = stdout_json(&out);
    let review_comments = json["review"]["comments"].as_array().unwrap();
    assert_eq!(review_comments.len(), 1);
    assert_eq!(review_comments[0]["status"], "resolved");

    // review delete
    let out = dv(&repo, &["review", "delete", &review_id, "--json"]);
    assert!(out.status.success());
    let json = stdout_json(&out);
    assert_eq!(json["review_id"], review_id);
    assert_eq!(json["deleted"], true);

    // review show now fails: no such review (exit 1, error json).
    let out = dv(&repo, &["review", "show", &review_id, "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let json = stdout_json(&out);
    assert!(json["error"].as_str().is_some());
}

#[test]
fn comment_add_with_no_review_auto_creates_draft() {
    let repo = seed_repo("autocreate");

    let out = dv(
        &repo,
        &[
            "comment", "add", "--file", "a.rs", "--lines", "1", "--body", "hi", "--json",
        ],
    );
    assert!(
        out.status.success(),
        "comment add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json = stdout_json(&out);
    assert_eq!(json["review_created"], true);
    assert!(json["review_id"].as_str().is_some());
    assert!(json["comment"]["id"].as_str().is_some());

    // A second comment add with no --review reuses the same draft rather
    // than creating another one.
    let out = dv(
        &repo,
        &[
            "comment", "add", "--file", "a.rs", "--lines", "2", "--body", "again", "--json",
        ],
    );
    assert!(out.status.success());
    let second = stdout_json(&out);
    assert_eq!(second["review_created"], false);
    assert_eq!(second["review_id"], json["review_id"].clone());
}

#[test]
fn unknown_comment_id_is_operation_error() {
    let repo = seed_repo("unknown-comment");

    let out = dv(&repo, &["comment", "resolve", "c-does-not-exist", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let json = stdout_json(&out);
    assert!(json["error"].as_str().unwrap().contains("c-does-not-exist"));
}

#[test]
fn bad_lines_is_usage_error() {
    let repo = seed_repo("bad-lines");

    let out = dv(
        &repo,
        &[
            "comment", "add", "--file", "a.rs", "--lines", "20:10", "--body", "x", "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    let json = stdout_json(&out);
    assert!(json["error"].as_str().is_some());
}

#[test]
fn review_list_and_comment_list_are_empty_on_a_fresh_repo() {
    let repo = seed_repo("empty");

    let out = dv(&repo, &["review", "list", "--json"]);
    assert!(out.status.success());
    let json = stdout_json(&out);
    assert_eq!(json["reviews"].as_array().unwrap().len(), 0);

    let out = dv(&repo, &["comment", "list", "--json"]);
    assert!(out.status.success());
    let json = stdout_json(&out);
    assert_eq!(json["comments"].as_array().unwrap().len(), 0);
}
