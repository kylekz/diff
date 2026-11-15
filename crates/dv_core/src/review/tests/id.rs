//! Id generation and the strict parser (PLAN §7).

use super::*;

const ULID: &str = "01j9z3k4m5n6p7q8r9s0t1v2w3";

#[test]
fn generated_ids_are_lowercase_ulids_with_their_kind_letter() {
    let ids = [
        ReviewId::generate().to_string(),
        ThreadId::generate().to_string(),
        CommentId::generate().to_string(),
        EventId::generate().to_string(),
    ];
    for (id, prefix) in ids.iter().zip(["r-", "t-", "c-", "e-"]) {
        assert!(id.starts_with(prefix), "{id}");
        assert_eq!(id.len(), 28, "{id}");
        assert_eq!(id, &id.to_ascii_lowercase(), "{id}");
    }
    ReviewId::parse(&ids[0]).unwrap();
    ThreadId::parse(&ids[1]).unwrap();
    CommentId::parse(&ids[2]).unwrap();
    EventId::parse(&ids[3]).unwrap();
    // Unique, and ordered by creation time to the millisecond.
    let a = ReviewId::generate();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let b = ReviewId::generate();
    assert!(a < b, "{a} < {b}");
    assert_ne!(ReviewId::generate(), ReviewId::generate());
}

#[test]
fn the_parser_accepts_exactly_kind_dash_ulid() {
    assert_eq!(
        ReviewId::parse(&format!("r-{ULID}")).unwrap().as_str(),
        format!("r-{ULID}")
    );
    let bad = [
        "../x".to_string(),
        "r-../../../etc/passwd".to_string(),
        format!("r-{}", ULID.to_ascii_uppercase()),
        format!("R-{ULID}"),
        format!("t-{ULID}"),
        format!("r_{ULID}"),
        format!("r-{ULID}x"),
        format!("r-{}", &ULID[1..]),
        format!("r-{ULID}/"),
        format!(" r-{ULID}"),
        "r-".to_string(),
        String::new(),
        // Crockford base32 has no i, l, o or u.
        format!("r-{}i", &ULID[..25]),
        format!("r-{}l", &ULID[..25]),
        format!("r-{}o", &ULID[..25]),
        format!("r-{}u", &ULID[..25]),
        // 26 digits of base32 are 130 bits; a ULID's first digit is at most 7.
        format!("r-8{}", &ULID[1..]),
        format!("r-z{}", &ULID[1..]),
    ];
    for text in &bad {
        let err = ReviewId::parse(text).unwrap_err();
        assert_eq!(err.kind, "review");
        assert!(text.parse::<ReviewId>().is_err(), "{text:?}");
    }
    assert!(ReviewId::parse(&format!("r-7{}", &ULID[1..])).is_ok());
}

#[test]
fn each_kind_checks_its_own_letter() {
    assert!(ThreadId::parse(&format!("t-{ULID}")).is_ok());
    assert!(ThreadId::parse(&format!("c-{ULID}")).is_err());
    assert!(CommentId::parse(&format!("c-{ULID}")).is_ok());
    assert!(CommentId::parse(&format!("t-{ULID}")).is_err());
    assert!(EventId::parse(&format!("e-{ULID}")).is_ok());
    assert!(EventId::parse(&format!("r-{ULID}")).is_err());

    let thread = ItemId::parse(&format!("t-{ULID}")).unwrap();
    assert!(matches!(thread, ItemId::Thread(_)));
    let comment = ItemId::parse(&format!("c-{ULID}")).unwrap();
    assert!(matches!(comment, ItemId::Comment(_)));
    for text in [format!("r-{ULID}"), format!("e-{ULID}"), "x".to_string()] {
        assert!(ItemId::parse(&text).is_err(), "{text}");
    }
}

#[test]
fn ids_serialise_as_strings_and_are_checked_when_read() {
    let id = ReviewId::parse(&format!("r-{ULID}")).unwrap();
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(json, format!("\"r-{ULID}\""));
    assert_eq!(serde_json::from_str::<ReviewId>(&json).unwrap(), id);
    assert!(serde_json::from_str::<ReviewId>("\"../x\"").is_err());
    assert!(serde_json::from_str::<ReviewId>(&format!("\"t-{ULID}\"")).is_err());

    let item: ItemId = serde_json::from_str(&format!("\"c-{ULID}\"")).unwrap();
    assert_eq!(
        serde_json::to_string(&item).unwrap(),
        format!("\"c-{ULID}\"")
    );
}

/// PLAN §9.6: a GitHub thread's key is `gh:<node id>`, never an id that names a log's item.
#[test]
fn a_thread_key_is_a_thread_id_or_githubs_node_id() {
    let id = ThreadId::generate();
    let local = ThreadKey::parse(id.as_str()).unwrap();
    assert_eq!(local, ThreadKey::Local(id.clone()));
    assert_eq!((local.local(), local.github()), (Some(&id), None));
    assert_eq!(local.to_string(), id.as_str());
    assert_eq!(ThreadKey::from(&id), local);

    let github: ThreadKey = "gh:PRRT_kwDOAbc-12_=".parse().unwrap();
    assert_eq!(github, ThreadKey::Github("PRRT_kwDOAbc-12_=".to_string()));
    assert!(github.is_github() && github.local().is_none());
    assert_eq!(github.to_string(), "gh:PRRT_kwDOAbc-12_=");

    for bad in ["gh:", "gh: x", "gh:a\tb", "GH:PRRT_1", "PRRT_1", "t-1", ""] {
        assert!(ThreadKey::parse(bad).is_err(), "{bad:?}");
    }
    // A node id is never read as a thread id, whatever it looks like.
    assert!(ThreadId::parse(&format!("gh:{id}")).is_err());
}
