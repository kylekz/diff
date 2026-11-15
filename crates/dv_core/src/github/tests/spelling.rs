//! One spelling for a GitHub name in a store path and in a ref (PLAN §9.3, §9.4).

use std::collections::HashSet;

use dv_test_support::FixtureRepo;

use super::*;

/// Names GitHub allows that a ref or a Windows file name doesn't, and names made to collide
/// with their spellings.
const NAMES: &[&str] = &[
    "api",
    "API.js",
    "a_b-c.d",
    ".github",
    "_github",
    "%2egithub",
    "..github",
    "a..b",
    "a...b",
    "x.lock",
    "x_lock",
    "x%2elock",
    ".lock",
    "x.lock.d",
    "x.LOCK",
    "trailing.",
    "trailing..",
    "con",
    "CON",
    "con.d",
    "console",
    "nul",
    "com1",
    "lpt9.txt",
    "ghe.example.com:8443",
    "dependabot[bot]",
    "100%",
    "a b",
    "tilde~caret^colon:",
    "question?star*bracket[",
    "back\\slash",
    "at@{brace",
    "é",
    "-leading-dash",
];

#[test]
fn a_name_is_lowercase_with_what_is_unsafe_escaped() {
    for (name, wanted) in [
        ("api", "api"),
        ("API.js", "api.js"),
        ("a_b-c.d", "a_b-c.d"),
        (".github", "%2egithub"),
        ("_github", "_github"),
        ("%2egithub", "%252egithub"),
        ("a..b", "a.%2eb"),
        ("a...b", "a.%2e%2eb"),
        ("x.lock", "x%2elock"),
        ("x.LOCK", "x%2elock"),
        (".lock", "%2elock"),
        ("x.lock.d", "x.lock.d"),
        ("trailing.", "trailing%2e"),
        ("con", "%63on"),
        ("con.d", "%63on.d"),
        ("console", "console"),
        ("lpt9.txt", "%6cpt9.txt"),
        ("ghe.example.com:8443", "ghe.example.com%3a8443"),
        ("dependabot[bot]", "dependabot%5bbot%5d"),
        ("100%", "100%25"),
        ("é", "%c3%a9"),
    ] {
        assert_eq!(spell(name), wanted, "{name}");
    }
}

#[test]
fn two_names_share_a_spelling_only_when_they_differ_by_case() {
    let spelled: HashSet<String> = NAMES.iter().map(|name| spell(name)).collect();
    let folded: HashSet<String> = NAMES.iter().map(|name| name.to_ascii_lowercase()).collect();
    assert_eq!(spelled.len(), folded.len());
    assert_eq!(spell("Acme"), spell("acme"));
    // The pairs a looser rule would merge.
    for (a, b) in [
        (".github", "_github"),
        (".github", "%2egithub"),
        ("x.lock", "x_lock"),
        ("x.lock", "x%2elock"),
        ("con", "%63on"),
    ] {
        assert_ne!(spell(a), spell(b), "{a} and {b}");
    }
}

#[test]
fn every_spelling_is_a_ref_component_and_a_store_name() {
    let repo = FixtureRepo::new();
    let valid = |name: &str| {
        repo.git_command()
            .args([
                "check-ref-format",
                &format!("refs/dv/pr/{name}/{name}/7/head"),
            ])
            .status()
            .unwrap()
            .success()
    };
    // What the rule is for: git refuses these as they are.
    for name in [".github", "x.lock", "a..b"] {
        assert!(!valid(name), "{name}");
    }
    let backend = crate::testing::open(&repo);
    for name in NAMES {
        let spelled = spell(name);
        assert!(valid(&spelled), "{name} as {spelled}");
        assert!(
            spelled
                .chars()
                .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '-' | '%')),
            "{name} as {spelled}"
        );
        // No Windows file name ends in a dot, and none is a device's.
        assert!(
            !spelled.ends_with('.') && !spelled.starts_with('.'),
            "{spelled}"
        );
        assert!(
            !["con", "nul", "com1", "lpt9"].contains(&spelled.split('.').next().unwrap()),
            "{spelled}"
        );
        crate::Backend::store_write_atomic(
            &backend,
            &format!("github/{spelled}/{spelled}.json"),
            b"{}",
        )
        .unwrap_or_else(|err| panic!("{name} as {spelled}: {err}"));
    }
}
