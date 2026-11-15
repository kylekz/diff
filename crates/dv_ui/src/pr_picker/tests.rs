//! The PR picker's rows and states ([`build`]) as plain data; the picker in the shell, with
//! its list read through `mock_gh`, is `crate::shell::tests::pr::read`.

use std::sync::Arc;

use dv_core::Location;
use dv_core::github::{ListedPr, PrRef};
use pretty_assertions::assert_eq;

use super::reference::{CloneOffer, PrOffer};
use super::*;
use crate::services::github::{ListFailure, ListFailureKind, PrList};

fn here() -> Location {
    Location::Local("/code/api".into())
}

fn pr(number: u64, title: &str, author: &str, requested: bool) -> ListedPr {
    ListedPr {
        number,
        title: title.to_string(),
        author: author.to_string(),
        draft: false,
        updated_at: None,
        review_requested: requested,
    }
}

fn list() -> PrList {
    PrList {
        prs: Some(Arc::new(vec![
            pr(7, "Add the answer", "octocat", true),
            pr(12, "Cache the config", "hubot", false),
            pr(71, "Stream the body", "octocat", false),
        ])),
        read_at: Some(1),
        ..PrList::default()
    }
}

fn keys(rows: &[Row]) -> Vec<&str> {
    rows.iter().map(|row| row.key.as_str()).collect()
}

fn clone_at(location: Location, key: &str) -> CloneOffer {
    CloneOffer {
        location,
        key: key.to_string(),
        name: key.to_string(),
        title: None,
    }
}

#[test]
fn the_list_filters_by_number_title_and_author() {
    let (here, list) = (here(), list());
    let rows = |query: &str| build(query, Some(&here), Some(&list), None);

    let (all, state) = rows("");
    assert_eq!(keys(&all), ["pr/7", "pr/12", "pr/71"]);
    assert_eq!(state, ListState::Listed);
    assert_eq!(keys(&rows("  CACHE hubot ").0), ["pr/12"]);
    assert_eq!(keys(&rows("octocat").0), ["pr/7", "pr/71"]);
    assert_eq!(
        keys(&rows("#7").0),
        ["pr/7", "pr/71"],
        "no reference offered"
    );
    let (none, state) = rows("nothing");
    assert!(none.is_empty());
    assert_eq!(state, ListState::NoMatch);
}

#[test]
fn a_reference_is_the_first_row_and_a_number_still_filters() {
    let (here, list) = (here(), list());
    let reference = PrRef::Number(7);
    let offered = PrOffer::Clones {
        reference: reference.clone(),
        name: "acme/api#7".to_string(),
        clones: vec![CloneOffer {
            title: Some("Add the answer".to_string()),
            ..clone_at(here.clone(), "api")
        }],
    };
    let (rows, state) = build("7", Some(&here), Some(&list), Some(&offered));
    // The pull request named, then the others whose number holds the digits.
    assert_eq!(keys(&rows), ["open/api", "pr/71"]);
    assert_eq!(state, ListState::Listed);
    assert_eq!(
        rows[0].kind,
        RowKind::Reference {
            name: "acme/api#7".to_string(),
            title: Some("Add the answer".to_string()),
            // The repository in view needs no name.
            clone: None,
            location: here.clone(),
            reference,
        }
    );

    // Another repository's: its known clones, each named, and none of this list.
    let elsewhere = Location::Local("/code/web".into());
    let reference = PrRef::parse("acme/web#3").unwrap();
    let offered = PrOffer::Clones {
        reference: reference.clone(),
        name: "acme/web#3".to_string(),
        clones: vec![clone_at(elsewhere.clone(), "web")],
    };
    let (rows, state) = build("acme/web#3", Some(&here), Some(&list), Some(&offered));
    assert_eq!(keys(&rows), ["open/web"]);
    assert_eq!(state, ListState::Listed);
    assert!(matches!(
        &rows[0].kind,
        RowKind::Reference { clone: Some(clone), location, .. }
            if clone == "web" && *location == elsewhere
    ));

    // One dv knows no clone of: its folder is asked for.
    let offered = PrOffer::Unknown {
        reference: reference.clone(),
        repository: "acme/web".to_string(),
    };
    let (rows, _) = build("acme/web#3", Some(&here), Some(&list), Some(&offered));
    assert_eq!(
        rows,
        [Row {
            key: "choose_folder".to_string(),
            kind: RowKind::ChooseFolder {
                repository: "acme/web".to_string(),
                reference,
            },
        }]
    );
}

#[test]
fn what_stands_in_the_lists_place_says_why_there_is_none() {
    let here = here();
    // No repository in view: only what the field names.
    let (rows, state) = build("", None, None, None);
    assert!(rows.is_empty());
    assert_eq!(state, ListState::NoRepository);
    assert_eq!(state.key(), "no_repository");

    // The first read runs.
    assert_eq!(build("", Some(&here), None, None).1, ListState::Loading);
    let loading = PrList {
        loading: true,
        ..PrList::default()
    };
    assert_eq!(
        build("", Some(&here), Some(&loading), None).1,
        ListState::Loading
    );

    // It failed with nothing read before: why. Read again meanwhile, it loads.
    let failure = ListFailure {
        kind: ListFailureKind::SignedOut,
        failure: Failure::new("gh isn't signed in to github.com: run `gh auth login`"),
    };
    let failed = PrList {
        failure: Some(failure.clone()),
        ..PrList::default()
    };
    assert_eq!(
        build("", Some(&here), Some(&failed), None).1,
        ListState::Failed(failure.clone())
    );
    let again = PrList {
        loading: true,
        ..failed.clone()
    };
    assert_eq!(
        build("", Some(&here), Some(&again), None).1,
        ListState::Loading
    );

    // It failed after a list was read: that list still shows.
    let stale = PrList {
        failure: Some(failure),
        ..list()
    };
    let (rows, state) = build("", Some(&here), Some(&stale), None);
    assert_eq!(rows.len(), 3);
    assert_eq!(state, ListState::Listed);

    // No open pull requests, whatever is typed.
    let empty = PrList {
        prs: Some(Arc::new(Vec::new())),
        ..PrList::default()
    };
    assert_eq!(
        build("x", Some(&here), Some(&empty), None).1,
        ListState::Empty
    );
}
