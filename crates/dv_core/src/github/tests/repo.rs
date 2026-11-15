//! Which GitHub repository a clone is (PLAN §9.1).

use std::collections::HashMap;

use dv_test_support::{FixtureGithub, FixtureRepo};
use serde_json::json;

use super::*;
use crate::github::gh::tests::MockGh;
use crate::github::repo::{Place, configured_remotes, parse_url, pick};
use crate::testing::{open, open_at};

/// A remote whose URL's host counts as GitHub's.
fn remote(name: &str, url: &str) -> Remote {
    Remote {
        name: name.to_string(),
        at: Place::Github(parse_url(url).unwrap()),
        resolved: None,
    }
}

/// A remote that isn't on GitHub: on another forge's `host`, or with no host (a local path).
fn elsewhere(name: &str, host: Option<&str>) -> Remote {
    Remote {
        name: name.to_string(),
        at: match host {
            Some(host) => Place::OtherHost(host.to_string()),
            None => Place::NoHost,
        },
        resolved: None,
    }
}

fn resolved(remote: Remote, resolved: &str) -> Remote {
    Remote {
        resolved: Some(resolved.to_string()),
        ..remote
    }
}

/// What [`pick`] chose, as `remote host/owner/repo`.
fn picked(remotes: &[Remote]) -> String {
    let repo = pick(remotes).unwrap();
    format!("{} {}", repo.remote, repo.selector())
}

/// The remotes [`GithubError::NoRepository`] names for `remotes`: the ones on GitHub, and
/// the others.
fn undecided(remotes: &[Remote]) -> (Vec<String>, Vec<String>) {
    match pick(remotes) {
        Err(GithubError::NoRepository { remotes, elsewhere }) => (
            remotes,
            elsewhere.into_iter().map(|other| other.remote).collect(),
        ),
        other => panic!("{other:?}"),
    }
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

/// A `gh` that holds a login for none of the hosts it is asked about, and says so.
fn gh_without_logins() -> MockGh {
    MockGh::new(json!([{
        "argv": ["auth", "status", "*"],
        "stderr": "You are not logged into any accounts on that host\n",
        "exit": 1
    }]))
}

/// The hosts `gh` was asked about, in order.
fn asked(mock: &MockGh) -> Vec<String> {
    mock.calls()
        .iter()
        .map(|call| {
            let host = call["argv"][2].as_str().unwrap();
            host.strip_prefix("--hostname=").unwrap().to_string()
        })
        .collect()
}

#[test]
fn github_urls_name_a_host_an_owner_and_a_repository() {
    let parsed =
        |url: &str| parse_url(url).map(|url| format!("{}/{}/{}", url.host, url.owner, url.repo));
    for (url, wanted) in [
        ("https://github.com/acme/api.git", "github.com/acme/api"),
        ("https://github.com/acme/api", "github.com/acme/api"),
        ("https://github.com/acme/api/", "github.com/acme/api"),
        ("https://GitHub.com/Acme/API.git", "github.com/Acme/API"),
        ("https://www.github.com/acme/api", "github.com/acme/api"),
        // GitHub's ssh on port 443, and anything else under github.com, is github.com.
        (
            "ssh://git@ssh.github.com:443/acme/api.git",
            "github.com/acme/api",
        ),
        ("git@ssh.github.com:acme/api.git", "github.com/acme/api"),
        ("ssh://github.com/acme/api", "github.com/acme/api"),
        // A scheme's own port says nothing; another stays as written, but an ssh URL's is
        // sshd's and never the host's.
        ("https://github.com:443/acme/api.git", "github.com/acme/api"),
        (
            "http://ghe.example.com:80/acme/api",
            "ghe.example.com/acme/api",
        ),
        (
            "https://ghe.example.com:80/acme/api",
            "ghe.example.com:80/acme/api",
        ),
        (
            "ssh://git@github.com:22/acme/api.git",
            "github.com/acme/api",
        ),
        // Another forge's URL has the same shape; whether its host counts is gh's to say.
        ("git@bitbucket.org:team/repo.git", "bitbucket.org/team/repo"),
        ("gh:acme/api", "gh/acme/api"),
        (
            "https://kylekz@github.com/acme/api.git",
            "github.com/acme/api",
        ),
        (
            "http://ghe.example.com:8080/acme/api.git",
            "ghe.example.com:8080/acme/api",
        ),
        ("git@github.com:acme/api.git", "github.com/acme/api"),
        ("git@github.com:acme/api", "github.com/acme/api"),
        ("git@github.com:/acme/api.git", "github.com/acme/api"),
        ("org-123@github.com:acme/api.git", "github.com/acme/api"),
        ("github.com:acme/api.git", "github.com/acme/api"),
        (
            "git@ghe.example.com:acme/api.git",
            "ghe.example.com/acme/api",
        ),
        ("ssh://git@github.com/acme/api.git", "github.com/acme/api"),
        (
            "ssh://git@ghe.example.com:2222/acme/api",
            "ghe.example.com/acme/api",
        ),
        (
            "git+ssh://git@github.com/acme/api.git",
            "github.com/acme/api",
        ),
        (
            "https://github.com/acme/.github.git",
            "github.com/acme/.github",
        ),
        ("https://github.com/acme/api.js", "github.com/acme/api.js"),
        ("  https://github.com/acme/api.git\n", "github.com/acme/api"),
    ] {
        assert_eq!(parsed(url).as_deref(), Some(wanted), "{url}");
    }
    for url in [
        "",
        "/srv/git/api.git",
        "../api.git",
        "file:///srv/git/acme/api.git",
        r"C:\repos\acme\api",
        "C:/repos/api",
        "D:acme/api",
        "https://github.com/acme",
        "https://github.com/acme/api/extra",
        "https://github.com//api",
        "https:///acme/api",
        "https://gitlab.com/group/sub/project.git",
        "git@github.com:acme",
        "git@github.com:acme/api/extra.git",
        "host with spaces:acme/api",
        // A host is a name and at most a port, so nothing hides behind a second colon.
        "https://evil.com:x.ghe.com/acme/api",
        "https://evil.com:1:x.ghe.com/acme/api",
        "https://ghe.example.com:/acme/api",
        "ssh://git@-oProxyCommand=x/acme/api",
        "-x.example.com:acme/api",
        "https://github.com/acme/a pi",
        "https://github.com/acme/..",
        "ftp://github.com/acme/api",
    ] {
        assert_eq!(parsed(url), None, "{url:?}");
    }
}

#[test]
fn rule_1_the_remote_gh_repo_set_default_marked_wins() {
    // `base` on a remote that rule 2 would never pick.
    assert_eq!(
        picked(&[
            remote("origin", "git@github.com:kylekz/api.git"),
            remote("upstream", "git@github.com:acme/api.git"),
            resolved(remote("mirror", "https://github.com/mirrors/api"), "base"),
        ]),
        "mirror github.com/mirrors/api"
    );
    // An `owner/repo` names one of the remotes' repositories, without case.
    assert_eq!(
        picked(&[
            resolved(
                remote("origin", "git@github.com:kylekz/api.git"),
                "Acme/API"
            ),
            remote("parent", "git@github.com:acme/api.git"),
        ]),
        "parent github.com/acme/api"
    );
    // The repository is looked for on the marked remote's host.
    assert_eq!(
        picked(&[
            remote("public", "https://github.com/acme/api.git"),
            resolved(
                remote("origin", "https://ghe.example.com/kylekz/api.git"),
                "acme/api"
            ),
            remote("work", "https://ghe.example.com/acme/api.git"),
        ]),
        "work ghe.example.com/acme/api"
    );
    // A mark on a remote that isn't on GitHub is no mark.
    assert_eq!(
        picked(&[
            resolved(elsewhere("local", None), "base"),
            remote("origin", "git@github.com:acme/api.git"),
        ]),
        "origin github.com/acme/api"
    );
}

#[test]
fn rule_1_a_default_no_remote_has_asks_for_a_remote() {
    // gh would use acme/api; dv has no remote to fetch its pull requests from, and origin's
    // pull requests are other pull requests.
    for (named, remotes) in [
        (
            "acme/api",
            vec![
                resolved(
                    remote("origin", "git@github.com:kylekz/api.git"),
                    "acme/api",
                ),
                remote("backup", "git@github.com:kylekz/api-backup.git"),
            ],
        ),
        // The same name on another host is another repository.
        (
            "acme/api",
            vec![
                resolved(
                    remote("origin", "git@github.com:kylekz/api.git"),
                    "acme/api",
                ),
                remote("work", "https://ghe.example.com/acme/api.git"),
            ],
        ),
    ] {
        let error = pick(&remotes).unwrap_err();
        assert!(
            matches!(&error, GithubError::NoRemote { repository } if repository == named),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "`gh repo set-default` names {named}, which no remote of this repository points \
                 at; add a remote for it (dv fetches a pull request from a remote)"
            )
        );
        // Running `gh repo set-default` again is what led here.
        assert_eq!(error.fix(), None);
        assert_eq!(error.kind(), "no_remote");
    }
}

#[test]
fn rule_1_a_mark_that_is_neither_base_nor_a_repository_is_unreadable() {
    for value in [
        "",
        "nonsense",
        "acme/",
        "/api",
        "acme/api/extra",
        "acme api/x",
    ] {
        let error = pick(&[
            remote("upstream", "git@github.com:acme/api.git"),
            resolved(remote("origin", "git@github.com:kylekz/api.git"), value),
        ])
        .unwrap_err();
        assert!(
            matches!(&error, GithubError::UnreadableDefault { remote, value: read }
                if remote == "origin" && read == value),
            "{value:?}: {error:?}"
        );
        assert_eq!(error.fix().as_deref(), Some("gh repo set-default"));
        assert_eq!(error.kind(), "unreadable_default");
    }
    let error = pick(&[resolved(
        remote("origin", "git@github.com:kylekz/api.git"),
        "",
    )])
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "the remote origin's `gh-resolved` mark (\"\") is neither `base` nor an `owner/repo`; \
         set it again with `gh repo set-default`"
    );
}

#[test]
fn rule_2_upstream_then_origin() {
    let origin = remote("origin", "git@github.com:kylekz/api.git");
    let upstream = remote("upstream", "https://github.com/acme/api.git");
    let other = remote("another", "https://github.com/someone/api.git");
    assert_eq!(
        picked(&[origin.clone(), other.clone(), upstream.clone()]),
        "upstream github.com/acme/api"
    );
    assert_eq!(
        picked(&[other.clone(), origin.clone()]),
        "origin github.com/kylekz/api"
    );
    // An `upstream` that isn't on GitHub is no candidate; origin is next.
    assert_eq!(
        picked(&[elsewhere("upstream", Some("gitlab.com")), origin, other]),
        "origin github.com/kylekz/api"
    );
}

#[test]
fn rule_3_the_only_github_remote_else_no_repository() {
    assert_eq!(
        picked(&[
            elsewhere("backup", None),
            remote("fork", "https://github.com/kylekz/api.git"),
        ]),
        "fork github.com/kylekz/api"
    );

    let several = [
        remote("mine", "https://github.com/kylekz/api.git"),
        elsewhere("backup", None),
        remote("fork", "https://github.com/someone/api.git"),
    ];
    assert_eq!(
        undecided(&several),
        (names(&["fork", "mine"]), names(&["backup"]))
    );
    let error = pick(&several).unwrap_err();
    assert_eq!(
        error.to_string(),
        "can't tell which of this repository's remotes (fork, mine) is its GitHub repository; \
         pick one with `gh repo set-default` (not on GitHub: backup)"
    );
    assert_eq!(error.fix().as_deref(), Some("gh repo set-default"));
    assert_eq!(error.kind(), "no_repository");

    let none = pick(&[]).unwrap_err();
    assert_eq!(undecided(&[]), (names(&[]), names(&[])));
    assert_eq!(none.to_string(), "this repository has no remotes");
    assert_eq!(none.fix(), None);
}

#[test]
fn no_repository_names_the_host_that_may_be_an_enterprise_one() {
    // One host gh has no login for: the words and the fix say how to sign in to it. A mark
    // doesn't make a candidate of a remote that isn't on GitHub.
    let one = pick(&[
        resolved(elsewhere("origin", Some("ghe.example.com")), "base"),
        elsewhere("fork", Some("ghe.example.com")),
        elsewhere("backup", None),
    ])
    .unwrap_err();
    assert_eq!(
        one.to_string(),
        "none of this repository's remotes is on GitHub: origin is on ghe.example.com, fork is \
         on ghe.example.com, backup has no GitHub URL; if that is GitHub Enterprise, `gh auth \
         login --hostname ghe.example.com`"
    );
    assert_eq!(
        one.fix().as_deref(),
        Some("gh auth login --hostname ghe.example.com")
    );
    assert!(matches!(
        &one,
        GithubError::NoRepository { remotes, elsewhere }
            if remotes.is_empty() && elsewhere[0] == Elsewhere {
                remote: "origin".to_string(),
                host: Some("ghe.example.com".to_string()),
            }
    ));

    // Several: dv can't say which to sign in to.
    let several = pick(&[
        elsewhere("origin", Some("gitlab.com")),
        elsewhere("work", Some("ghe.example.com")),
    ])
    .unwrap_err();
    assert_eq!(
        several.to_string(),
        "none of this repository's remotes is on GitHub: origin is on gitlab.com, work is on \
         ghe.example.com; for a host that is GitHub Enterprise, `gh auth login --hostname <host>`"
    );
    assert_eq!(several.fix(), None);

    // None: nothing to sign in to.
    let local = pick(&[elsewhere("backup", None)]).unwrap_err();
    assert_eq!(
        local.to_string(),
        "none of this repository's remotes is on GitHub: backup has no GitHub URL"
    );
    assert_eq!(local.fix(), None);
}

#[test]
fn remotes_are_read_from_the_clones_configuration() {
    let repo = FixtureRepo::new();
    let mock = gh_without_logins();
    assert_eq!(configured_remotes(&open(&repo)).unwrap(), []);
    assert!(matches!(
        GithubRepo::of(&open(&repo), &mock.gh),
        Err(GithubError::NoRepository { remotes, elsewhere })
            if remotes.is_empty() && elsewhere.is_empty()
    ));

    repo.config("remote.origin.url", "git@github.com:kylekz/api.git")
        .config("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
        .config(
            "remote.origin.pushurl",
            "git@github.com:kylekz/api-push.git",
        )
        // A name with dots, a second URL (a push mirror), and gh's mark.
        .config("remote.acme.main.url", "https://github.com/acme/api.git")
        .config("remote.acme.main.gh-resolved", "base")
        .config("branch.main.remote", "origin");
    repo.git(&[
        "config",
        "--add",
        "remote.acme.main.url",
        "https://example.com/acme/mirror.git",
    ]);
    assert_eq!(
        configured_remotes(&open(&repo)).unwrap(),
        [
            ConfiguredRemote {
                name: "origin".to_string(),
                url: Some("git@github.com:kylekz/api.git".to_string()),
                resolved: None,
            },
            ConfiguredRemote {
                name: "acme.main".to_string(),
                url: Some("https://github.com/acme/api.git".to_string()),
                resolved: Some("base".to_string()),
            },
        ]
    );
    let picked = GithubRepo::of(&open(&repo), &mock.gh).unwrap();
    assert_eq!(picked.selector(), "github.com/acme/api");
    assert_eq!(picked.remote, "acme.main");
    // Every remote is on github.com: gh was asked nothing.
    assert_eq!(asked(&mock), names(&[]));
}

#[test]
fn a_git_that_fails_is_a_backend_error() {
    let repo = FixtureRepo::new();
    let backend = open(&repo);
    std::fs::write(repo.path().join(".git/config"), "[remote \"origin\n").unwrap();
    let error = GithubError::from(configured_remotes(&backend).unwrap_err());
    assert!(
        matches!(
            &error,
            GithubError::Backend(crate::BackendError::Git { .. })
        ),
        "{error:?}"
    );
    assert_eq!(error.kind(), "git");
    assert!(error.detail().unwrap().starts_with("$ git config -z"));
    let error = GithubRepo::of(&backend, &gh_without_logins().gh).unwrap_err();
    assert!(matches!(error, GithubError::Backend(_)), "{error:?}");
}

#[test]
fn another_forges_remote_is_no_candidate() {
    // A GitLab `upstream` doesn't hide a GitHub `origin`.
    let repo = FixtureRepo::new();
    repo.config("remote.upstream.url", "https://gitlab.com/acme/api.git")
        .config("remote.origin.url", "git@github.com:kylekz/api.git");
    let mock = gh_without_logins();
    let picked = GithubRepo::of(&open(&repo), &mock.gh).unwrap();
    assert_eq!(picked.selector(), "github.com/kylekz/api");
    assert_eq!(picked.remote, "origin");
    // gh is asked about the host it might hold a login for, and again the next time dv
    // looks: the user may have signed in since.
    GithubRepo::of(&open(&repo), &mock.gh).unwrap();
    assert_eq!(asked(&mock), ["gitlab.com", "gitlab.com"]);

    // Nothing on GitHub at all.
    let repo = FixtureRepo::new();
    repo.config("remote.origin.url", "git@bitbucket.org:team/repo.git")
        .config("remote.backup.url", "/srv/git/repo.git");
    let mock = gh_without_logins();
    let error = GithubRepo::of(&open(&repo), &mock.gh).unwrap_err();
    assert_eq!(
        error.to_string(),
        "none of this repository's remotes is on GitHub: origin is on bitbucket.org, backup \
         has no GitHub URL; if that is GitHub Enterprise, `gh auth login --hostname bitbucket.org`"
    );
    assert_eq!(asked(&mock), ["bitbucket.org"]);
}

#[test]
fn gh_is_asked_about_a_host_once_in_one_reading() {
    // Two remotes on one host, neither URL rewritten: one question about it, not four.
    let repo = FixtureRepo::new();
    repo.config("remote.origin.url", "git@ghe.example.com:kylekz/api.git")
        .config(
            "remote.upstream.url",
            "https://ghe.example.com/acme/api.git",
        )
        // A URL that is rewritten to the same host asks nothing more either.
        .config("remote.mirror.url", "work:acme/mirror.git")
        .config("url.git@ghe.example.com:.insteadOf", "work:")
        // A default port is no other host.
        .config("remote.port.url", "https://github.com:443/acme/api.git");
    let mock = gh_without_logins();
    // github.com:443 is github.com, the one remote on GitHub.
    let picked = GithubRepo::of(&open(&repo), &mock.gh).unwrap();
    assert_eq!(picked.selector(), "github.com/acme/api");
    assert_eq!(picked.remote, "port");
    assert_eq!(asked(&mock), ["ghe.example.com", "work"]);
}

#[test]
fn an_enterprise_host_counts_when_gh_holds_a_login_for_it() {
    let repo = FixtureRepo::new();
    repo.config("remote.origin.url", "git@ghe.example.com:acme/api.git");
    let mock = MockGh::new(json!([
        {"argv": ["auth", "status", "--hostname=ghe.example.com"], "exit": 0}
    ]));
    let picked = GithubRepo::of(&open(&repo), &mock.gh).unwrap();
    assert_eq!(picked.selector(), "ghe.example.com/acme/api");
    // A yes is remembered for the life of the runner.
    GithubRepo::of(&open(&repo), &mock.gh).unwrap();
    assert_eq!(asked(&mock), ["ghe.example.com"]);

    // Without one it may be another forge: the error says how to sign in if it isn't.
    let error = GithubRepo::of(&open(&repo), &gh_without_logins().gh).unwrap_err();
    assert_eq!(
        error.to_string(),
        "none of this repository's remotes is on GitHub: origin is on ghe.example.com; if \
         that is GitHub Enterprise, `gh auth login --hostname ghe.example.com`"
    );
    assert_eq!(
        error.fix().as_deref(),
        Some("gh auth login --hostname ghe.example.com")
    );
}

#[test]
fn a_url_git_rewrites_is_read_as_configured_first_then_as_rewritten() {
    // The fixture's origin is a GitHub URL that git fetches from a local repository: the
    // configured URL counts, and gh is asked nothing.
    let github = FixtureGithub::new("api");
    let mock = gh_without_logins();
    let repo = GithubRepo::of(&open(github.checkout()), &mock.gh).unwrap();
    assert_eq!(repo, github_repo("github.com", "dv-fixture", "api"));
    assert_eq!(repo.name_with_owner(), "dv-fixture/api");
    assert_eq!(repo.selector(), "github.com/dv-fixture/api");
    assert!(repo.is("GitHub.com", "DV-Fixture", "API"));
    assert!(!repo.is("ghe.example.com", "dv-fixture", "api"));
    assert_eq!(asked(&mock), names(&[]));

    // A linked worktree is the same clone.
    let worktree = github.checkout().add_worktree("second", "second");
    let linked = open_at(github.checkout(), &worktree);
    assert_eq!(GithubRepo::of(&linked, &mock.gh).unwrap(), repo);

    // A shorthand the user's configuration expands: `gh` is no host, the rewritten URL's is.
    let shorthand = FixtureRepo::new();
    shorthand
        .config("remote.origin.url", "gh:acme/api")
        .config("url.git@github.com:.insteadOf", "gh:");
    let picked = GithubRepo::of(&open(&shorthand), &mock.gh).unwrap();
    assert_eq!(picked.selector(), "github.com/acme/api");
    assert_eq!(picked.remote, "origin");
    assert_eq!(asked(&mock), ["gh"]);
}

#[test]
fn a_repository_url_is_the_same_whatever_its_case() {
    let url = |text: &str| parse_url(text).unwrap();
    assert_eq!(
        url("https://github.com/Acme/API.git"),
        url("git@github.com:acme/api.git")
    );
    assert_ne!(
        url("https://github.com/acme/api"),
        url("https://ghe.example.com/acme/api")
    );
    let known = HashMap::from([(url("https://github.com/Acme/API"), "a clone")]);
    assert_eq!(
        known.get(&url("https://GITHUB.com/acme/api")),
        Some(&"a clone")
    );
    // The spelling the URL has stays.
    assert_eq!(url("https://github.com/Acme/API").owner, "Acme");
}

#[test]
fn a_repository_is_the_same_however_its_names_are_spelled() {
    let repo = github_repo("github.com", "Acme", "API");
    let lower = github_repo("github.com", "acme", "api");
    assert_eq!(repo, lower);
    // The spelling GitHub gave stays for display.
    assert_eq!(repo.name_with_owner(), "Acme/API");

    let mut by_repo = HashMap::new();
    by_repo.insert(repo.clone(), "first");
    by_repo.insert(lower, "second");
    assert_eq!(by_repo.len(), 1);
    assert_eq!(by_repo[&repo], "second");

    for other in [
        github_repo("ghe.example.com", "acme", "api"),
        github_repo("github.com", "acme", "web"),
        github_repo("github.com", "acm", "eapi"),
        GithubRepo {
            remote: "upstream".to_string(),
            ..repo.clone()
        },
    ] {
        assert_ne!(repo, other);
        assert!(!by_repo.contains_key(&other), "{other:?}");
    }
}

#[test]
fn every_host_a_remote_names_may_hold_a_mirror() {
    use crate::github::repo::remote_hosts;
    let repo = FixtureRepo::new();
    assert_eq!(remote_hosts(&open(&repo)).unwrap(), ["github.com"]);
    repo.config("remote.origin.url", "git@github.com:acme/api.git")
        .config("remote.work.url", "https://GHE.example.com/acme/api")
        .config("remote.mirror.url", "https://ghe.example.com/acme/api.git")
        .config("remote.local.url", "../elsewhere")
        // A shorthand git rewrites to a host of its own.
        .config("url.https://tenant.ghe.com/.insteadOf", "t:")
        .config("remote.short.url", "t:acme/api");
    let mut hosts = remote_hosts(&open(&repo)).unwrap();
    hosts.sort();
    assert_eq!(hosts, ["ghe.example.com", "github.com", "tenant.ghe.com"]);
}
