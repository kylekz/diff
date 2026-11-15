//! Errors as views show them (DESIGN §4.4): a one-line headline from the error's `Display`, and
//! the command that failed with what it printed for the disclosure and "Copy diagnostics".
//! Never raw stderr as the headline.
//!
//! A failure also says whether it is a lost connection to `dv backend --stdio`
//! ([`Failure::disconnected`], `BackendError::Disconnected`, PLAN §5.2): the connection is
//! reconnecting, and what failed is read again once it is back, so a review raises no banner of
//! its own for it.

use std::fmt;

use dv_core::diff::DiffError;
use dv_core::git::GitError;
use dv_core::github::GithubError;
use dv_core::review::{OpError, SnapshotError, StoreError};
use dv_core::{BackendError, TargetError};

/// An error ready for a banner: cheap to clone, so one failure can sit in state and in a view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// One line, fit for a headline.
    pub message: String,
    /// The failed command and its output, when there is more to show.
    pub detail: Option<String>,
    /// The connection to a remote backend died under the call (module docs).
    pub disconnected: bool,
    /// Whether doing the same again can end differently: `false` for what only a change
    /// elsewhere puts right (another repository's pull request, one that isn't there), which
    /// a banner then offers no Retry for.
    pub retryable: bool,
}

impl Failure {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            detail: None,
            disconnected: false,
            retryable: true,
        }
    }

    /// `message` with `detail` under it.
    pub fn with_detail(message: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            detail: Some(detail.into()),
            ..Self::new(message)
        }
    }

    fn with_backend(message: String, backend: Option<&BackendError>) -> Self {
        Self {
            message,
            detail: backend.and_then(BackendError::detail),
            disconnected: matches!(backend, Some(BackendError::Disconnected { .. })),
            retryable: true,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<&BackendError> for Failure {
    fn from(error: &BackendError) -> Self {
        Self::with_backend(error.to_string(), Some(error))
    }
}

impl From<&GitError> for Failure {
    fn from(error: &GitError) -> Self {
        Self::with_backend(error.to_string(), git_backend(error))
    }
}

impl From<&DiffError> for Failure {
    fn from(error: &DiffError) -> Self {
        let backend = match error {
            DiffError::Git(git) => git_backend(git),
            _ => None,
        };
        Self::with_backend(error.to_string(), backend)
    }
}

impl From<&TargetError> for Failure {
    fn from(error: &TargetError) -> Self {
        let backend = match error {
            TargetError::Git(git) => git_backend(git),
            _ => None,
        };
        Self::with_backend(error.to_string(), backend)
    }
}

impl From<&StoreError> for Failure {
    fn from(error: &StoreError) -> Self {
        let backend = match error {
            StoreError::Backend(backend) => Some(backend),
            _ => None,
        };
        Self::with_backend(error.to_string(), backend)
    }
}

/// The headline says what failed and, when a command fixes it, which (`gh auth login`); the
/// detail is what `gh` ran and printed.
impl From<&GithubError> for Failure {
    fn from(error: &GithubError) -> Self {
        let message = match error.fix() {
            Some(fix) if !error.to_string().contains(&fix) => format!("{error}: run `{fix}`"),
            _ => error.to_string(),
        };
        match error {
            GithubError::Backend(backend) => Self::with_backend(message, Some(backend)),
            GithubError::Target(target) => Self {
                message,
                ..Self::from(target)
            },
            GithubError::Store(store) => Self {
                message,
                ..Self::from(store)
            },
            _ => Self {
                message,
                detail: error.detail(),
                disconnected: false,
                retryable: error.is_retryable(),
            },
        }
    }
}

impl From<&OpError> for Failure {
    fn from(error: &OpError) -> Self {
        let message = error.to_string();
        match error {
            OpError::Store(store) => Self {
                message,
                ..Self::from(store)
            },
            OpError::Target(target) => Self {
                message,
                ..Self::from(target)
            },
            OpError::Git(git) => Self {
                message,
                ..Self::from(git)
            },
            OpError::Snapshot(SnapshotError::Backend(backend)) => {
                Self::with_backend(message, Some(backend))
            }
            OpError::Snapshot(
                SnapshotError::Unmerged { detail } | SnapshotError::Unreadable { detail, .. },
            ) => Self::with_detail(message, detail.clone()),
            _ => Self::new(message),
        }
    }
}

fn git_backend(error: &GitError) -> Option<&BackendError> {
    match error {
        GitError::Backend(backend) => Some(backend),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_headline_is_one_line_and_git_output_goes_to_the_detail() {
        let git = BackendError::Git {
            args: vec!["diff".into(), "--raw".into()],
            code: Some(128),
            stderr: "fatal: bad object HEAD\nmore\n".into(),
        };
        let failure = Failure::from(&DiffError::Git(GitError::Backend(git)));
        assert_eq!(failure.message, "git diff failed (exit code 128)");
        let detail = failure.detail.expect("the command and its stderr");
        assert!(detail.contains("$ git diff --raw"), "{detail}");
        assert!(detail.contains("fatal: bad object HEAD"), "{detail}");

        let plain = Failure::from(&TargetError::NoDefaultBase {
            branch: "feat".into(),
        });
        assert!(!plain.message.contains('\n'));
        assert_eq!(plain.detail, None);
        assert!(!plain.disconnected);
    }

    #[test]
    fn a_lost_connection_says_so_through_every_wrapper() {
        let lost = || BackendError::Disconnected {
            location: "Ubuntu".into(),
            reason: "dv was killed".into(),
        };
        let direct = Failure::from(&lost());
        assert!(direct.disconnected);
        assert_eq!(
            direct.message,
            "lost the connection to dv in Ubuntu: dv was killed"
        );
        assert!(Failure::from(&DiffError::Git(GitError::Backend(lost()))).disconnected);
        assert!(Failure::from(&StoreError::Backend(lost())).disconnected);
        assert!(Failure::from(&OpError::Store(StoreError::Backend(lost()))).disconnected);
        let unavailable = BackendError::Unavailable {
            location: "Ubuntu".into(),
            reason: "the connection to dv failed: no build".into(),
        };
        assert!(!Failure::from(&unavailable).disconnected);
    }

    #[test]
    fn what_a_retry_cant_fix_says_so() {
        let elsewhere = GithubError::OtherRepository {
            number: 482,
            repository: "acme/api".into(),
        };
        assert!(!Failure::from(&elsewhere).retryable);
        let gone = GithubError::BaseGone {
            number: 9,
            branch: "stack".into(),
        };
        assert!(!Failure::from(&gone).retryable);
        // Signed out, offline, a push under the read: again may do it.
        assert!(Failure::from(&GithubError::Moved { number: 7 }).retryable);
        assert!(Failure::new("anything else").retryable);
    }
}
