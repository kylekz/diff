//! Comment author resolution (docs/phase-3-github.md's backlog item,
//! folded into phase 3): what name to stamp a CLI-created comment/reply
//! with when the caller didn't pass `--author` explicitly.
//!
//! Each step degrades silently to the next — no stderr noise when offline
//! or unauthenticated, since an agent calling `comment add` repeatedly
//! shouldn't warn every time just because `gh` has no session:
//!
//!  1. `git config dv.author` — explicit override, works fully offline.
//!  2. the cached GitHub login (`gh-login-<host>`, a plain-text file next
//!     to `recent.json` in the app data dir, keyed by the repo's GitHub
//!     host so a GHE repo or a `gh auth switch` doesn't serve back a
//!     login cached for a different host) — fast, no `gh` subprocess.
//!  3. an in-memory, per-process memo of the same cache, keyed the same
//!     way — covers the case where the file cache write in step 3 below
//!     silently failed (no data dir, permissions, ...): without this, every
//!     single call in a long-lived process (the GUI, or any future batch
//!     CLI command) would otherwise re-pay the network round trip.
//!  4. `GithubClient::for_repo` + `current_login()` — hits `gh`; on success
//!     the login is cached for next time, both to the file (best-effort: a
//!     failed write is not surfaced) and to the in-process memo above.
//!  5. `git config user.name`.
//!  6. the literal string `"you"`.
//!
//! Deliberately takes only a [`GitRepo`] (no CLI-only types), so the GUI
//! can call it too once it's wired in there (a later task).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use dv_core::{GitRepo, GithubClient, RepoSlug};

pub fn resolve_author(repo: &GitRepo) -> String {
    if let Some(v) = repo.config("dv.author") {
        return v;
    }

    // Resolved via the routed git layer only (no `gh` subprocess) so the
    // cache lookups below stay as fast as the comment they replace.
    let host = repo
        .remote_url("origin")
        .ok()
        .and_then(|url| RepoSlug::parse_remote_url(&url).ok())
        .map(|slug| slug.host);

    if let Some(v) = read_cached_login(host.as_deref()) {
        return v;
    }
    if let Some(h) = &host
        && let Some(v) = memo_get(h)
    {
        return v;
    }
    if let Ok(client) = GithubClient::for_repo(repo)
        && let Ok(login) = client.current_login()
    {
        let host = client.slug().host.clone();
        write_cached_login(&host, &login);
        memo_set(&host, &login);
        return login;
    }
    if let Some(v) = repo.config("user.name") {
        return v;
    }
    "you".to_string()
}

/// Per-process memo of resolved GitHub logins, keyed by host — checked
/// after the file cache but before ever touching the network. Not a
/// replacement for the file cache (it doesn't survive past this process),
/// just a guard against paying the `gh api user` round trip repeatedly
/// within one long-lived run when the file cache isn't landing.
static LOGIN_MEMO: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn login_memo() -> &'static Mutex<HashMap<String, String>> {
    LOGIN_MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn memo_get(host: &str) -> Option<String> {
    login_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(host)
        .cloned()
}

fn memo_set(host: &str, login: &str) {
    login_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(host.to_string(), login.to_string());
}

fn cache_path(host: &str) -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("dv").join(format!("gh-login-{host}")))
}

fn read_cached_login(host: Option<&str>) -> Option<String> {
    let path = cache_path(host?)?;
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Best-effort: a failed write (no data dir, permissions, ...) is silently
/// ignored — the login was still resolved for this call, it just won't be
/// cached for the next one.
fn write_cached_login(host: &str, login: &str) {
    let Some(path) = cache_path(host) else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, login);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_path_is_keyed_by_host() {
        let a = cache_path("github.com").expect("data dir resolvable in test env");
        let b = cache_path("ghe.corp.com").expect("data dir resolvable in test env");
        assert_ne!(a, b, "different hosts must not share a cache file");
        assert!(
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("github.com")
        );
        assert!(
            b.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("ghe.corp.com")
        );
    }

    #[test]
    fn read_cached_login_is_none_without_a_resolved_host() {
        assert_eq!(read_cached_login(None), None);
    }

    #[test]
    fn login_memo_round_trips_per_host_without_touching_the_network() {
        // Distinctive, test-only host names — the memo is a shared static,
        // so a collision with another test's host would be a false pass.
        let host_a = "author-test-memo-a.invalid";
        let host_b = "author-test-memo-b.invalid";

        assert_eq!(memo_get(host_a), None, "must start empty for this host");

        memo_set(host_a, "kyle");
        assert_eq!(memo_get(host_a).as_deref(), Some("kyle"));
        assert_eq!(
            memo_get(host_b),
            None,
            "a different host must not see host_a's memoized login"
        );

        memo_set(host_a, "kylekz");
        assert_eq!(
            memo_get(host_a).as_deref(),
            Some("kylekz"),
            "re-setting the same host must overwrite, not accumulate"
        );
    }
}
