//! [`RepoSlug`]: a parsed `(host, owner, repo)` identity for a GitHub (or
//! GitHub Enterprise) repository, derived from the `origin` remote's URL —
//! never from user input directly, so `gh -R <slug>` always targets the
//! repo dv actually has open.

use std::fmt;

use super::error::GhError;

/// A GitHub repo identity: which host (github.com or a GHE hostname), and
/// the `owner/repo` pair on it. [`Display`] renders `host/owner/repo`,
/// which `gh ... -R <arg>` accepts for both github.com and enterprise
/// hosts (see docs/phase-3-github.md and CLAUDE.md's design notes) — one
/// code path, no github.com special case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSlug {
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl fmt::Display for RepoSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.host, self.owner, self.repo)
    }
}

impl RepoSlug {
    /// Parse an `origin` remote URL into a [`RepoSlug`]. Handles the four
    /// shapes git actually produces for GitHub remotes:
    ///
    /// - `git@github.com:owner/repo.git` (SSH, scp-like shorthand)
    /// - `https://github.com/owner/repo(.git)`
    /// - `ssh://git@github.com/owner/repo.git`
    /// - `git@ghe.corp.com:owner/repo.git` (enterprise — host preserved
    ///   verbatim, not assumed to be github.com)
    ///
    /// A trailing `.git` is optional and stripped either way.
    pub fn parse_remote_url(url: &str) -> Result<Self, GhError> {
        let url = url.trim();
        let unparsable = || GhError::UnparsableRemote {
            url: url.to_string(),
        };

        let (host, path) = if let Some(rest) = url.strip_prefix("ssh://") {
            // ssh://[user@]host[:port]/owner/repo(.git)
            let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
            let (host, path) = rest.split_once('/').ok_or_else(unparsable)?;
            let host = host.split_once(':').map(|(h, _)| h).unwrap_or(host);
            (host, path)
        } else if let Some(rest) = url.strip_prefix("https://") {
            // https://[userinfo@]host[:port]/owner/repo(.git) — userinfo
            // shows up for `https://x-access-token:<token>@github.com/...`
            // remotes (gh itself rewrites remotes to that shape sometimes).
            let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
            let (host, path) = rest.split_once('/').ok_or_else(unparsable)?;
            let host = host.split_once(':').map(|(h, _)| h).unwrap_or(host);
            (host, path)
        } else if let Some(rest) = url.strip_prefix("http://") {
            let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
            let (host, path) = rest.split_once('/').ok_or_else(unparsable)?;
            let host = host.split_once(':').map(|(h, _)| h).unwrap_or(host);
            (host, path)
        } else if let Some(at_pos) = url.find('@') {
            // scp-like shorthand: user@host:owner/repo(.git)
            let rest = &url[at_pos + 1..];
            rest.split_once(':').ok_or_else(unparsable)?
        } else {
            return Err(unparsable());
        };

        if host.is_empty() {
            return Err(unparsable());
        }

        // Normalize the host the same way `gh` itself does
        // (NormalizeHostname): lowercase, and collapse the SSH-over-HTTPS
        // alias `ssh.github.com` to `github.com` so `-R <slug>` and the
        // login cache key line up regardless of which form the remote uses.
        let host = host.to_lowercase();
        let host = if host == "ssh.github.com" {
            "github.com".to_string()
        } else {
            host
        };

        let path = path.trim_matches('/').trim_end_matches(".git");
        let (owner, repo) = path.split_once('/').ok_or_else(unparsable)?;
        if owner.is_empty() || repo.is_empty() || repo.contains('/') {
            return Err(unparsable());
        }

        Ok(RepoSlug {
            host,
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slug(host: &str, owner: &str, repo: &str) -> RepoSlug {
        RepoSlug {
            host: host.to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
        }
    }

    #[test]
    fn parses_scp_like_ssh_url() {
        assert_eq!(
            RepoSlug::parse_remote_url("git@github.com:kylekz/difftest.git").unwrap(),
            slug("github.com", "kylekz", "difftest")
        );
    }

    #[test]
    fn parses_https_url_with_and_without_dot_git() {
        assert_eq!(
            RepoSlug::parse_remote_url("https://github.com/kylekz/difftest.git").unwrap(),
            slug("github.com", "kylekz", "difftest")
        );
        assert_eq!(
            RepoSlug::parse_remote_url("https://github.com/kylekz/difftest").unwrap(),
            slug("github.com", "kylekz", "difftest")
        );
    }

    #[test]
    fn parses_ssh_scheme_url() {
        assert_eq!(
            RepoSlug::parse_remote_url("ssh://git@github.com/kylekz/difftest.git").unwrap(),
            slug("github.com", "kylekz", "difftest")
        );
    }

    #[test]
    fn preserves_enterprise_host() {
        assert_eq!(
            RepoSlug::parse_remote_url("git@ghe.corp.com:owner/repo.git").unwrap(),
            slug("ghe.corp.com", "owner", "repo")
        );
        assert_eq!(
            RepoSlug::parse_remote_url("https://ghe.corp.com/owner/repo").unwrap(),
            slug("ghe.corp.com", "owner", "repo")
        );
    }

    #[test]
    fn display_is_host_owner_repo() {
        assert_eq!(
            slug("github.com", "kylekz", "difftest").to_string(),
            "github.com/kylekz/difftest"
        );
    }

    #[test]
    fn rejects_unparsable_urls() {
        assert!(RepoSlug::parse_remote_url("not a url").is_err());
        assert!(RepoSlug::parse_remote_url("https://github.com/onlyowner").is_err());
        assert!(RepoSlug::parse_remote_url("git@github.com:owner").is_err());
        assert!(RepoSlug::parse_remote_url("").is_err());
    }

    #[test]
    fn ssh_url_with_port_strips_port_from_host() {
        assert_eq!(
            RepoSlug::parse_remote_url("ssh://git@ghe.corp.com:22/owner/repo.git").unwrap(),
            slug("ghe.corp.com", "owner", "repo")
        );
    }

    #[test]
    fn uppercase_host_is_lowercased() {
        assert_eq!(
            RepoSlug::parse_remote_url("https://GitHub.COM/kylekz/difftest").unwrap(),
            slug("github.com", "kylekz", "difftest")
        );
    }

    #[test]
    fn https_url_with_userinfo_strips_it_from_host() {
        assert_eq!(
            RepoSlug::parse_remote_url("https://x-access-token:tok@github.com/o/r.git").unwrap(),
            slug("github.com", "o", "r")
        );
    }

    #[test]
    fn ssh_over_https_alias_normalizes_to_github_com() {
        assert_eq!(
            RepoSlug::parse_remote_url("ssh://git@ssh.github.com:443/o/r.git").unwrap(),
            slug("github.com", "o", "r")
        );
    }

    #[test]
    fn https_url_with_port_strips_it_but_keeps_enterprise_host() {
        assert_eq!(
            RepoSlug::parse_remote_url("https://ghe.corp:8443/o/r").unwrap(),
            slug("ghe.corp", "o", "r")
        );
    }
}
