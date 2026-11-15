//! How a review's threads are named where both kinds show together (docs/PLAN.md §9.6): the
//! review's own, by their [`ThreadId`], and GitHub's on a PR review, by GitHub's node id.
//!
//! GitHub's threads are shown from the pull request's mirror and never enter the review's log,
//! so they have no [`ThreadId`]: an id that names a log's items never names one of them. The
//! UI and the CLI's documents name every thread by its [`ThreadKey`] instead; the CLI spells a
//! GitHub thread's `gh:<node id>`.

use std::fmt;
use std::str::FromStr;

use super::id::{InvalidId, ThreadId};

/// What a GitHub thread's key starts with, as the CLI spells it.
pub const GITHUB_PREFIX: &str = "gh:";

/// A thread of a review as the UI and the DTOs name it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ThreadKey {
    /// One of the review's own threads, in its log.
    Local(ThreadId),
    /// One of GitHub's threads on the review's pull request, by GitHub's node id.
    Github(String),
}

impl ThreadKey {
    /// `text` as a key: `gh:<node id>` (the id not empty, without whitespace or anything
    /// that moves or hides text, `crate::text::is_unprintable`) or a thread id.
    pub fn parse(text: &str) -> Result<Self, InvalidId> {
        match text.strip_prefix(GITHUB_PREFIX) {
            Some(node)
                if !node.is_empty()
                    && !node
                        .chars()
                        .any(|c| c.is_whitespace() || crate::text::is_unprintable(c)) =>
            {
                Ok(Self::Github(node.to_string()))
            }
            Some(_) => Err(InvalidId {
                kind: "thread",
                text: text.to_string(),
                reason: "expected a GitHub node id after gh:",
            }),
            None => ThreadId::parse(text).map(Self::Local),
        }
    }

    /// The review's own thread it names.
    pub fn local(&self) -> Option<&ThreadId> {
        match self {
            Self::Local(id) => Some(id),
            Self::Github(_) => None,
        }
    }

    /// The node id of the GitHub thread it names.
    pub fn github(&self) -> Option<&str> {
        match self {
            Self::Local(_) => None,
            Self::Github(node) => Some(node),
        }
    }
}

impl fmt::Display for ThreadKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(id) => f.write_str(id.as_str()),
            Self::Github(node) => write!(f, "{GITHUB_PREFIX}{node}"),
        }
    }
}

impl FromStr for ThreadKey {
    type Err = InvalidId;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl From<ThreadId> for ThreadKey {
    fn from(id: ThreadId) -> Self {
        Self::Local(id)
    }
}

impl From<&ThreadId> for ThreadKey {
    fn from(id: &ThreadId) -> Self {
        Self::Local(id.clone())
    }
}

impl From<&ThreadKey> for ThreadKey {
    fn from(key: &ThreadKey) -> Self {
        key.clone()
    }
}
