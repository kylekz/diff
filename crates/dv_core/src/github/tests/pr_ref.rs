//! How a pull request is named (PLAN §9.3).

use super::*;
use crate::review::PrLink;

fn short(owner: &str, repo: &str, number: u64) -> PrRef {
    PrRef::Short {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    }
}

fn url(host: &str, owner: &str, repo: &str, number: u64) -> PrRef {
    PrRef::Url {
        host: host.to_string(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    }
}

#[test]
fn every_spelling_parses() {
    for (text, wanted) in [
        ("482", PrRef::Number(482)),
        ("#482", PrRef::Hash(482)),
        ("  #482\n", PrRef::Hash(482)),
        ("acme/api#482", short("acme", "api", 482)),
        ("Acme/API.js#1", short("Acme", "API.js", 1)),
        ("acme/.github#7", short("acme", ".github", 7)),
        (
            "https://github.com/acme/api/pull/482",
            url("github.com", "acme", "api", 482),
        ),
        (
            "https://github.com/acme/api/pull/482/",
            url("github.com", "acme", "api", 482),
        ),
        (
            "https://github.com/acme/api/pull/482/files",
            url("github.com", "acme", "api", 482),
        ),
        (
            "https://github.com/acme/api/pull/482/commits/0123abc",
            url("github.com", "acme", "api", 482),
        ),
        (
            "https://github.com/acme/api/pull/482/files?diff=split&w=1#diff-0123",
            url("github.com", "acme", "api", 482),
        ),
        (
            "https://github.com/acme/api/pull/482#discussion_r1234567",
            url("github.com", "acme", "api", 482),
        ),
        (
            "https://GitHub.com/Acme/API/pull/482",
            url("github.com", "Acme", "API", 482),
        ),
        (
            "http://ghe.example.com:8080/acme/api/pull/3",
            url("ghe.example.com:8080", "acme", "api", 3),
        ),
        (
            "https://www.github.com/acme/api/pull/482",
            url("github.com", "acme", "api", 482),
        ),
        (
            "https://github.com:443/acme/api/pull/482",
            url("github.com", "acme", "api", 482),
        ),
    ] {
        assert_eq!(PrRef::parse(text), Some(wanted.clone()), "{text:?}");
        assert_eq!(text.parse::<PrRef>(), Ok(wanted), "{text:?}");
    }
}

#[test]
fn a_reference_parses_as_people_paste_it() {
    let pr = url("github.com", "acme", "api", 482);
    for text in [
        // Without its scheme, as a browser's address bar shows it.
        "github.com/acme/api/pull/482",
        "github.com/acme/api/pull/482/files",
        "www.github.com/acme/api/pull/482#discussion_r1",
        // The pull request as a diff or a patch.
        "https://github.com/acme/api/pull/482.diff",
        "https://github.com/acme/api/pull/482.patch",
        // In brackets, and at the end of a sentence.
        "<https://github.com/acme/api/pull/482>",
        "<https://github.com/acme/api/pull/482>.",
        "https://github.com/acme/api/pull/482)",
        "https://github.com/acme/api/pull/482.",
        "https://github.com/acme/api/pull/482,",
        "https://github.com/acme/api/pull/482/files). ",
        "\thttps://github.com/acme/api/pull/482\r\n",
    ] {
        assert_eq!(PrRef::parse(text), Some(pr.clone()), "{text:?}");
    }
    for (text, wanted) in [
        ("#482.", PrRef::Hash(482)),
        ("#482)", PrRef::Hash(482)),
        ("acme/api#482,", short("acme", "api", 482)),
        ("<acme/api#482>", short("acme", "api", 482)),
        ("482.", PrRef::Number(482)),
        (
            "ghe.example.com/acme/api/pull/3",
            url("ghe.example.com", "acme", "api", 3),
        ),
    ] {
        assert_eq!(PrRef::parse(text), Some(wanted), "{text:?}");
    }
}

#[test]
fn what_names_no_pull_request_is_refused() {
    for text in [
        "",
        "#",
        "0",
        "#0",
        "+482",
        "-482",
        "4 82",
        "482abc",
        "#482abc",
        "99999999999999999999999",
        "acme#482",
        "acme/api",
        "acme/api#",
        "acme/api/extra#482",
        "acme/a pi#482",
        "/api#482",
        "acme/#482",
        "acme/api#482#1",
        "https://github.com/acme/api",
        "https://github.com/acme/api/pulls",
        "https://github.com/acme/api/pull",
        "https://github.com/acme/api/pull/",
        "https://github.com/acme/api/pull/new",
        "https://github.com/acme/api/issues/482",
        "https://github.com/acme/pull/482",
        "https:///acme/api/pull/482",
        "ssh://git@github.com/acme/api/pull/482",
        // Without a scheme, only a host that reads as one and the whole path.
        "acme/api/pull/482",
        "github.com/acme/api",
        "github.com/acme/api/pulls/482",
        "github.com/acme/pull/482",
        "github.com/pull/482",
        // What may be something else is left alone.
        "(https://github.com/acme/api/pull/482)",
        "https://github.com/acme/api/pull/482.txt",
        "https://github.com/acme/api/pull/482.diff.patch",
        "https://evil.com:x.github.com/acme/api/pull/482",
        "<#482",
        "#482>",
    ] {
        assert_eq!(PrRef::parse(text), None, "{text:?}");
    }
    assert_eq!(
        "acme/api".parse::<PrRef>().unwrap_err().to_string(),
        "not a pull request (a number, #482, owner/repo#482 or its URL): \"acme/api\""
    );
}

#[test]
fn a_reference_is_the_clones_pull_request_or_another_repositorys() {
    let clone = github_repo("github.com", "Acme", "API");
    let link = |number| PrLink {
        owner: "Acme".to_string(),
        repo: "API".to_string(),
        number,
    };
    // The clone's own spelling of its names is the one kept.
    for reference in [
        PrRef::Number(482),
        PrRef::Hash(482),
        short("acme", "api", 482),
        url("github.com", "acme", "api", 482),
    ] {
        assert_eq!(reference.number(), 482);
        assert_eq!(
            reference.in_repo(&clone).unwrap(),
            link(482),
            "{reference:?}"
        );
    }

    let other = |reference: PrRef| match reference.in_repo(&clone) {
        Err(error @ GithubError::OtherRepository { .. }) => error.to_string(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        other(short("acme", "web", 482)),
        "#482 is in acme/web; open it from a clone of that repository"
    );
    assert_eq!(
        other(url("github.com", "someone", "api", 7)),
        "#7 is in someone/api; open it from a clone of that repository"
    );
    // The same name on another host is another repository, told apart by the host.
    assert_eq!(
        other(url("ghe.example.com", "acme", "api", 7)),
        "#7 is in ghe.example.com/acme/api; open it from a clone of that repository"
    );

    // `owner/repo#n` has no host: it is read on the clone's.
    let enterprise = github_repo("ghe.example.com", "acme", "api");
    assert_eq!(
        short("acme", "api", 3).in_repo(&enterprise).unwrap().number,
        3
    );
    assert!(
        url("github.com", "acme", "api", 3)
            .in_repo(&enterprise)
            .is_err()
    );
}
