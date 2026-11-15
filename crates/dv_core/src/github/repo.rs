//! Which GitHub repository a clone is (docs/PLAN.md §9.1, D7): its remotes, read through its
//! backend so a WSL repository is read where it lives, and the remote whose repository pull
//! requests live in.
//!
//! **Which remotes are on GitHub.** A remote counts when its URL names a host, an owner and a
//! repository and nothing more ([`parse_url`]) and the host is GitHub's or a GitHub Enterprise
//! one ([`Gh::is_github_host`]: `github.com`, a `*.ghe.com` tenant, the host `GH_HOST` names,
//! or one gh holds a login for). The URL is read as configured; one that doesn't count is
//! read once more as git rewrites it (`git remote get-url`, which applies
//! `url.<base>.insteadOf` without reaching the network), so a `gh:acme/api` shorthand
//! resolves. A host alias of the user's ssh configuration (`git@work:acme/api.git`) is not
//! resolved. A remote that doesn't count is no candidate for any rule: another forge's
//! `upstream` never hides a GitHub `origin`, and gh is never pointed at a host it has no
//! business with. gh is asked about a host at most once in one reading of a clone.
//!
//! **Which of them** ([`pick`]):
//! 1. The remote `gh repo set-default` marked (`remote.<name>.gh-resolved`): `base` is that
//!    remote's repository; an `owner/repo` is the remote, on the marked remote's host, with
//!    that repository. One that no remote has is [`GithubError::NoRemote`], never another
//!    remote's repository: its pull requests would be other pull requests. A mark that is
//!    neither is [`GithubError::UnreadableDefault`].
//! 2. Else `upstream`, else `origin`.
//! 3. Else the only one. None, or several, is [`GithubError::NoRepository`].

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::backend::{Backend, BackendError, GitOpts};

use super::error::Elsewhere;
use super::{Gh, GithubError};

/// GitHub's own host, for which `gh` needs no `--hostname`.
pub const GITHUB_COM: &str = "github.com";

/// A clone's repository on GitHub and the remote that points at it (PLAN §9.1).
///
/// Two are equal when they name the same repository through the same remote: GitHub's hosts
/// and names have no case, so equality and the hash fold it, while the fields keep the
/// spelling the remote's URL has.
#[derive(Debug, Clone)]
pub struct GithubRepo {
    /// `github.com` or a GitHub Enterprise host, lowercase: `gh`'s `--hostname`.
    pub host: String,
    pub owner: String,
    pub repo: String,
    /// The git remote, which `git fetch` takes a pull request's commits from.
    pub remote: String,
}

impl GithubRepo {
    /// The repository of the clone `backend` is bound to. `gh` says which hosts are GitHub's:
    /// it is asked about a host at most once here, and never about `github.com`.
    pub fn of(backend: &dyn Backend, gh: &Gh) -> Result<Self, GithubError> {
        // What gh said about each host, so a second remote there, or the same URL read again
        // as git rewrites it, doesn't ask again (a gh that hangs costs one timeout a host).
        let mut counts: HashMap<String, bool> = HashMap::new();
        let mut place = |url: &str| match parse_url(url) {
            None => Place::NoHost,
            Some(repo) => {
                let host = repo.host.clone();
                if *counts
                    .entry(host.clone())
                    .or_insert_with(|| gh.is_github_host(&host))
                {
                    Place::Github(repo)
                } else {
                    Place::OtherHost(host)
                }
            }
        };
        let mut remotes = Vec::new();
        for remote in configured_remotes(backend)? {
            let at = match remote.url.as_deref() {
                None => Place::NoHost,
                Some(url) => match place(url) {
                    at @ Place::Github(_) => at,
                    configured => {
                        let rewritten = rewritten_url(backend, &remote.name)?;
                        if rewritten == url {
                            configured
                        } else {
                            // Not on GitHub either way, it is on the host its own URL names.
                            match (place(&rewritten), configured) {
                                (at @ Place::Github(_), _) => at,
                                (rewritten, Place::NoHost) => rewritten,
                                (_, configured) => configured,
                            }
                        }
                    }
                },
            };
            remotes.push(Remote {
                name: remote.name,
                at,
                resolved: remote.resolved,
            });
        }
        pick(&remotes)
    }

    /// `owner/repo`, as GitHub writes a repository.
    pub fn name_with_owner(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// `host/owner/repo`: what `gh -R` takes, naming the repository whatever the working
    /// directory (PLAN §9.2).
    pub fn selector(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.repo)
    }

    /// Whether this is the repository `owner/repo` on `host`, whatever the remote. GitHub
    /// compares all three without case.
    pub fn is(&self, host: &str, owner: &str, repo: &str) -> bool {
        self.host.eq_ignore_ascii_case(host)
            && self.owner.eq_ignore_ascii_case(owner)
            && self.repo.eq_ignore_ascii_case(repo)
    }
}

impl PartialEq for GithubRepo {
    fn eq(&self, other: &Self) -> bool {
        self.is(&other.host, &other.owner, &other.repo) && self.remote == other.remote
    }
}

impl Eq for GithubRepo {}

impl Hash for GithubRepo {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for name in [&self.host, &self.owner, &self.repo] {
            hash_folded(name, state);
        }
        self.remote.hash(state);
    }
}

/// Hashes `name` as its lowercase spelling, to agree with an equality that ignores ASCII
/// case.
pub(super) fn hash_folded<H: Hasher>(name: &str, state: &mut H) {
    for byte in name.bytes() {
        state.write_u8(byte.to_ascii_lowercase());
    }
    // As `str`'s own hash ends: `ab`, `c` and `a`, `bc` differ.
    state.write_u8(0xff);
}

/// One remote as the clone's git configuration has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredRemote {
    pub name: String,
    /// `remote.<name>.url`, the first when there are several.
    pub url: Option<String>,
    /// `remote.<name>.gh-resolved`, which `gh repo set-default` writes.
    pub resolved: Option<String>,
}

/// One remote and where it points, as [`pick`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub at: Place,
    /// `remote.<name>.gh-resolved`.
    pub resolved: Option<String>,
}

/// Where a remote points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// At a repository on GitHub.
    Github(RepoUrl),
    /// At a URL of a GitHub URL's shape on a host that isn't GitHub's: another forge, or a
    /// GitHub Enterprise host gh has no login for.
    OtherHost(String),
    /// Anywhere else: a local path, a URL of another shape, no URL at all.
    NoHost,
}

/// The host, owner and repository a GitHub URL names. Two are equal whatever the case of
/// their names, as [`GithubRepo`] is.
#[derive(Debug, Clone)]
pub struct RepoUrl {
    pub host: String,
    pub owner: String,
    pub repo: String,
}

impl PartialEq for RepoUrl {
    fn eq(&self, other: &Self) -> bool {
        self.host.eq_ignore_ascii_case(&other.host)
            && self.owner.eq_ignore_ascii_case(&other.owner)
            && self.repo.eq_ignore_ascii_case(&other.repo)
    }
}

impl Eq for RepoUrl {}

impl Hash for RepoUrl {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for name in [&self.host, &self.owner, &self.repo] {
            hash_folded(name, state);
        }
    }
}

/// The clone's remotes, in the order its configuration lists them (`git config -z
/// --get-regexp '^remote\..*\.(url|gh-resolved)$'`).
pub fn configured_remotes(backend: &dyn Backend) -> Result<Vec<ConfiguredRemote>, BackendError> {
    const ARGS: &[&str] = &[
        "config",
        "-z",
        "--get-regexp",
        r"^remote\..*\.(url|gh-resolved)$",
    ];
    // Exit 1: no key matches, a repository without remotes.
    let output = backend.git(ARGS, &GitOpts::new().with_ok_codes(&[0, 1]))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut remotes: Vec<ConfiguredRemote> = Vec::new();
    // `-z` prints each entry as `<key>\n<value>\0`; the key's section and variable are
    // lowercase, its subsection (the remote's name, which may hold dots) as written.
    for entry in text.split_terminator('\0') {
        let Some((key, value)) = entry.split_once('\n') else {
            continue;
        };
        let Some((name, variable)) = key
            .strip_prefix("remote.")
            .and_then(|rest| rest.rsplit_once('.'))
        else {
            continue;
        };
        let index = match remotes.iter().position(|remote| remote.name == name) {
            Some(index) => index,
            None => {
                remotes.push(ConfiguredRemote {
                    name: name.to_string(),
                    url: None,
                    resolved: None,
                });
                remotes.len() - 1
            }
        };
        let remote = &mut remotes[index];
        match variable {
            "url" if remote.url.is_none() => remote.url = Some(value.to_string()),
            "gh-resolved" => remote.resolved = Some(value.to_string()),
            _ => {}
        }
    }
    Ok(remotes)
}

/// A remote's URL as git fetches from it, `url.<base>.insteadOf` applied (`git remote
/// get-url`, which reads the configuration alone).
fn rewritten_url(backend: &dyn Backend, remote: &str) -> Result<String, BackendError> {
    let output = backend.git(&["remote", "get-url", "--", remote], &GitOpts::new())?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Every host a remote of the clone names, as configured or as git rewrites its URL, and
/// `github.com`, each once: the hosts a pull request's mirror may be kept under (PLAN §9.4),
/// read without asking gh which of them are GitHub's.
pub(crate) fn remote_hosts(backend: &dyn Backend) -> Result<Vec<String>, BackendError> {
    let mut hosts = vec![GITHUB_COM.to_string()];
    for remote in configured_remotes(backend)? {
        let Some(url) = remote.url else {
            continue;
        };
        for url in [rewritten_url(backend, &remote.name)?, url] {
            if let Some(repo) = parse_url(&url)
                && !hosts.contains(&repo.host)
            {
                hosts.push(repo.host);
            }
        }
    }
    Ok(hosts)
}

/// The remote whose repository pull requests live in, by the three rules (module docs).
pub fn pick(remotes: &[Remote]) -> Result<GithubRepo, GithubError> {
    // The remotes on GitHub: `upstream`, then `origin`, then the rest by name.
    let mut candidates: Vec<(&Remote, &RepoUrl)> = remotes
        .iter()
        .filter_map(|remote| match &remote.at {
            Place::Github(repo) => Some((remote, repo)),
            Place::OtherHost(_) | Place::NoHost => None,
        })
        .collect();
    let rank = |name: &str| match name {
        "upstream" => 0,
        "origin" => 1,
        _ => 2,
    };
    candidates.sort_by(|(a, _), (b, _)| {
        (rank(&a.name), a.name.as_str()).cmp(&(rank(&b.name), b.name.as_str()))
    });
    let repo = |(remote, url): &(&Remote, &RepoUrl)| GithubRepo {
        host: url.host.clone(),
        owner: url.owner.clone(),
        repo: url.repo.clone(),
        remote: remote.name.clone(),
    };

    let marked = candidates
        .iter()
        .find_map(|candidate| Some((candidate, candidate.0.resolved.as_deref()?)));
    match marked {
        Some((candidate, "base")) => return Ok(repo(candidate)),
        Some(((remote, on), named)) => {
            let Some((owner, name)) = named
                .split_once('/')
                .filter(|(owner, name)| is_name(owner) && is_name(name))
            else {
                return Err(GithubError::UnreadableDefault {
                    remote: remote.name.clone(),
                    value: named.to_string(),
                });
            };
            let wanted = RepoUrl {
                host: on.host.clone(),
                owner: owner.to_string(),
                repo: name.to_string(),
            };
            return candidates
                .iter()
                .find(|(_, url)| **url == wanted)
                .map(repo)
                .ok_or_else(|| GithubError::NoRemote {
                    repository: named.to_string(),
                });
        }
        None => {}
    }
    match candidates.as_slice() {
        [first, ..] if rank(&first.0.name) < 2 => Ok(repo(first)),
        [only] => Ok(repo(only)),
        _ => Err(GithubError::NoRepository {
            remotes: candidates
                .iter()
                .map(|(remote, _)| remote.name.clone())
                .collect(),
            elsewhere: remotes
                .iter()
                .filter_map(|remote| {
                    let host = match &remote.at {
                        Place::Github(_) => return None,
                        Place::OtherHost(host) => Some(host.clone()),
                        Place::NoHost => None,
                    };
                    Some(Elsewhere {
                        remote: remote.name.clone(),
                        host,
                    })
                })
                .collect(),
        }),
    }
}

/// Whether `host` is GitHub's without asking gh (PLAN §9.1): `github.com`, a `*.ghe.com`
/// tenant, or the host `GH_HOST` names (`gh_host`). `host` is as [`parse_url`] gives it.
pub(super) fn is_known_host(host: &str, gh_host: Option<&str>) -> bool {
    host == GITHUB_COM
        || host.ends_with(".ghe.com")
        || gh_host.is_some_and(|named| normalize_host(named) == host)
}

/// The host and repository a remote's URL names, when it has the shape of a GitHub URL
/// (PLAN §9.1): `https://<host>/<owner>/<repo>[.git]` (or `http://`),
/// `[<user>@]<host>:<owner>/<repo>[.git]`, or `ssh://[<user>@]<host>[:<port>]/<owner>/<repo>[.git]`
/// (or `git+ssh://`). A local path, or a URL with more or fewer than two path components, is
/// none. Whether the host is GitHub's is [`Gh::is_github_host`]'s to say.
pub fn parse_url(url: &str) -> Option<RepoUrl> {
    let url = url.trim();
    let (host, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_once('/')?;
            let default_port = match scheme.to_ascii_lowercase().as_str() {
                "https" => Some("443"),
                "http" => Some("80"),
                "ssh" | "git+ssh" => None,
                _ => return None,
            };
            (url_host(authority, default_port)?, path)
        }
        // scp-style, `[user@]host:path`. A `/` before the colon is a local path, and so is a
        // one-letter host: a Windows drive.
        None => {
            let (authority, path) = url.split_once(':')?;
            if authority.contains(['/', '\\']) {
                return None;
            }
            let host = url_host(authority, None).filter(|host| host.len() > 1)?;
            (host, path.strip_prefix('/').unwrap_or(path))
        }
    };
    let path = path.strip_suffix('/').unwrap_or(path);
    let (owner, repo) = path.strip_suffix(".git").unwrap_or(path).split_once('/')?;
    (is_name(owner) && is_name(repo)).then(|| RepoUrl {
        host,
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

/// The host a URL's authority (`[<user>@]<host>[:<port>]`) names, as gh knows it
/// ([`normalize_host`]): without the user, and without the port when it is the scheme's
/// `default_port`. `None` for `default_port` is ssh, whose port is always dropped: it is
/// sshd's, and says nothing of the host gh talks to (`ssh.github.com:443` is `github.com`).
pub(super) fn url_host(authority: &str, default_port: Option<&str>) -> Option<String> {
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (name, port) = split_host(host)?;
    Some(
        match port.filter(|port| default_port.is_some_and(|default| default != *port)) {
            Some(port) => normalize_host(&format!("{name}:{port}")),
            None => normalize_host(name),
        },
    )
}

/// `text` as a host's name and its port, when it is `<name>[:<digits>]` and the name is
/// letters, digits, `-` and `.` after a letter or digit: nothing an argument could be
/// mistaken for an option by, and no second `:` to hide another host behind.
fn split_host(text: &str) -> Option<(&str, Option<&str>)> {
    let (name, port) = match text.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (text, None),
    };
    let named = name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'));
    let numbered =
        port.is_none_or(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    (named && numbered).then_some((name, port))
}

/// Whether `text` can be a host, with or without a port.
pub(super) fn is_host(text: &str) -> bool {
    split_host(text).is_some()
}

/// A host as `gh` names it: lowercase, without `www.`, and `github.com` for every host under
/// it (`ssh.github.com`, GitHub's ssh on port 443). A port stays.
pub(super) fn normalize_host(host: &str) -> String {
    let host = host.to_ascii_lowercase();
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (name, format!(":{port}")),
        None => (host.as_str(), String::new()),
    };
    let name = if name.ends_with(".github.com") {
        GITHUB_COM
    } else {
        name.strip_prefix("www.").unwrap_or(name)
    };
    format!("{name}{port}")
}

/// Whether `text` can be an owner's or a repository's name: GitHub's are ASCII letters,
/// digits, `-`, `_` and `.`.
pub(super) fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text != "."
        && text != ".."
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}
