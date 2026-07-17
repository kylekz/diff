//! Parser for the GUI's "open anything" quick-open input (R2): one text
//! field that accepts a GitHub PR URL, `owner/repo#123`, a bare
//! `#123`/`123`, or a filesystem path (including
//! `\\wsl.localhost\<distro>\...` UNC paths). Two deliberate breadths:
//! any host is accepted in URL form (dv's [`super::RepoSlug`] is
//! enterprise-aware, not github.com-only), and non-PR input falls
//! through to a path target instead of erroring (the field opens repos
//! as well as PRs).
//!
//! Parsing only — no filesystem checks, no network, no repo resolution.
//! Whether a path exists, or which local clone can serve `owner/repo`, is
//! the caller's (app shell's) problem; keeping this pure keeps it
//! unit-testable and WSL-neutral.

/// What one line of quick-open input asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenTarget {
    /// A PR fully identified by host + owner/repo — a pasted PR URL or
    /// `owner/repo#123` (the latter always `github.com`; enterprise users
    /// paste the full URL, same convention as `RemoteRef::slug`).
    Pr {
        host: String,
        owner: String,
        repo: String,
        number: u64,
    },
    /// A bare `#123` or `123` — a PR in whatever repo is currently active.
    PrNumber(u64),
    /// Anything else: a filesystem path for the existing open-repo flow.
    Path(String),
}

/// Parse one quick-open input line. `Err` carries a user-facing message
/// for input that *committed* to a PR shape and then broke (a URL that
/// isn't a PR URL, `owner/repo#` with no number) — those must not fall
/// through to the path branch, where "open `https://github.com/...` as a
/// directory" would fail with a far more confusing error later. Blank
/// input is an error too (callers usually guard it first).
pub fn parse_open_target(input: &str) -> Result<OpenTarget, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("nothing to open".to_string());
    }

    // URL form: https://<host>/<owner>/<repo>/pull/<number>[/...]. A
    // scheme-less "github.com/..." is accepted as a convenience; other
    // hosts need the scheme to distinguish them from a relative path.
    let url_body = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        .or_else(|| input.starts_with("github.com/").then_some(input));
    if let Some(body) = url_body {
        let parts: Vec<&str> = body.split('/').collect();
        if parts.len() >= 5 && parts[3] == "pull" {
            // Tolerate trailing segments/anchors ("/files", "#discussion_…")
            // by taking leading digits only.
            let digits: String = parts[4].chars().take_while(char::is_ascii_digit).collect();
            if let Ok(number) = digits.parse::<u64>() {
                // Strip a `:port` — `RepoSlug::parse_remote_url` does the
                // same, so a stored `RemoteRef::slug` never carries one; a
                // ported enterprise URL could otherwise never match any
                // known clone (R2 review, P3).
                let host = parts[0].split(':').next().unwrap_or(parts[0]);
                return Ok(OpenTarget::Pr {
                    host: host.to_string(),
                    owner: parts[1].to_string(),
                    repo: parts[2].to_string(),
                    number,
                });
            }
        }
        return Err(format!("not a PR URL: {input}"));
    }

    // `owner/repo#123` / `#123`. Only treat `#` as the PR separator when
    // what follows parses as a number — a path can legally contain `#`.
    if let Some((repo_part, number)) = input.split_once('#')
        && let Ok(number) = number.parse::<u64>()
    {
        if repo_part.is_empty() {
            return Ok(OpenTarget::PrNumber(number));
        }
        if let Some((owner, repo)) = repo_part.split_once('/')
            && !owner.is_empty()
            && !repo.is_empty()
            && !repo.contains('/')
            && !repo_part.contains(['\\', ':', ' '])
        {
            return Ok(OpenTarget::Pr {
                host: "github.com".to_string(),
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
            });
        }
    }

    // Bare digits — the "PR number in the current repo" convention. A
    // directory literally named `123` loses to the PR reading — deliberate;
    // `./123` still opens it.
    if let Ok(number) = input.parse::<u64>() {
        return Ok(OpenTarget::PrNumber(number));
    }

    Ok(OpenTarget::Path(input.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(host: &str, owner: &str, repo: &str, number: u64) -> OpenTarget {
        OpenTarget::Pr {
            host: host.to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        }
    }

    #[test]
    fn parses_full_pr_url() {
        assert_eq!(
            parse_open_target("https://github.com/longbridge/gpui-component/pull/2554"),
            Ok(pr("github.com", "longbridge", "gpui-component", 2554))
        );
    }

    #[test]
    fn parses_pr_url_with_trailing_segments_and_anchor() {
        assert_eq!(
            parse_open_target("https://github.com/kylekz/difftest/pull/1/files#diff-abc"),
            Ok(pr("github.com", "kylekz", "difftest", 1))
        );
        // The anchor can also ride directly on the number.
        assert_eq!(
            parse_open_target("https://github.com/kylekz/difftest/pull/7#issuecomment-1"),
            Ok(pr("github.com", "kylekz", "difftest", 7))
        );
    }

    #[test]
    fn parses_schemeless_github_com_url() {
        assert_eq!(
            parse_open_target("github.com/kylekz/difftest/pull/3"),
            Ok(pr("github.com", "kylekz", "difftest", 3))
        );
    }

    #[test]
    fn parses_enterprise_host_url() {
        assert_eq!(
            parse_open_target("https://ghe.corp.com/team/svc/pull/42"),
            Ok(pr("ghe.corp.com", "team", "svc", 42))
        );
    }

    #[test]
    fn enterprise_url_port_is_stripped_to_match_stored_slugs() {
        // `RepoSlug::parse_remote_url` strips ports, so stored slugs never
        // carry one — the parsed host must match that convention.
        assert_eq!(
            parse_open_target("https://ghe.corp.com:8443/team/svc/pull/42"),
            Ok(pr("ghe.corp.com", "team", "svc", 42))
        );
    }

    #[test]
    fn non_pr_url_is_an_error_not_a_path() {
        assert!(parse_open_target("https://github.com/kylekz/difftest").is_err());
        assert!(parse_open_target("https://github.com/kylekz/difftest/issues/5").is_err());
        assert!(parse_open_target("https://github.com/kylekz/difftest/pull/notanumber").is_err());
    }

    #[test]
    fn parses_owner_repo_hash_number() {
        assert_eq!(
            parse_open_target("kylekz/difftest#1"),
            Ok(pr("github.com", "kylekz", "difftest", 1))
        );
    }

    #[test]
    fn parses_bare_hash_number_and_bare_number() {
        assert_eq!(parse_open_target("#123"), Ok(OpenTarget::PrNumber(123)));
        assert_eq!(parse_open_target("123"), Ok(OpenTarget::PrNumber(123)));
    }

    #[test]
    fn windows_and_unc_paths_are_paths() {
        assert_eq!(
            parse_open_target(r"D:\Software\diff"),
            Ok(OpenTarget::Path(r"D:\Software\diff".to_string()))
        );
        assert_eq!(
            parse_open_target(r"\\wsl.localhost\Ubuntu\home\kyle\proj"),
            Ok(OpenTarget::Path(
                r"\\wsl.localhost\Ubuntu\home\kyle\proj".to_string()
            ))
        );
        assert_eq!(
            parse_open_target("//wsl.localhost/Ubuntu/home/kyle/proj"),
            Ok(OpenTarget::Path(
                "//wsl.localhost/Ubuntu/home/kyle/proj".to_string()
            ))
        );
    }

    #[test]
    fn a_path_containing_hash_is_still_a_path() {
        // `#` only means "PR" when the tail parses as a number.
        assert_eq!(
            parse_open_target(r"D:\code\c#-samples"),
            Ok(OpenTarget::Path(r"D:\code\c#-samples".to_string()))
        );
        // ...and a `#<digits>` tail with path punctuation before it stays a
        // path too (a drive-letter colon or backslash disqualifies the
        // owner/repo shape).
        assert_eq!(
            parse_open_target(r"D:\code\issue#42"),
            Ok(OpenTarget::Path(r"D:\code\issue#42".to_string()))
        );
    }

    #[test]
    fn relative_path_stays_a_path() {
        assert_eq!(
            parse_open_target("refs/pr-test-gpui-component"),
            Ok(OpenTarget::Path("refs/pr-test-gpui-component".to_string()))
        );
    }

    #[test]
    fn whitespace_is_trimmed_and_blank_is_an_error() {
        assert_eq!(parse_open_target("  #7  "), Ok(OpenTarget::PrNumber(7)));
        assert!(parse_open_target("").is_err());
        assert!(parse_open_target("   ").is_err());
    }
}
