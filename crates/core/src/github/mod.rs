//! GitHub integration via the `gh` CLI (docs/phase-3-github.md).
//!
//! Everything here is headless (no gpui) and talks to GitHub exclusively
//! by shelling out to `gh` — no token storage, no octocrab/HTTP client of
//! our own, auth/SSO/enterprise all come from the user's own `gh auth
//! login`. See [`GithubClient`] for the entry point and CLAUDE.md's
//! "Design decisions (already made)" for the reasoning behind `gh` always
//! running on the Windows host even for WSL-located repos.

mod client;
mod error;
mod models;
mod open_target;
mod slug;

pub use client::{GithubClient, gh_status};
pub use error::GhError;
pub use models::{
    ChecksSummary, CreatePr, CreatedPr, DraftComment, GhSide, PrMeta, PrState, PrStatus, PrSummary,
    RemoteComment, RemoteThread, ReviewDecision, ReviewEvent, ReviewSubmission, SubmittedReview,
};
pub use open_target::{OpenTarget, parse_open_target};
pub use slug::RepoSlug;

#[cfg(test)]
mod integration_test {
    //! One `#[ignore]`d end-to-end smoke test against the real `gh` binary
    //! and a real (private) test repo — see docs/phase-3-github.md § Test
    //! environment. Not run in CI (which may have `gh` unauthenticated, or
    //! not installed at all); run manually with:
    //!
    //! ```text
    //! cargo test -p dv-core --lib github::integration_test -- --ignored --nocapture
    //! ```
    use crate::git::GitRepo;
    use crate::github::GithubClient;
    use crate::location::RepoLocation;

    #[test]
    #[ignore = "hits the real gh binary and a live GitHub repo; run manually"]
    fn smoke_test_against_kylekz_difftest() {
        // Point this at a local clone of git@github.com:kylekz/difftest.git
        // (docs/phase-3-github.md's dedicated phase-3 test repo) — set
        // DV_DIFFTEST_PATH to override the default guess.
        let repo_path = std::env::var("DV_DIFFTEST_PATH").unwrap_or_else(|_| {
            std::env::current_dir()
                .unwrap()
                .join("../../../difftest")
                .to_string_lossy()
                .into_owned()
        });
        let repo = GitRepo::open(RepoLocation::Local(repo_path.into()))
            .expect("open the difftest checkout (set DV_DIFFTEST_PATH if it's elsewhere)");

        let client = GithubClient::for_repo(&repo).expect("resolve gh + parse origin remote");
        println!("slug: {}", client.slug());

        client.preflight().expect("gh --version + gh auth status");

        let prs = client.list_prs().expect("gh pr list");
        println!("open PRs: {}", prs.len());
        for pr in &prs {
            println!("  #{} {} ({})", pr.number, pr.title, pr.head_ref);
        }
    }

    /// Live-verifies `pr_review_threads`' `gh api graphql` query actually
    /// parses against a real PR (docs/phase-6-review-navigator.md
    /// deliverable 6) — `gh api graphql` is otherwise unproven anywhere in
    /// this codebase. Run manually:
    ///
    /// ```text
    /// cargo test -p dv-core --lib github:: -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "hits the real gh binary and a live GitHub repo; run manually"]
    fn pr_review_threads_smoke() {
        let repo_path = std::env::var("DV_DIFFTEST_PATH").unwrap_or_else(|_| {
            std::env::current_dir()
                .unwrap()
                .join("../../../difftest")
                .to_string_lossy()
                .into_owned()
        });
        let repo = GitRepo::open(RepoLocation::Local(repo_path.into()))
            .expect("open the difftest checkout (set DV_DIFFTEST_PATH if it's elsewhere)");

        let client = GithubClient::for_repo(&repo).expect("resolve gh + parse origin remote");
        client.preflight().expect("gh --version + gh auth status");

        let threads = client
            .pr_review_threads(1)
            .expect("gh api graphql review threads");
        println!("PR #1 review threads: {}", threads.len());
        for t in &threads {
            println!(
                "  {} resolved={} {}:{:?} comments={} review_database_id={:?}",
                t.id,
                t.is_resolved,
                t.path,
                t.line,
                t.comments.len(),
                t.review_database_id
            );
        }
    }
}
