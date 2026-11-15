//! GitHub's CLI (docs/PLAN.md D7, §12.2 item 6): `gh` there (`DV_GH` names another program,
//! `mock_gh` in tests) and signed in (`gh auth status`). It is asked through the runner
//! everything else reaches GitHub with (`dv_core::github::Gh`, PLAN §9.2): the same
//! environment, the same words for a `gh` that isn't installed, the same reading of "not
//! signed in". dv never signs in itself: the fix is `gh auth login`. Inside a WSL distro this
//! is the distro's own `gh`, a separate sign-in.

use std::time::Duration;

use dv_core::github::{Gh, GhOptions, GithubError, says_signed_out};

use super::{Check, Fix, Seams, State, id};

const TITLE: &str = "GitHub CLI";

/// How long `gh auth status` may take: it asks GitHub whether the token still works.
const TIMEOUT: Duration = Duration::from_secs(15);

pub(super) fn check(seams: &Seams) -> Check {
    let gh = Gh::new(GhOptions {
        env: seams.child_env.clone(),
        timeout: TIMEOUT,
        ..GhOptions::new(seams.gh.clone())
    });
    match gh.auth_status() {
        Err(error @ GithubError::GhMissing { .. }) => {
            Check::new(id::GH, TITLE, State::Missing, error.to_string())
        }
        Err(GithubError::Unavailable { reason, .. }) => Check::new(
            id::GH,
            TITLE,
            State::Failed,
            format!("gh auth status failed: {reason}"),
        ),
        // "gh auth status failed: couldn't start …"
        Err(error) => Check::new(id::GH, TITLE, State::Failed, error.to_string()),
        Ok(status) if status.code == Some(0) => {
            let said = format!("{}\n{}", status.stdout, status.stderr);
            let detail = match account(&said) {
                Some((host, account)) => format!("signed in to {host} as {account}"),
                None => "signed in".to_string(),
            };
            Check::new(id::GH, TITLE, State::Ok, detail)
        }
        Ok(status) => {
            let said = format!("{}\n{}", status.stderr, status.stdout);
            if says_signed_out(&said) {
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
}
