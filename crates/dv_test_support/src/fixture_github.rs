//! `FixtureGithub`: a GitHub repository and a clone of it, without GitHub (docs/PLAN.md
//! §9.11).
//!
//! Three repositories, each built through the `git` CLI:
//!
//! - **The author's** ([`FixtureGithub::author`]), a [`FixtureRepo`]: where the repository's
//!   commits and its pull requests' are made, so they carry the fixture's identity and dates
//!   and the same script yields the same commit ids on every machine, which recorded `gh`
//!   responses can name.
//! - **GitHub's** ([`FixtureGithub::bare`]), a bare repository beside the author's: its
//!   branches are what the author published, and a pull request is `refs/pull/<n>/head`, as
//!   on GitHub ([`FixtureGithub::open_pr`]).
//! - **The clone** ([`FixtureGithub::checkout`]), a second [`FixtureRepo`]: the repository under
//!   review. Its `origin` is `https://github.com/dv-fixture/<name>.git`, which its own
//!   configuration rewrites to the bare repository (`url.<file URL>.insteadOf`), so code under
//!   test reads a GitHub remote and its `git fetch` reaches the fixture, never the network.
//!   It starts as a fresh clone does: `main` checked out, tracking `origin/main`, and
//!   `origin/HEAD` set. A pull request's commits aren't in it until something fetches them.

use std::path::{Path, PathBuf};

use crate::FixtureRepo;

/// The owner of every fixture repository on the stand-in GitHub.
pub const GITHUB_OWNER: &str = "dv-fixture";

/// A stand-in GitHub repository and a clone of it (module docs).
pub struct FixtureGithub {
    origin: GithubOrigin,
    clone: FixtureRepo,
}

/// The fixture GitHub's own side of a [`FixtureGithub`]: the author's repository, where
/// commits are made, and the bare one standing in for GitHub, which they are pushed to.
pub struct GithubOrigin {
    name: String,
    author: FixtureRepo,
    bare: PathBuf,
}

/// Where a persistent fixture keeps its GitHub side, beside the clone's own directories
/// (`<clone>.fixture/`).
const ORIGIN_DIR: &str = "github";

impl FixtureGithub {
    /// The repository `dv-fixture/<name>` with one commit on `main` (a `README.md`), and its
    /// clone.
    pub fn new(name: &str) -> Self {
        Self::build(name, FixtureRepo::new(), FixtureRepo::new())
    }

    /// [`FixtureGithub::new`] whose clone is built at `path`, its work tree, named by the
    /// path's last component, and kept when the value is dropped
    /// ([`FixtureRepo::persistent`]): for the repositories the automation host seeds for a
    /// scenario (PLAN §14.2). Its GitHub side goes beside the clone's own directories, in
    /// `<path>.fixture/github/`, and is found again with [`GithubOrigin::reopen`].
    pub fn persistent(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_else(|| panic!("{} has no UTF-8 file name", path.display()))
            .to_string();
        let clone = FixtureRepo::persistent(path);
        let author = FixtureRepo::persistent(clone.outside_dir(ORIGIN_DIR).join("author"));
        Self::build(&name, author, clone)
    }

    fn build(name: &str, author: FixtureRepo, clone: FixtureRepo) -> Self {
        author.write("README.md", format!("# {name}\n"));
        author.commit("Initial commit");
        let bare = author.bare_clone("github.git");

        let url = origin_url(name);
        clone
            .config("remote.origin.url", &url)
            .config("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
            .config(&format!("url.{}.insteadOf", file_url(&bare)), &url);
        clone.git(&["fetch", "-q", "origin"]);
        clone.git(&["remote", "set-head", "origin", "main"]);
        clone.git(&["checkout", "-q", "-B", "main", "--track", "origin/main"]);
        Self {
            origin: GithubOrigin {
                name: name.to_string(),
                author,
                bare,
            },
            clone,
        }
    }

    /// The clone and the GitHub side, each on its own: what holds one of them for longer
    /// than the other (the automation host's seeded repositories).
    pub fn into_parts(self) -> (FixtureRepo, GithubOrigin) {
        (self.clone, self.origin)
    }

    /// The repository's name on GitHub; its owner is [`GITHUB_OWNER`].
    pub fn name(&self) -> &str {
        self.origin.name()
    }

    /// `origin`'s URL in the clone.
    pub fn url(&self) -> String {
        origin_url(self.name())
    }

    /// The author's repository: commits made here reach GitHub through
    /// [`FixtureGithub::publish`] and [`FixtureGithub::open_pr`].
    pub fn author(&self) -> &FixtureRepo {
        self.origin.author()
    }

    /// The bare repository standing in for GitHub.
    pub fn bare(&self) -> &Path {
        self.origin.bare()
    }

    /// The clone: the repository under review.
    pub fn checkout(&self) -> &FixtureRepo {
        &self.clone
    }

    /// Pushes the author's `branch` to GitHub as it stands, forced: a base branch that moved
    /// on, or was rewritten.
    pub fn publish(&self, branch: &str) -> &Self {
        self.origin.publish(branch);
        self
    }

    /// Makes `rev` of the author's repository the head of pull request `number`
    /// ([`GithubOrigin::open_pr`]) and returns its commit id.
    pub fn open_pr(&self, number: u64, rev: &str) -> String {
        self.origin.open_pr(number, rev)
    }
}

impl GithubOrigin {
    /// The GitHub side of the persistent fixture whose clone is at `path`
    /// ([`FixtureGithub::persistent`]), found again by a later process; `None` when the
    /// repository there has none.
    pub fn reopen(path: impl AsRef<Path>) -> Option<Self> {
        let path = path.as_ref();
        let name = path.file_name()?.to_str()?.to_string();
        let side = path
            .with_file_name(format!("{name}.fixture"))
            .join(ORIGIN_DIR);
        let (author, bare) = (
            side.join("author"),
            side.join("author.fixture").join("github.git"),
        );
        (author.join(".git").is_dir() && bare.is_dir()).then(|| Self {
            name,
            author: FixtureRepo::reopen(author),
            bare,
        })
    }

    /// The repository's name on GitHub; its owner is [`GITHUB_OWNER`].
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The author's repository.
    pub fn author(&self) -> &FixtureRepo {
        &self.author
    }

    /// The bare repository standing in for GitHub.
    pub fn bare(&self) -> &Path {
        &self.bare
    }

    /// Pushes the author's `branch` to GitHub as it stands, forced.
    pub fn publish(&self, branch: &str) {
        self.push(&format!("+refs/heads/{branch}:refs/heads/{branch}"));
    }

    /// Makes `rev` of the author's repository the head of pull request `number`
    /// (`refs/pull/<number>/head` on GitHub) and returns its commit id. Called again, it is a
    /// push to the pull request, or a force-push when `rev` doesn't descend from the old head.
    /// The head's branch isn't published: like a fork's, it reaches the clone only through the
    /// pull request's ref.
    pub fn open_pr(&self, number: u64, rev: &str) -> String {
        let oid = self
            .author
            .git(&["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
            .trim()
            .to_string();
        self.push(&format!("+{oid}:refs/pull/{number}/head"));
        oid
    }

    fn push(&self, refspec: &str) {
        self.author.git(&[
            "push".as_ref(),
            "-q".as_ref(),
            self.bare.as_os_str(),
            refspec.as_ref(),
        ]);
    }
}

impl std::fmt::Debug for FixtureGithub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FixtureGithub")
            .field("name", &self.origin.name)
            .field("bare", &self.origin.bare)
            .field("clone", &self.clone.path())
            .finish()
    }
}

/// The URL a clone of `dv-fixture/<name>` has for `origin`.
fn origin_url(name: &str) -> String {
    format!("https://github.com/{GITHUB_OWNER}/{name}.git")
}

/// `path` as a `file://` URL git fetches from: `/` separators, a Windows drive after a third
/// slash (`file:///C:/…`), and `%` and spaces escaped, since git decodes a URL's path.
fn file_url(path: &Path) -> String {
    let path = path
        .to_string_lossy()
        .replace('\\', "/")
        .replace('%', "%25")
        .replace(' ', "%20");
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

#[cfg(test)]
mod tests;
