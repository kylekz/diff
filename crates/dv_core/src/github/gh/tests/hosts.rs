//! Which hosts are GitHub's (PLAN §9.1) and whether gh is signed in (PLAN §12.2).

use super::*;
use crate::github::repo::is_known_host;

/// An `auth status --hostname=<host>` entry that exits `exit` after saying `stderr`.
fn login(host: &str, exit: u8, stderr: &str) -> Value {
    json!({
        "argv": ["auth", "status", format!("--hostname={host}")],
        "stderr": stderr,
        "exit": exit
    })
}

#[test]
fn githubs_own_hosts_and_gh_host_need_no_asking() {
    for host in ["github.com", "acme.ghe.com", "eu.acme.ghe.com"] {
        assert!(is_known_host(host, None), "{host}");
    }
    for host in [
        "ghe.com",
        "notghe.com",
        "evilgithub.com",
        "github.com.evil.example",
        "gitlab.com",
        "bitbucket.org",
        "ghe.example.com",
        "gh",
    ] {
        assert!(!is_known_host(host, None), "{host}");
        assert!(!is_known_host(host, Some("other.example.com")), "{host}");
    }
    // `GH_HOST` names a host however it is spelled.
    assert!(is_known_host("ghe.example.com", Some("GHE.Example.com")));
    assert!(is_known_host(
        "ghe.example.com:8443",
        Some("ghe.example.com:8443")
    ));
    assert!(!is_known_host("ghe.example.com", Some("")));

    // None of them starts gh.
    let mock = MockGh::with(json!([]), |options| {
        options
            .env
            .push(("GH_HOST".into(), Some("ghe.example.com".into())));
    });
    for host in ["github.com", "acme.ghe.com", "ghe.example.com"] {
        assert!(mock.gh.is_github_host(host), "{host}");
    }
    assert_eq!(mock.calls(), [json!(null); 0]);
}

#[test]
fn another_host_counts_only_with_a_login_gh_holds() {
    let mock = MockGh::new(json!([
        login(
            "ghe.example.com",
            0,
            "ghe.example.com\n  ✓ Logged in to ghe.example.com account kylekz (keyring)\n"
        ),
        // A login gh holds but couldn't check is a login.
        login(
            "old.example.com",
            1,
            "old.example.com\n  X Failed to log in to old.example.com account kylekz (keyring)\n  \
             - The token in keyring is invalid.\n  - To re-authenticate, run: gh auth login -h \
             old.example.com\n"
        ),
        login(
            "slow.example.com",
            1,
            "slow.example.com\n  X Timeout trying to log in to slow.example.com account kylekz (keyring)\n"
        ),
        login(
            "gitlab.com",
            1,
            "You are not logged into any accounts on gitlab.com\n"
        ),
    ]));
    for _ in 0..2 {
        assert!(mock.gh.is_github_host("ghe.example.com"));
        // However the host is spelled.
        assert!(mock.gh.is_github_host("GHE.Example.com"));
        assert!(mock.gh.is_github_host("old.example.com"));
        assert!(mock.gh.is_github_host("slow.example.com"));
        assert!(!mock.gh.is_github_host("gitlab.com"));
        // `mock_gh` has no entry for it: any exit but 0 without a login's words is no.
        assert!(!mock.gh.is_github_host("bitbucket.org"));
    }
    // What can't be a host is never asked about.
    for text in [
        "",
        "--hostname",
        "-x.example.com",
        "evil.com:x.ghe.com",
        "a b",
        "host:",
    ] {
        assert!(!mock.gh.is_github_host(text), "{text:?}");
    }
    let asked: Vec<Value> = mock
        .calls()
        .iter()
        .map(|call| call["argv"].clone())
        .collect();
    // A yes is remembered; a no is asked again, so a sign-in made since is seen.
    assert_eq!(
        asked,
        [
            "ghe.example.com",
            "old.example.com",
            "slow.example.com",
            "gitlab.com",
            "bitbucket.org",
            "gitlab.com",
            "bitbucket.org"
        ]
        .map(|host| json!(["auth", "status", format!("--hostname={host}")]))
    );
}

#[test]
fn a_sign_in_made_meanwhile_is_seen_without_a_restart() {
    let mock = MockGh::new(json!([
        {
            "argv": ["auth", "status", "--hostname=ghe.example.com"],
            "stderr": "You are not logged into any accounts on ghe.example.com\n",
            "exit": 1,
            "times": 1
        },
        {"argv": ["auth", "status", "--hostname=ghe.example.com"], "exit": 0}
    ]));
    assert!(!mock.gh.is_github_host("ghe.example.com"));
    // The user runs `gh auth login --hostname ghe.example.com`.
    assert!(mock.gh.is_github_host("ghe.example.com"));
    assert!(mock.gh.is_github_host("ghe.example.com"));
    assert_eq!(mock.calls().len(), 2);
}

#[test]
fn a_gh_that_cannot_answer_says_no_and_is_asked_again() {
    let dir = tempfile::tempdir().unwrap();
    let gh = Gh::new(GhOptions::new(
        dir.path().join("no-such-gh").into_os_string(),
    ));
    assert!(!gh.is_github_host("ghe.example.com"));
    assert!(gh.is_github_host("github.com"));

    let mock = MockGh::with(
        json!([
            {"argv": ["auth", "**"], "exit": 0, "delay_ms": 30_000, "times": 1},
            {"argv": ["auth", "**"], "exit": 0}
        ]),
        // Far less than the first answer takes, and room for the second, which comes at
        // once, on a machine busy with the other tests.
        |options| options.timeout = Duration::from_secs(3),
    );
    assert!(!mock.gh.is_github_host("ghe.example.com"));
    assert!(mock.gh.is_github_host("ghe.example.com"));
    assert_eq!(mock.calls().len(), 2);
}

#[test]
fn only_a_stored_login_answers_for_a_host() {
    let gh = Gh::new(GhOptions::new("gh".into()));
    let removed = |call: &Call<'_>| -> Vec<String> {
        gh.command(call)
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect()
    };
    // A token in the environment would answer for any host at all.
    let asked = auth_status_call(Some("ghe.example.com"));
    assert_eq!(asked.args, ["auth", "status", "--hostname=ghe.example.com"]);
    assert_eq!(
        removed(&asked),
        [
            "GH_FORCE_TTY",
            "CLICOLOR_FORCE",
            "GH_DEBUG",
            "DEBUG",
            "GH_ENTERPRISE_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN"
        ]
    );
    // The setup check's question is about every login, the environment's included.
    let whole = auth_status_call(None);
    assert_eq!(whole.args, ["auth", "status"]);
    assert_eq!(removed(&whole), DROPPED_ENV);
}

#[test]
fn auth_status_is_what_gh_said_however_it_exited() {
    let mock = MockGh::new(json!([
        {
            "argv": ["auth", "status"],
            "stderr": "You are not logged into any GitHub hosts. To log in, run: gh auth login\n",
            "exit": 1,
            "times": 1
        },
        {
            "argv": ["auth", "status"],
            "stdout": "github.com\n  ✓ Logged in to github.com account kylekz (keyring)\n"
        }
    ]));
    let signed_out = mock.gh.auth_status().unwrap();
    assert_eq!(signed_out.code, Some(1));
    assert_eq!(signed_out.stdout, "");
    assert!(says_signed_out(&signed_out.stderr));
    assert_eq!(
        mock.gh.auth_status().unwrap(),
        AuthStatus {
            code: Some(0),
            stdout: "github.com\n  ✓ Logged in to github.com account kylekz (keyring)\n"
                .to_string(),
            stderr: String::new(),
        }
    );
    assert_eq!(mock.calls()[0]["argv"], json!(["auth", "status"]));

    let dir = tempfile::tempdir().unwrap();
    let gh = Gh::new(GhOptions::new(
        dir.path().join("no-such-gh").into_os_string(),
    ));
    let missing = gh.auth_status().unwrap_err();
    assert_eq!(missing.kind(), "gh_missing");
    assert_eq!(
        missing.to_string(),
        "gh isn't installed: dv reaches GitHub through it (https://cli.github.com)"
    );
}

#[test]
fn only_gh_saying_so_is_signed_out() {
    assert!(says_signed_out(
        "You are not logged into any GitHub hosts. To log in, run: gh auth login"
    ));
    assert!(says_signed_out(
        "You are not logged in to any GitHub hosts."
    ));
    assert!(says_signed_out(
        "github.com\n  X Failed to log in to github.com account kylekz (keyring)\n  - To \
         re-authenticate, run: gh auth login -h github.com"
    ));
    assert!(!says_signed_out("error connecting to api.github.com"));
    assert!(!says_signed_out(
        "mock_gh: no scenario entry for argv [\"auth\", \"status\"]"
    ));
}
