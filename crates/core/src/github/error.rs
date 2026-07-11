//! [`GhError`]: every failure mode of the `gh`-shelling client, with a
//! `Display` written for a human to read directly (CLI stderr, a GPUI error
//! toast) rather than for a developer to `{:?}`-dump.

use std::fmt;

/// Errors from resolving, authenticating with, or invoking `gh`.
///
/// Two variants ([`GhError::NotFound`], [`GhError::NotAuthenticated`]) are
/// deliberately more than "gh failed" — they're the two failure modes a
/// first-time user is overwhelmingly likely to hit, so they carry
/// actionable next steps instead of just gh's own stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhError {
    /// `gh` could not be located via `DV_GH`, a `PATH` lookup, or (Windows
    /// only) the GitHub CLI installer's default path.
    NotFound,
    /// `gh auth status` (or another `gh` call whose failure looks like an
    /// auth problem — see `classify_failure`) failed for `host`.
    NotAuthenticated { host: String },
    /// The repo's `origin` remote URL doesn't match any GitHub/GHE URL
    /// shape [`crate::github::RepoSlug`] knows how to parse.
    UnparsableRemote { url: String },
    /// The repo has no `origin` remote configured at all.
    NoOriginRemote,
    /// A `gh` invocation exited non-zero for a reason that isn't one of the
    /// friendly cases above. `detail` is gh's own (truncated) stderr.
    Command { detail: String },
    /// `gh` produced output that didn't parse as the JSON (or plain text)
    /// shape a given call expects.
    InvalidResponse { detail: String },
    /// `gh pr create` failed because `branch` has no upstream on the
    /// remote yet — dv never auto-pushes, so this is surfaced back to the
    /// user instead.
    BranchNotPushed { branch: String },
}

impl fmt::Display for GhError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GhError::NotFound => write!(
                f,
                "gh (the GitHub CLI) was not found on this machine.\n\
                 install it from https://cli.github.com, or run `winget install GitHub.cli`,\n\
                 or set the DV_GH environment variable to its full path."
            ),
            GhError::NotAuthenticated { host } => {
                if host == "github.com" {
                    write!(f, "not authenticated with github.com — run `gh auth login`")
                } else {
                    write!(
                        f,
                        "not authenticated with {host} — run `gh auth login --hostname {host}`"
                    )
                }
            }
            GhError::UnparsableRemote { url } => write!(
                f,
                "origin remote ({url}) doesn't look like a GitHub URL dv understands"
            ),
            GhError::NoOriginRemote => write!(f, "repo has no \"origin\" remote configured"),
            GhError::Command { detail } => write!(f, "gh failed: {detail}"),
            GhError::InvalidResponse { detail } => write!(f, "unexpected gh output: {detail}"),
            GhError::BranchNotPushed { branch } => write!(
                f,
                "branch {branch:?} has no upstream on the remote — push it first \
                 (`git push -u origin {branch}`), then try again"
            ),
        }
    }
}

impl std::error::Error for GhError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_authenticated_message_mentions_hostname_flag_for_enterprise() {
        let err = GhError::NotAuthenticated {
            host: "ghe.corp.com".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("--hostname ghe.corp.com"));
    }

    #[test]
    fn not_authenticated_message_omits_hostname_flag_for_github_com() {
        let err = GhError::NotAuthenticated {
            host: "github.com".to_string(),
        };
        assert!(!err.to_string().contains("--hostname"));
    }

    #[test]
    fn not_found_message_has_install_instructions() {
        let msg = GhError::NotFound.to_string();
        assert!(msg.contains("https://cli.github.com"));
        assert!(msg.contains("winget install GitHub.cli"));
    }
}
