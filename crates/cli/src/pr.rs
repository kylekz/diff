//! `prepare_pr`: given a GitHub PR's metadata, make sure its commits exist
//! locally and compute the diff range dv uses everywhere else — the one
//! function both `dv pr fetch`/`dv review submit` (`crates/cli/src/pr_cmd.rs`)
//! and the GUI's "open PR" flow (`crates/app/src/workspace.rs`, via
//! `dv_cli::pr::prepare_pr`) share, per docs/phase-3-github.md deliverable
//! 2 (crate-boundary move: Phase 8 S8b).

use anyhow::{Result, anyhow};
use dv_core::{GitRepo, PrMeta};

/// The concrete local range a PR maps to, once its blobs are confirmed
/// present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRange {
    pub number: u64,
    pub base_oid: String,
    pub head_oid: String,
    pub merge_base: String,
    /// `<merge_base>..<head_oid>` — GitHub's PR "Files changed" view is
    /// effectively a merge-base diff, matching what every other dv diff
    /// (`DiffSource::Range { merge_base: true, .. }`) resolves to as well.
    pub range: String,
}

/// Fetch whatever's missing — `meta.head_oid` via `pull/<n>/head`,
/// `meta.base_oid` via the base branch's ref (qualified `refs/heads/...` so
/// a same-named tag can't shadow the branch), each skipped when
/// [`GitRepo::has_object`] says the object is already local — then
/// compute the merge-base range.
///
/// Verifies both oids actually landed after fetching: `meta` can be stale
/// (read before a force-push moved the PR's head, or before the base branch
/// was rewritten), in which case the fetch itself succeeds but never
/// produces the oid `meta` named, and a cryptic merge-base failure is a
/// worse failure mode than saying so directly for either side.
pub fn prepare_pr(repo: &GitRepo, meta: &PrMeta) -> Result<PrRange> {
    if !repo.has_object(&meta.head_oid) {
        repo.fetch_pr_head(meta.number)?;
    }
    if !repo.has_object(&meta.base_oid) {
        repo.fetch_ref(&format!("refs/heads/{}", meta.base_ref))?;
    }
    if !repo.has_object(&meta.head_oid) {
        let short = meta.head_oid.get(..8).unwrap_or(&meta.head_oid);
        return Err(anyhow!(
            "PR head {short} not found after fetch — the PR may have been \
             force-pushed; re-open it to refresh"
        ));
    }
    if !repo.has_object(&meta.base_oid) {
        let short = meta.base_oid.get(..8).unwrap_or(&meta.base_oid);
        return Err(anyhow!(
            "PR base {short} (branch {}) not found after fetch — the PR's base may have \
             changed; re-open it to refresh",
            meta.base_ref
        ));
    }
    let merge_base = repo.merge_base(&meta.base_oid, &meta.head_oid)?;
    let range = format!("{merge_base}..{}", meta.head_oid);
    Ok(PrRange {
        number: meta.number,
        base_oid: meta.base_oid.clone(),
        head_oid: meta.head_oid.clone(),
        merge_base,
        range,
    })
}

#[cfg(test)]
mod tests {
    //! `prepare_pr` needs a real repo with a real `origin` remote to
    //! exercise the fetch/skip-fetch branches meaningfully, so this is an
    //! integration-style test (real `git`, temp dirs) rather than a unit
    //! test — same tradeoff `crates/core/tests/git_integration.rs` makes
    //! for the git layer it builds on.

    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use dv_core::{ChecksSummary, GitRepo, PrState, RepoLocation};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestRepo {
        dir: PathBuf,
    }

    impl TestRepo {
        fn new(name: &str) -> Self {
            let pid = std::process::id();
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("dv-pr-test-{pid}-{n}-{name}"));
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
                std::fs::create_dir_all(parent).expect("create parent dir");
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
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn dummy_meta(number: u64, base_ref: &str, base_oid: &str, head_oid: &str) -> PrMeta {
        PrMeta {
            number,
            title: "test PR".to_string(),
            body: String::new(),
            url: format!("https://example.invalid/pull/{number}"),
            state: PrState::Open,
            is_draft: false,
            base_ref: base_ref.to_string(),
            head_ref: "feature".to_string(),
            base_oid: base_oid.to_string(),
            head_oid: head_oid.to_string(),
            author: "someone".to_string(),
            review_decision: None,
            checks: ChecksSummary::None,
        }
    }

    /// Full round trip: a "reviewer" clone that only has the original base
    /// tip must fetch both the PR head (via the synthetic `pull/<n>/head`
    /// ref, exactly as GitHub publishes it) and the base branch's new tip
    /// (which advanced after the clone was made) before it can compute the
    /// merge-base range.
    #[test]
    fn prepare_pr_fetches_missing_head_and_base_then_computes_merge_base() {
        let origin = TestRepo::new("origin");
        origin.write("base.txt", b"base\n");
        let base_sha = origin.commit("base");

        let clone_dir =
            std::env::temp_dir().join(format!("dv-pr-test-{}-reviewer", std::process::id()));
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
            "clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Advance the PR's head on a feature branch off `base_sha`.
        origin.git(&["checkout", "-b", "feature", &base_sha]);
        origin.write("feature.txt", b"feature work\n");
        let head_sha = origin.commit("feature work");
        // Publish it the way GitHub does for an open PR: a synthetic ref,
        // no local branch.
        origin.git(&["update-ref", "refs/pull/9/head", &head_sha]);

        // Advance the PR's base after the clone, so the reviewer clone is
        // missing both endpoints.
        origin.git(&["checkout", "main"]);
        origin.write("main-only.txt", b"main moved on\n");
        let new_base_sha = origin.commit("main moves on");

        let reviewer =
            GitRepo::open(RepoLocation::Local(clone_dir.clone())).expect("open reviewer clone");
        assert!(
            !reviewer.has_object(&head_sha),
            "reviewer clone must not have the PR head yet"
        );
        assert!(
            !reviewer.has_object(&new_base_sha),
            "reviewer clone must not have the advanced base yet"
        );

        let meta = dummy_meta(9, "main", &new_base_sha, &head_sha);
        let range = prepare_pr(&reviewer, &meta).expect("prepare_pr should fetch and compute");

        assert_eq!(range.number, 9);
        assert_eq!(range.base_oid, new_base_sha);
        assert_eq!(range.head_oid, head_sha);
        assert_eq!(
            range.merge_base, base_sha,
            "common ancestor is the fork point"
        );
        assert_eq!(range.range, format!("{base_sha}..{head_sha}"));

        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    /// When both endpoints are already local (a repeat call, say),
    /// `prepare_pr` must not need to fetch at all — exercised implicitly by
    /// calling it twice on the same repo and getting the same answer.
    #[test]
    fn prepare_pr_skips_fetch_when_objects_already_local() {
        let repo = TestRepo::new("local-only");
        repo.write("a.txt", b"one\n");
        let base_sha = repo.commit("base");
        repo.write("a.txt", b"two\n");
        let head_sha = repo.commit("head");

        let git_repo =
            GitRepo::open(RepoLocation::Local(repo.path().to_path_buf())).expect("open repo");
        let meta = dummy_meta(1, "main", &base_sha, &head_sha);

        // No `origin` remote configured at all — if `prepare_pr` tried to
        // fetch here it would error, so success proves the has_object
        // skip-fetch path was taken for both endpoints.
        let range = prepare_pr(&git_repo, &meta).expect("no fetch should be needed");
        assert_eq!(range.merge_base, base_sha);
        assert_eq!(range.range, format!("{base_sha}..{head_sha}"));
    }

    /// A stale `meta.head_oid` (read before the PR's head moved — e.g. a
    /// force-push) fetches `pull/<n>/head` successfully, but that ref now
    /// points somewhere else: the oid `meta` named is still missing
    /// locally afterward, and `prepare_pr` must say so clearly instead of
    /// falling through to a cryptic merge-base failure.
    #[test]
    fn prepare_pr_errors_clearly_when_head_still_missing_after_fetch() {
        let origin = TestRepo::new("force-pushed-origin");
        origin.write("base.txt", b"base\n");
        let base_sha = origin.commit("base");

        let clone_dir = std::env::temp_dir().join(format!(
            "dv-pr-test-{}-force-pushed-reviewer",
            std::process::id()
        ));
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
            "clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        origin.git(&["checkout", "-b", "feature", &base_sha]);
        origin.write("feature.txt", b"feature work\n");
        let head_sha = origin.commit("feature work");
        origin.git(&["update-ref", "refs/pull/9/head", &head_sha]);

        let reviewer =
            GitRepo::open(RepoLocation::Local(clone_dir.clone())).expect("open reviewer clone");

        // `meta.head_oid` names an oid that doesn't exist anywhere —
        // simulating metadata read before a force-push moved the PR's
        // head off what `fetch_pr_head` will actually fetch.
        let bogus_head = "f".repeat(40);
        let meta = dummy_meta(9, "main", &base_sha, &bogus_head);

        let err = prepare_pr(&reviewer, &meta).expect_err("stale head oid must error clearly");
        assert!(
            err.to_string().contains("not found after fetch"),
            "unexpected error: {err}"
        );

        let _ = std::fs::remove_dir_all(&clone_dir);
    }

    /// Same as the head-side test above, mirrored onto `base_oid`: stale
    /// metadata (read before the base branch was rewritten/retargeted)
    /// fetches the base ref successfully, but the specific oid `meta`
    /// named still isn't local afterward — `prepare_pr` must say so
    /// clearly instead of silently proceeding to a cryptic merge-base
    /// failure, exactly as it already does for `head_oid`.
    #[test]
    fn prepare_pr_errors_clearly_when_base_still_missing_after_fetch() {
        let origin = TestRepo::new("stale-base-origin");
        origin.write("base.txt", b"base\n");
        let base_sha = origin.commit("base");

        let clone_dir = std::env::temp_dir().join(format!(
            "dv-pr-test-{}-stale-base-reviewer",
            std::process::id()
        ));
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
            "clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let reviewer =
            GitRepo::open(RepoLocation::Local(clone_dir.clone())).expect("open reviewer clone");

        // `meta.head_oid` reuses `base_sha` (already local in the clone —
        // no PR-head fetch is even attempted) so this test isolates the
        // base-side check in isolation; `meta.base_oid` names an oid that
        // doesn't exist anywhere, simulating metadata read before `main`
        // was rewritten out from under it.
        let bogus_base = "e".repeat(40);
        let meta = dummy_meta(9, "main", &bogus_base, &base_sha);

        let err = prepare_pr(&reviewer, &meta).expect_err("stale base oid must error clearly");
        let msg = err.to_string();
        assert!(
            msg.contains("not found after fetch"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("main"),
            "error should name the base branch: {msg}"
        );

        let _ = std::fs::remove_dir_all(&clone_dir);
    }
}
