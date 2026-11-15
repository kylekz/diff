//! Git object ids and the repository queries the rest of dv_core builds on (docs/PLAN.md §2.2,
//! §6), over any [`Backend`].
//!
//! Revisions from users and reviews are passed after `--end-of-options`, so a revision that
//! starts with `-` is looked up as a revision and never read as an option.

pub mod branches;
pub mod command;
pub(crate) mod temp_index;

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::backend::{Backend, BackendError, GitOpts, GitOutput, RepoInfo};

/// The empty tree's id in a SHA-1 repository: the old side of a diff against nothing (an unborn
/// `HEAD`, a root commit).
pub const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// The empty tree's id in a SHA-256 repository.
pub const EMPTY_TREE_SHA256: &str =
    "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

/// A repository's object id hash (`git rev-parse --show-object-format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    /// Parses git's name for the format (`sha1`, `sha256`).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "sha1" => Some(Self::Sha1),
            "sha256" => Some(Self::Sha256),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }

    /// The length of an object id in hex digits.
    pub fn hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }

    /// The empty tree's id in this format.
    pub fn empty_tree(self) -> Oid {
        Oid(match self {
            Self::Sha1 => EMPTY_TREE_SHA1,
            Self::Sha256 => EMPTY_TREE_SHA256,
        }
        .to_string())
    }
}

/// A full git object id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits, as git prints them.
/// Serialised as that string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Oid(String);

impl Oid {
    /// `text` as an object id, when it is one (full length, lowercase hex).
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        (hex && (text.len() == 40 || text.len() == 64)).then(|| Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 7 hex digits, as git abbreviates an id for people.
    pub fn short(&self) -> &str {
        &self.0[..7]
    }

    /// The format this id belongs to, from its length.
    pub fn format(&self) -> ObjectFormat {
        if self.0.len() == ObjectFormat::Sha256.hex_len() {
            ObjectFormat::Sha256
        } else {
            ObjectFormat::Sha1
        }
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Oid {
    type Err = InvalidOid;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text).ok_or_else(|| InvalidOid(text.to_string()))
    }
}

impl TryFrom<String> for Oid {
    type Error = InvalidOid;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<Oid> for String {
    fn from(oid: Oid) -> Self {
        oid.0
    }
}

/// A string that isn't a full object id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a git object id: {0:?}")]
pub struct InvalidOid(pub String);

/// Why a repository query failed.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error("unknown revision {rev:?}")]
    UnknownRevision { rev: String },
    #[error("unexpected output from git {command}: {detail}")]
    UnexpectedOutput {
        command: &'static str,
        detail: String,
    },
}

/// One worktree of a repository ([`GitRepo::worktrees`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Its root as git prints it, `/`-separated.
    pub path: String,
    /// The commit checked out; `None` on an unborn branch.
    pub head: Option<Oid>,
    /// The branch checked out; `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// The bare repository itself, listed first in a bare repository with linked worktrees: no
    /// files to review.
    pub bare: bool,
    /// Its directory is gone (`prunable`: deleted without `git worktree remove`), though git
    /// still lists it until `git worktree prune`.
    pub prunable: bool,
}

/// Repository queries over a [`Backend`]: thin wrappers around single git commands, shared by
/// targets (PLAN §2.2) and the diff pipeline (PLAN §6).
#[derive(Clone)]
pub struct GitRepo {
    backend: Arc<dyn Backend>,
}

impl GitRepo {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    pub fn info(&self) -> &RepoInfo {
        self.backend.repo()
    }

    /// The repository's object id format, which the backend resolved with
    /// `git rev-parse --show-object-format` when it opened.
    pub fn object_format(&self) -> ObjectFormat {
        self.info().object_format
    }

    /// The empty tree's id in this repository's object format.
    pub fn empty_tree(&self) -> Oid {
        self.object_format().empty_tree()
    }

    /// The commit `HEAD` points at; `None` on an unborn branch (a repo without commits).
    pub fn head(&self) -> Result<Option<Oid>, GitError> {
        // `--verify --quiet` exits 1 without output when `HEAD` doesn't resolve.
        let output = self.git(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                "HEAD^{commit}",
            ],
            &[0, 1],
        )?;
        if output.code == 1 {
            return Ok(None);
        }
        parse_oid("rev-parse", &output).map(Some)
    }

    /// The commit `rev` names (a branch, tag, oid prefix, `HEAD~2`, …), peeled to a commit;
    /// [`GitError::UnknownRevision`] when it names none.
    pub fn resolve(&self, rev: &str) -> Result<Oid, GitError> {
        let unknown = || GitError::UnknownRevision {
            rev: rev.to_string(),
        };
        if rev.is_empty() {
            return Err(unknown());
        }
        let peeled = format!("{rev}^{{commit}}");
        let output = self.git(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &peeled,
            ],
            &[0, 1],
        )?;
        if output.code == 1 {
            return Err(unknown());
        }
        parse_oid("rev-parse", &output)
    }

    /// The best common ancestor of two revisions; `None` when their histories are unrelated.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<Oid>, GitError> {
        for rev in [a, b] {
            if rev.is_empty() {
                return Err(GitError::UnknownRevision {
                    rev: rev.to_string(),
                });
            }
        }
        // Exit 1 means "no common ancestor"; an unknown revision is exit 128.
        let output = match self.git(&["merge-base", "--end-of-options", a, b], &[0, 1]) {
            Ok(output) => output,
            Err(GitError::Backend(BackendError::Git {
                code: Some(128),
                stderr,
                ..
            })) if stderr.contains("Not a valid object name") => {
                let rev = stderr
                    .lines()
                    .find_map(|line| line.split("Not a valid object name ").nth(1))
                    .unwrap_or_default();
                return Err(GitError::UnknownRevision {
                    rev: rev.trim().to_string(),
                });
            }
            Err(err) => return Err(err),
        };
        if output.code == 1 {
            return Ok(None);
        }
        parse_oid("merge-base", &output).map(Some)
    }

    /// The full ref name a branch target's `name` stands for (PLAN §2.2): `refs/heads/<name>`
    /// when that branch exists, because the name is a branch's and `git switch` too prefers a
    /// branch over a tag of the same name; otherwise the ref git's revision rules find
    /// (`heads/feat` and `refs/heads/feat` are `refs/heads/feat`, `origin/feat` is
    /// `refs/remotes/origin/feat`, `HEAD` the branch checked out). `None` when `name` is a
    /// revision but not a ref (`HEAD~2`, an object id) or matches several refs;
    /// [`GitError::UnknownRevision`] when it names nothing.
    pub fn full_ref_name(&self, name: &str) -> Result<Option<String>, GitError> {
        let unknown = || GitError::UnknownRevision {
            rev: name.to_string(),
        };
        if name.is_empty() {
            return Err(unknown());
        }
        let branch = format!("refs/heads/{name}");
        if self.commit_exists(&branch)? {
            return Ok(Some(branch));
        }
        // Exit 1: no such revision. A revision that isn't a ref, or an ambiguous ref name,
        // prints nothing and exits 0.
        let output = self.git(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--symbolic-full-name",
                "--end-of-options",
                name,
            ],
            &[0, 1],
        )?;
        if output.code == 1 {
            return Err(unknown());
        }
        let text = stdout_text("rev-parse", &output)?.trim_end();
        Ok((!text.is_empty()).then(|| text.to_string()))
    }

    /// Every worktree of the repository, this one included (`git worktree list --porcelain`):
    /// its root as git prints it (`/`-separated), what it has checked out, and whether it is
    /// the bare repository or a worktree whose directory is gone.
    pub fn worktrees(&self) -> Result<Vec<Worktree>, GitError> {
        let output = self.git(&["worktree", "list", "--porcelain"], &[0])?;
        let text = stdout_text("worktree list", &output)?;
        let mut worktrees: Vec<Worktree> = Vec::new();
        for line in text.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                worktrees.push(Worktree {
                    path: path.to_string(),
                    head: None,
                    branch: None,
                    bare: false,
                    prunable: false,
                });
            } else if let Some(last) = worktrees.last_mut() {
                if line == "bare" {
                    last.bare = true;
                } else if line == "prunable" || line.starts_with("prunable ") {
                    last.prunable = true;
                } else if let Some(head) = line.strip_prefix("HEAD ") {
                    // An unborn branch's HEAD is printed as zeros.
                    last.head =
                        Oid::parse(head).filter(|oid| !oid.as_str().bytes().all(|b| b == b'0'));
                } else if let Some(branch) = line.strip_prefix("branch ") {
                    last.branch = Some(
                        branch
                            .strip_prefix("refs/heads/")
                            .unwrap_or(branch)
                            .to_string(),
                    );
                }
            }
        }
        Ok(worktrees)
    }

    /// The branch checked out in this worktree (`main`, also on an unborn branch); `None` when
    /// `HEAD` is detached.
    pub fn current_branch(&self) -> Result<Option<String>, GitError> {
        // `symbolic-ref --quiet` exits 1 without output when `HEAD` is detached.
        let output = self.git(&["symbolic-ref", "--quiet", "HEAD"], &[0, 1])?;
        if output.code == 1 {
            return Ok(None);
        }
        let text = stdout_text("symbolic-ref", &output)?;
        Ok(text
            .trim_end()
            .strip_prefix("refs/heads/")
            .map(str::to_string))
    }

    /// A commit's parents, first parent first; empty for a root commit.
    pub fn parents(&self, commit: &Oid) -> Result<Vec<Oid>, GitError> {
        // `rev-list --parents` prints `<commit> <parent>…` on one line; `commit` is a full object
        // id, so it can't be read as an option.
        let output = self.git(
            &["rev-list", "--parents", "--max-count=1", commit.as_str()],
            &[0],
        )?;
        let text = stdout_text("rev-list", &output)?;
        let mut ids = text.split_whitespace();
        let unexpected = || GitError::UnexpectedOutput {
            command: "rev-list",
            detail: format!("expected {commit} and its parents, got {text:?}"),
        };
        if ids.next() != Some(commit.as_str()) {
            return Err(unexpected());
        }
        ids.map(|id| Oid::parse(id).ok_or_else(unexpected))
            .collect()
    }

    /// The branch a `branch` target compares against when it names no base (PLAN §2.2): the
    /// remote default branch `origin/HEAD` points at (`origin/main`), else a local `main`, else
    /// `master`; `None` when there is none of these.
    pub fn default_branch(&self) -> Result<Option<String>, GitError> {
        // `symbolic-ref --quiet` exits 1 without output when `origin/HEAD` isn't a symbolic ref.
        let output = self.git(
            &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
            &[0, 1],
        )?;
        if output.code == 0 {
            let target = stdout_text("symbolic-ref", &output)?.trim_end();
            // A clone records `origin/HEAD`; a later `git remote set-head` or a pruned branch
            // can leave it pointing at nothing, so the target must resolve to be used.
            if let Some(short) = target.strip_prefix("refs/remotes/")
                && self.commit_exists(target)?
            {
                return Ok(Some(short.to_string()));
            }
        }
        for branch in ["main", "master"] {
            if self.commit_exists(&format!("refs/heads/{branch}"))? {
                return Ok(Some(branch.to_string()));
            }
        }
        Ok(None)
    }

    /// Whether the commit `oid` is in the repository (`git cat-file -e <oid>^{commit}`). A
    /// round's snapshot commit is gone when its ref was deleted by hand and git pruned it, or
    /// the store came from another clone (PLAN §2.3).
    pub fn has_commit(&self, oid: &Oid) -> Result<bool, GitError> {
        let peeled = format!("{oid}^{{commit}}");
        // Exit 128 with "Not a valid object name" for a missing object (or one that isn't a
        // commit); any other 128 is a real failure.
        let output = self.git(&["cat-file", "-e", &peeled], &[0, 128])?;
        if output.code == 0 {
            return Ok(true);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Not a valid object name") {
            Ok(false)
        } else {
            Err(GitError::UnexpectedOutput {
                command: "cat-file -e",
                detail: stderr.trim_end().to_string(),
            })
        }
    }

    /// How the commits `from` and `to` differ in history (`git rev-list --count --left-right
    /// <from>...<to>`): how many commits only `from` reaches, and how many only `to` does.
    /// `(0, n)` is a fast-forward of n commits; a first count above zero means `to` doesn't
    /// hold `from` (a force-push). Both are full object ids, so neither reads as an option.
    pub fn commits_apart(&self, from: &Oid, to: &Oid) -> Result<(u32, u32), GitError> {
        let range = format!("{from}...{to}");
        let output = self.git(&["rev-list", "--count", "--left-right", &range], &[0])?;
        let text = stdout_text("rev-list", &output)?;
        let mut counts = text.split_whitespace().map(str::parse::<u32>);
        match (counts.next(), counts.next(), counts.next()) {
            (Some(Ok(behind)), Some(Ok(ahead)), None) => Ok((behind, ahead)),
            _ => Err(GitError::UnexpectedOutput {
                command: "rev-list",
                detail: format!("expected two counts, got {text:?}"),
            }),
        }
    }

    /// The object each of the refs `names` (full names) points at; a ref that doesn't exist is
    /// left out (one `git for-each-ref`).
    pub fn ref_oids(&self, names: &[&str]) -> Result<HashMap<String, Oid>, GitError> {
        let mut args = vec!["for-each-ref", "--format=%(refname) %(objectname)"];
        args.extend_from_slice(names);
        let output = self.git(&args, &[0])?;
        // A name also matches as a directory of refs: only the refs asked for are kept.
        Ok(stdout_text("for-each-ref", &output)?
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(name, _)| names.contains(name))
            .filter_map(|(name, oid)| Some((name.to_string(), Oid::parse(oid)?)))
            .collect())
    }

    /// Whether the full ref name `name` resolves to a commit.
    fn commit_exists(&self, name: &str) -> Result<bool, GitError> {
        let peeled = format!("{name}^{{commit}}");
        let output = self.git(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &peeled,
            ],
            &[0, 1],
        )?;
        Ok(output.code == 0)
    }

    fn git(&self, args: &[&str], ok_codes: &[i32]) -> Result<GitOutput, GitError> {
        let opts = GitOpts::new().with_ok_codes(ok_codes);
        Ok(self.backend.git(args, &opts)?)
    }
}

impl fmt::Debug for GitRepo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitRepo")
            .field("toplevel", &self.info().toplevel)
            .finish()
    }
}

fn stdout_text<'a>(command: &'static str, output: &'a GitOutput) -> Result<&'a str, GitError> {
    output
        .stdout_utf8()
        .ok_or_else(|| GitError::UnexpectedOutput {
            command,
            detail: "stdout is not UTF-8".to_string(),
        })
}

/// The single object id `command` printed.
fn parse_oid(command: &'static str, output: &GitOutput) -> Result<Oid, GitError> {
    let text = stdout_text(command, output)?.trim_end();
    Oid::parse(text).ok_or_else(|| GitError::UnexpectedOutput {
        command,
        detail: format!("expected an object id, got {text:?}"),
    })
}

#[cfg(test)]
mod tests;
