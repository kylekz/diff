//! Opening and refreshing a pull request against the fixture GitHub and `mock_gh` (PLAN
//! §9.11): the fetch and the old side ([`fetching`]), opening ([`opening`]), refreshing
//! ([`refreshing`]), what goes with a pull
//! request's last review ([`lifecycle`]), openers and fetches at once and a backend that
//! fails under them ([`contending`]), GitHub's threads placed and resolved ([`threads`]), and
//! the fixture they share.

mod contending;
mod fetching;
mod lifecycle;
mod opening;
mod refreshing;
mod threads;

use std::sync::Arc;

use dv_test_support::fixture_gh::{self, GhPr};
use dv_test_support::{FixtureGithub, GITHUB_OWNER};
use serde_json::Value;

use super::*;
use crate::backend::Backend;
use crate::git::{GitRepo, Oid};
use crate::github::gh::tests::MockGh;
use crate::review::pins::{self, Pins};
use crate::review::{Author, Review, ReviewStore};
use crate::testing::open as open_backend;

/// The login the fixture's `gh` is signed in as.
const VIEWER: &str = "kylekz";

/// The pull request every test opens.
const NUMBER: u64 = 7;

/// `src/lib.rs` as the pull request's first commit adds it.
const LIB: &str = "pub fn answer() -> u32 {\n    42\n}\n";

/// The fixture GitHub's repository `dv-fixture/<name>` with pull request 7, `feat` into
/// `main`, one commit adding `src/lib.rs`; its clone; and a `gh` that answers about it.
struct PrFixture {
    github: FixtureGithub,
    mock: MockGh,
    /// The pull request as `gh` tells it; [`PrFixture::answer`] after changing it.
    pr: GhPr,
}

impl PrFixture {
    fn new() -> Self {
        Self::named("api")
    }

    fn named(name: &str) -> Self {
        let github = FixtureGithub::new(name);
        let author = github.author();
        author
            .branch("feat")
            .checkout("feat")
            .write("src/lib.rs", LIB);
        author.commit("Add the answer");
        let head = github.open_pr(NUMBER, "feat");
        let mut pr = GhPr::new(NUMBER, &head);
        pr.files = vec![fixture_gh::file(
            "src/lib.rs",
            "added",
            Some("@@ -0,0 +1,3 @@\n+pub fn answer() -> u32 {\n+    42\n+}"),
        )];
        let fixture = Self {
            github,
            mock: MockGh::new(Value::Array(Vec::new())),
            pr,
        };
        fixture.answer(Vec::new());
        fixture
    }

    /// Has `gh` answer about the pull request as [`PrFixture::pr`] stands, `first` taking
    /// any call they match before that.
    fn answer(&self, first: Vec<Value>) {
        let mut calls = first;
        calls.push(fixture_gh::viewer(VIEWER));
        calls.extend(self.pr.calls_in(&self.github));
        self.mock.answer(Value::Array(calls));
    }

    /// A push to the pull request: `src/lib.rs` rewritten as `text` on `feat`, which `gh`
    /// reports as the head from now on. Returns the new head.
    fn push(&mut self, text: &str) -> Oid {
        let author = self.github.author();
        author.checkout("feat").write("src/lib.rs", text);
        author.commit("Change the answer");
        let head = self.github.open_pr(NUMBER, "feat");
        self.pr.head = head.clone();
        self.answer(Vec::new());
        oid(&head)
    }

    fn backend(&self) -> Arc<dyn Backend> {
        Arc::new(open_backend(self.github.checkout()))
    }

    fn git(&self) -> GitRepo {
        GitRepo::new(self.backend())
    }

    fn store(&self) -> ReviewStore {
        ReviewStore::new(self.backend())
    }

    fn mirrors(&self) -> MirrorStore {
        MirrorStore::new(self.backend())
    }

    fn repo(&self) -> GithubRepo {
        github_repo("github.com", GITHUB_OWNER, self.github.name())
    }

    fn key(&self) -> MirrorKey {
        MirrorKey::new(&self.repo(), NUMBER, VIEWER)
    }

    /// Opens pull request 7 as the reviewer.
    fn open(&self) -> OpenedPr {
        self.open_ref(&PrRef::Hash(NUMBER)).unwrap()
    }

    fn open_ref(&self, reference: &PrRef) -> Result<OpenedPr, GithubError> {
        open(&self.backend(), &self.mock.gh, reference, reviewer)
    }

    fn refresh(&self, review: &Review) -> Refreshed {
        refresh(&self.backend(), &self.mock.gh, review)
    }

    fn pins(&self, review: &Review) -> Pins {
        pins::read(&self.git(), &review.id).unwrap().unwrap()
    }

    /// The pull request's head as the fixture GitHub has it.
    fn head(&self) -> Oid {
        oid(&self.pr.head)
    }

    /// `main` as the fixture GitHub has it: the pull request's merge base while `main` stands
    /// still.
    fn main(&self) -> Oid {
        oid(self.github.author().git(&["rev-parse", "main"]).trim())
    }

    /// The clone's refs under `prefix`, with what each points at.
    fn refs(&self, prefix: &str) -> Vec<(String, String)> {
        self.github
            .checkout()
            .git(&["for-each-ref", "--format=%(refname) %(objectname)", prefix])
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(name, oid)| (name.to_string(), oid.to_string()))
            .collect()
    }

    /// The arguments of every `gh` call so far, each joined by spaces.
    fn asked(&self) -> Vec<String> {
        self.mock
            .calls()
            .iter()
            .map(|call| {
                call["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|arg| arg.as_str().unwrap())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }
}

fn reviewer() -> Author {
    Author::reviewer("dv fixture")
}

fn oid(text: &str) -> Oid {
    Oid::parse(text).unwrap_or_else(|| panic!("not an object id: {text:?}"))
}
