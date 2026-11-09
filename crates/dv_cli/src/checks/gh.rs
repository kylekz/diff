//! GitHub's CLI (docs/PLAN.md D7, §12.2 item 6): `gh` there (`DV_GH` names another program,
//! `mock_gh` in tests) and signed in (`gh auth status`, run with `GH_PROMPT_DISABLED=1`). dv
//! never signs in itself: the fix is `gh auth login`. Inside a WSL distro this is the distro's
//! own `gh`, a separate sign-in.

use std::time::Duration;

use toolbox_process::Command;

use super::process::{self, apply_env};
use super::{Check, Fix, Seams, State, id};

const TITLE: &str = "GitHub CLI";

/// How long `gh auth status` may take: it asks GitHub whether the token still works.
const TIMEOUT: Duration = Duration::from_secs(15);

pub(super) fn check(seams: &Seams) -> Check {
    let command = apply_env(
        Command::new(&seams.gh).args(["auth", "status"]),
        &seams.child_env,
    )
    .env("GH_PROMPT_DISABLED", "1");
    match process::run(command, TIMEOUT) {
        Err(error) if process::not_found(&error) => Check::new(
            id::GH,
            TITLE,
            State::Missing,
            "gh isn't installed: dv reaches GitHub through it (https://cli.github.com)",
        ),
        Err(error) => Check::new(
            id::GH,
            TITLE,
            State::Failed,
            format!("gh auth status failed: {error}"),
        ),
        Ok(ran) if ran.success() => {
            let said = format!("{}\n{}", ran.stdout, ran.stderr);
            let detail = match account(&said) {
                Some((host, account)) => format!("signed in to {host} as {account}"),
                None => "signed in".to_string(),
            };
            Check::new(id::GH, TITLE, State::Ok, detail)
        }
        Ok(ran) => {
            let said = format!("{}\n{}", ran.stderr, ran.stdout);
            if signed_out(&said) {
                Check::new(
                    id::GH,
                    TITLE,
                    State::Missing,
                    "gh isn't signed in to GitHub",
                )
                .with_fix(Fix::command("gh auth login"))
            } else {
                // Offline, a timeout, a broken install: gh's own words, not a sign-in.
                let first = said
                    .lines()
                    .map(|line| line.trim().trim_start_matches(['X', '✗', '!']).trim())
                    .find(|line| !line.is_empty() && *line != "github.com")
                    .unwrap_or("it printed nothing")
                    .to_string();
                Check::new(
                    id::GH,
                    TITLE,
                    State::Failed,
                    format!("gh auth status failed: {first}"),
                )
            }
        }
    }
}

/// Whether what `gh auth status` said is that nobody is signed in (or the sign-in must be made
/// again): "You are not logged into any GitHub hosts", "… run: gh auth login".
fn signed_out(said: &str) -> bool {
    let said = said.to_lowercase();
    said.contains("not logged in")
        || said.contains("not logged into")
        || said.contains("gh auth login")
}

/// The host and account `gh auth status` names: "Logged in to github.com account kylekz
/// (keyring)", or an older gh's "Logged in to github.com as kylekz".
fn account(said: &str) -> Option<(String, String)> {
    said.lines().find_map(|line| {
        let rest = line.split("Logged in to ").nth(1)?;
        let mut words = rest.split_whitespace();
        let host = words.next()?.to_string();
        let account = match words.next()? {
            "account" | "as" => words.next()?,
            _ => return None,
        };
        Some((host, account.trim_end_matches(['.', ',']).to_string()))
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn the_account_is_read_from_either_gh() {
        let new = "github.com\n  ✓ Logged in to github.com account kylekz (keyring)\n  - Active account: true\n";
        assert_eq!(account(new), Some(("github.com".into(), "kylekz".into())));
        let old = "github.com\n  ✓ Logged in to github.com as kylekz (oauth_token)\n";
        assert_eq!(account(old), Some(("github.com".into(), "kylekz".into())));
        assert_eq!(account("You are not logged into any GitHub hosts."), None);
    }

    #[test]
    fn only_gh_saying_so_is_signed_out() {
        assert!(signed_out(
            "You are not logged into any GitHub hosts. To log in, run: gh auth login"
        ));
        assert!(signed_out(
            "github.com\n  X Failed to log in to github.com account kylekz (keyring)\n  - To \
             re-authenticate, run: gh auth login -h github.com"
        ));
        assert!(!signed_out("error connecting to api.github.com"));
        assert!(!signed_out(
            "mock_gh: no scenario entry for argv [\"auth\", \"status\"]"
        ));
    }
}
