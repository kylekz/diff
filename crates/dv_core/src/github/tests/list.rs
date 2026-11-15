//! The PR picker's list (PLAN §9.9): the two `gh pr list` calls and what is made of them.

use dv_test_support::fixture_gh;
use serde_json::json;

use super::*;
use crate::github::gh::tests::MockGh;
use crate::github::list::{LIST_FIELDS, REVIEW_REQUESTED};

#[test]
fn the_list_is_two_calls_the_requested_pull_requests_first() {
    let repo = github_repo("github.com", "acme", "api");
    let asks = fixture_gh::listed(9, "Asks for me", "hubot", true, "2026-10-18T08:30:00.5Z");
    let asks_too = fixture_gh::listed(
        4,
        "Asks for me too",
        "octocat",
        false,
        "2026-10-01T00:00:00Z",
    );
    // An old one that asks for a review, past the open ones gh lists.
    let old = fixture_gh::listed(1, "Still waiting", "mona", false, "2025-01-01T00:00:00Z");
    let mock = MockGh::new(json!(fixture_gh::pr_list(
        vec![
            fixture_gh::listed(12, "Newest", "octocat", false, "2026-10-19T08:30:00Z"),
            asks.clone(),
            fixture_gh::listed(7, "Oldest", "octocat", false, "not a time"),
            asks_too.clone(),
        ],
        vec![asks_too, asks, old],
    )));

    let listing = list(&mock.gh, &repo).unwrap();
    assert!(listing.requested.is_none());
    let prs = listing.prs;
    assert_eq!(
        prs.iter()
            .map(|pr| (pr.number, pr.review_requested))
            .collect::<Vec<_>>(),
        // The ones asking for a review in the order gh lists them, the one past the open
        // list among them; then the rest of the open ones, each once.
        [(4, true), (9, true), (1, true), (12, false), (7, false)]
    );
    assert_eq!(
        prs[1],
        ListedPr {
            number: 9,
            title: "Asks for me".to_string(),
            author: "hubot".to_string(),
            draft: true,
            updated_at: Some(1_792_312_200_500),
            review_requested: true,
        }
    );
    assert_eq!(prs[2].title, "Still waiting");
    assert_eq!(prs[4].updated_at, None, "a time dv can't read is none");

    let calls = mock.calls();
    let argv: Vec<Vec<&str>> = calls
        .iter()
        .map(|call| {
            call["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|arg| arg.as_str().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(
        argv,
        [
            vec![
                "pr",
                "list",
                "--json",
                LIST_FIELDS,
                "--limit",
                "50",
                "--repo=github.com/acme/api"
            ],
            vec![
                "pr",
                "list",
                "--search",
                REVIEW_REQUESTED,
                "--json",
                LIST_FIELDS,
                "--limit",
                "50",
                "--repo=github.com/acme/api"
            ],
        ]
    );
}

#[test]
fn a_deleted_author_is_ghost_and_a_bot_loses_its_prefix() {
    let repo = github_repo("github.com", "acme", "api");
    let mock = MockGh::new(json!(fixture_gh::pr_list(
        vec![
            json!({"number": 3, "title": "Bump", "author": {"is_bot": true, "login": "app/dependabot"}, "isDraft": false, "updatedAt": "2026-10-19T08:30:00Z"}),
            json!({"number": 2, "title": "Gone", "author": null, "isDraft": false, "updatedAt": "2026-10-19T08:30:00Z"}),
            json!({"number": 1}),
        ],
        Vec::new(),
    )));
    let prs = list(&mock.gh, &repo).unwrap().prs;
    assert_eq!(
        prs.iter()
            .map(|pr| (pr.number, pr.author.as_str(), pr.title.as_str()))
            .collect::<Vec<_>>(),
        [
            (3, "dependabot", "Bump"),
            (2, "ghost", "Gone"),
            (1, "ghost", "")
        ]
    );
}

#[test]
fn a_gh_that_fails_or_answers_something_else_fails_the_list() {
    let repo = github_repo("github.com", "acme", "api");
    let signed_out = MockGh::new(json!([fixture_gh::signed_out()]));
    assert_eq!(
        list(&signed_out.gh, &repo).unwrap_err().kind(),
        "not_authenticated"
    );
    assert_eq!(signed_out.calls().len(), 1, "the second call isn't made");

    let odd = MockGh::new(json!([
        {"argv": ["pr", "list", "--search", "**"], "stdout": []},
        {"argv": ["pr", "list", "--json", "**"], "stdout": {"message": "no"}},
    ]));
    assert_eq!(
        list(&odd.gh, &repo).unwrap_err().kind(),
        "unexpected_output"
    );

    // A host that isn't GitHub's is never asked.
    let elsewhere = github_repo("gitlab.example", "acme", "api");
    let mock = MockGh::new(json!([{"argv": ["auth", "status", "**"], "exit": 1}]));
    assert_eq!(list(&mock.gh, &elsewhere).unwrap_err().kind(), "not_github");
}

#[test]
fn the_open_ones_list_unmarked_when_the_ones_asking_for_a_review_cant_be_read() {
    let repo = github_repo("github.com", "acme", "api");
    let open = vec![
        fixture_gh::listed(12, "Newest", "octocat", false, "2026-10-19T08:30:00Z"),
        fixture_gh::listed(7, "Oldest", "octocat", false, "2026-10-18T08:30:00Z"),
    ];
    // GitHub's search out of reach; an answer of another shape.
    for (search, kind) in [
        (
            json!({"argv": ["pr", "list", "--search", "**"], "stderr": "HTTP 502: Bad Gateway\n", "exit": 1}),
            "unavailable",
        ),
        (
            json!({"argv": ["pr", "list", "--search", "**"], "stdout": {"message": "no"}}),
            "unexpected_output",
        ),
    ] {
        let mock = MockGh::new(json!([
            search,
            {"argv": ["pr", "list", "--json", "**"], "stdout": open},
        ]));
        let listing = list(&mock.gh, &repo).unwrap();
        assert_eq!(
            listing
                .prs
                .iter()
                .map(|pr| (pr.number, pr.review_requested))
                .collect::<Vec<_>>(),
            [(12, false), (7, false)]
        );
        assert_eq!(listing.requested.map(|error| error.kind()), Some(kind));
    }
}

#[test]
fn a_pull_requests_pages_are_built_from_its_key() {
    let repo = github_repo("github.com", "Acme", "api.rs");
    let key = MirrorKey::new(&repo, 482, "kylekz");
    assert_eq!(
        key.pr_url().as_deref(),
        Some("https://github.com/Acme/api.rs/pull/482")
    );
    assert_eq!(
        key.checks_url().as_deref(),
        Some("https://github.com/Acme/api.rs/pull/482/checks")
    );
    let port = MirrorKey::new(&github_repo("ghe.example:8443", "acme", "api"), 1, "me");
    assert_eq!(
        port.pr_url().as_deref(),
        Some("https://ghe.example:8443/acme/api/pull/1")
    );
    // A key that names no GitHub host or repository (a file edited by hand) has no page.
    for (host, owner, name) in [
        ("evil.example/x?", "acme", "api"),
        ("user@github.com", "acme", "api"),
        ("github.com", "acme/../x", "api"),
        ("github.com", "acme", "api#frag"),
        ("github.com", "", "api"),
        ("github.com", "acme", "a b"),
    ] {
        let key = MirrorKey::new(&github_repo(host, owner, name), 1, "me");
        assert_eq!(key.pr_url(), None, "{host} {owner} {name}");
    }
}

#[test]
fn a_remembered_repository_matches_without_case_and_reads_only_when_well_formed() {
    let repo: RepoUrl =
        serde_json::from_value(json!({"host": "github.com", "owner": "Acme", "repo": "API"}))
            .unwrap();
    assert!(repo.is_well_formed());
    assert!(repo.is(Some("GitHub.com"), "acme", "api"));
    assert!(repo.is(None, "ACME", "Api"));
    assert!(!repo.is(Some("ghe.example"), "acme", "api"));
    assert!(!repo.is(None, "acme", "web"));
    assert_eq!(repo.name_with_owner(), "Acme/API");
    assert_eq!(
        serde_json::to_value(&repo).unwrap(),
        json!({"host": "github.com", "owner": "Acme", "repo": "API"})
    );
    let odd: RepoUrl =
        serde_json::from_value(json!({"host": "github.com", "owner": "a\nb", "repo": "api"}))
            .unwrap();
    assert!(!odd.is_well_formed());
}
