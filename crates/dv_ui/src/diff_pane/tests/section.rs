//! The "Not in the diff" section (DESIGN §4.1, PLAN §8) over a real fixture repo: a thread
//! whose file leaves the review heads the stream in it as a full card, which the thread keys
//! reach (`]`, `R`, `X`), which collapses and opens like a file, and which goes once its file
//! is back; the section isn't a file for `J`/`K` or the current file; an empty interdiff shows
//! it with the empty state as its last row; a reply being written follows its thread into it.

use dv_core::review::{Thread, ThreadId, ThreadState, Verdict};
use gpui::Focusable as _;
use pretty_assertions::assert_eq;

use super::*;
use crate::session::ReviewSession;

fn session(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Entity<ReviewSession> {
    shell.read_with(cx, |shell, _| {
        shell.session().cloned().expect("a review is open")
    })
}

fn thread(shell: &Entity<Shell>, id: &ThreadId, cx: &mut VisualTestContext) -> Thread {
    session(shell, cx).read_with(cx, |session, _| {
        session.review().thread(id).cloned().expect("the thread")
    })
}

fn focused(pane: &Entity<DiffPane>, cx: &mut VisualTestContext) -> Option<ThreadId> {
    pane.read_with(cx, |pane, _| pane.focused_thread().cloned())
}

/// A thread on `b.txt`'s new line 2 (`2`); then the agent reverts `b.txt`, which leaves the
/// review: the thread is drawn in the section.
fn thread_whose_file_leaves(
    repo: &FixtureRepo,
    shell: &Entity<Shell>,
    cx: &mut VisualTestContext,
) -> ThreadId {
    let session = session(shell, cx);
    let adding = session.update(cx, |session, cx| {
        let anchor = session
            .anchor_for("b.txt", AnchorSide::New, 2, 2)
            .expect("the lines are loaded");
        session.add_thread(anchor, "Why a digit?".to_string(), cx)
    });
    settle_drawn(cx, shell, "the thread to land", |_, _| true);
    let id = cx
        .foreground_executor()
        .block_test(adding)
        .expect("the thread is added");
    repo.write("b.txt", "one\ntwo\nthree\n");
    let pane = pane(shell, cx);
    settle_drawn(cx, shell, "b.txt to leave", move |_, cx| {
        let pane = pane.read(cx);
        pane.blocks.len() == 3 && pane.is_section(0)
    });
    id
}

#[gpui::test]
fn a_thread_whose_file_left_heads_the_stream_in_its_section(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let id = thread_whose_file_leaves(&repo, &shell, cx);

    pane.read_with(cx, |pane, cx| {
        assert_eq!(
            pane.section_summary(cx),
            Some(SectionSummary {
                collapsed: false,
                threads: vec![id.clone()],
                github: Vec::new(),
                empty: None,
            })
        );
        assert_eq!(
            pane.blocks[0].rows,
            [
                Row::Header {
                    collapsed: false,
                    labels: false
                },
                Row::Heading(InsertItem::Thread(id.clone().into()))
            ]
        );
        let files: Vec<String> = pane
            .file_summaries()
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert_eq!(files, ["a/long.txt", "c.txt"]);
    });
    let session = session(&shell, cx);
    session.read_with(cx, |session, _| {
        let found = session.review().thread(&id).expect("the thread");
        assert!(session.placement(found).is_not_in_diff());
    });

    // At the top, the section isn't a file: none is current, `J` goes to the first file and
    // `K` from there stays (the section is no file to step back to).
    pane.update(cx, |pane, cx| pane.go_to_section(cx));
    settle_drawn(cx, &shell, "the top", |_, _| true);
    assert!(pane.read_with(cx, |pane, _| pane.top_file().is_none()));
    assert_eq!(current(&pane, cx), None);
    press(cx, &shell, "j");
    assert_eq!(current(&pane, cx).as_deref(), Some("a/long.txt"));
    press(cx, &shell, "k");
    assert_eq!(current(&pane, cx).as_deref(), Some("a/long.txt"));

    // `]` from the top (no cursor) reaches its card, `R` replies in it, `X` resolves it.
    pane.update(cx, |pane, cx| {
        pane.focused_thread = None;
        pane.cursor = None;
        pane.go_to_section(cx);
    });
    settle_drawn(cx, &shell, "the top", |_, _| true);
    press(cx, &shell, "]");
    assert_eq!(focused(&pane, cx), Some(id.clone()));
    press(cx, &shell, "r");
    let reply_item = pane.read_with(cx, |pane, _| pane.composer_item());
    assert_eq!(
        reply_item,
        pane.read_with(cx, |pane, _| pane.thread_item(&id))
    );
    assert!(reply_item.is_some(), "the reply composer is in the card");
    cx.simulate_input("Still needed?");
    press(cx, &shell, "secondary-enter");
    let replied = thread(&shell, &id, cx);
    assert_eq!(replied.replies.len(), 1);
    assert_eq!(replied.replies[0].body, "Still needed?");
    press(cx, &shell, "x");
    assert_eq!(thread(&shell, &id, cx).state, ThreadState::Resolved);
    assert!(pane.read_with(cx, |pane, _| {
        !pane.is_thread_open(&id, ThreadState::Resolved)
    }));
    // Resolved, it stays in the section, collapsed like any resolved card.
    assert_eq!(
        pane.read_with(cx, |pane, cx| pane.section_summary(cx))
            .map(|section| section.threads),
        Some(vec![id.clone()])
    );
}

#[gpui::test]
fn the_section_collapses_like_a_file_and_goes_once_its_file_is_back(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let id = thread_whose_file_leaves(&repo, &shell, cx);

    // The chevron collapses it to its header; showing the thread (a comments row) opens it.
    pane.update(cx, |pane, cx| pane.set_open(SECTION, false, cx));
    settle_drawn(cx, &shell, "the section to collapse", |_, _| true);
    pane.read_with(cx, |pane, cx| {
        assert_eq!(
            pane.blocks[0].rows,
            [Row::Header {
                collapsed: true,
                labels: false
            }]
        );
        assert!(pane.section_summary(cx).is_some_and(|s| s.collapsed));
        assert_eq!(pane.thread_item(&id), None);
    });
    pane.update(cx, |pane, cx| pane.reveal_thread(&id, cx));
    settle_drawn(cx, &shell, "the card", |_, _| true);
    pane.read_with(cx, |pane, _| {
        assert_eq!(pane.thread_item(&id), Some(1));
        assert_eq!(pane.focused_thread(), Some(&id));
    });

    // The agent brings b.txt back: its thread is found there again and the section goes.
    repo.write("b.txt", "one\n2\nthree\nfour\n");
    let watched = pane.clone();
    settle_drawn(cx, &shell, "b.txt to come back", move |_, cx| {
        let pane = watched.read(cx);
        !pane.is_section(0) && pane.blocks.len() == 3
    });
    pane.read_with(cx, |pane, cx| {
        assert_eq!(pane.section_summary(cx), None);
        let item = pane.thread_item(&id).expect("its card in b.txt");
        let (block, _) = pane.locate(item).expect("a row");
        assert_eq!(pane.blocks[block].path, "b.txt");
    });
}

#[gpui::test]
fn an_empty_interdiff_shows_the_section_with_the_empty_state_under_it(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let id = thread_whose_file_leaves(&repo, &shell, cx);
    // Round 1 closes asking for changes: nothing changed since, so the interdiff is empty.
    let finishing = session(&shell, cx).update(cx, |session, cx| {
        session.finish_round(Some(Verdict::RequestChanges), None, cx)
    });
    let watched = pane.clone();
    settle_drawn(cx, &shell, "the empty interdiff", move |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).comparison().is_interdiff())
            && watched
                .read(cx)
                .section_summary(cx)
                .is_some_and(|section| section.empty.is_some())
    });
    cx.foreground_executor()
        .block_test(finishing)
        .expect("the round finishes");
    // The stream keeps the section, the empty state its last row, and no empty state shows.
    assert_eq!(shell.read_with(cx, |shell, cx| shell.empty_state(cx)), None);
    pane.read_with(cx, |pane, cx| {
        assert_eq!(
            pane.section_summary(cx),
            Some(SectionSummary {
                collapsed: false,
                threads: vec![id.clone()],
                github: Vec::new(),
                empty: Some("no_changes_since"),
            })
        );
        assert_eq!(pane.blocks.len(), 1);
        assert_eq!(pane.blocks[0].rows.last(), Some(&Row::Empty));
    });
    // Its card is drawn, so a comments row reaches it and `R` replies in it.
    pane.update(cx, |pane, cx| pane.reveal_thread(&id, cx));
    settle_drawn(cx, &shell, "the card", |_, _| true);
    assert_eq!(focused(&pane, cx), Some(id.clone()));
    press(cx, &shell, "r");
    let card = pane.read_with(cx, |pane, _| {
        pane.composer_card()
            .map(|(path, thread)| (path.to_string(), thread.cloned()))
    });
    assert_eq!(card, Some((SECTION.to_string(), Some(id.clone()))));
    // Collapsed, the section keeps the empty state's row.
    press(cx, &shell, "escape");
    pane.update(cx, |pane, cx| pane.set_open(SECTION, false, cx));
    settle_drawn(cx, &shell, "the section to collapse", |_, _| true);
    pane.read_with(cx, |pane, _| {
        assert_eq!(
            pane.blocks[0].rows,
            [
                Row::Header {
                    collapsed: true,
                    labels: false
                },
                Row::Empty
            ]
        );
    });
}

#[gpui::test]
fn a_reply_being_written_follows_its_thread_into_the_section(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let session = session(&shell, cx);
    let adding = session.update(cx, |session, cx| {
        let anchor = session
            .anchor_for("b.txt", AnchorSide::New, 2, 2)
            .expect("the lines are loaded");
        session.add_thread(anchor, "Why a digit?".to_string(), cx)
    });
    settle_drawn(cx, &shell, "the thread to land", |_, _| true);
    let id = cx
        .foreground_executor()
        .block_test(adding)
        .expect("the thread is added");
    pane.update(cx, |pane, cx| pane.focus_thread(&id, cx));
    press(cx, &shell, "r");
    cx.simulate_input("Half a reply");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    // The agent reverts b.txt while the reviewer writes: the card moves into the section and
    // the reply stays in it, its text and the keyboard with it.
    repo.write("b.txt", "one\ntwo\nthree\n");
    let watched = pane.clone();
    settle_drawn(cx, &shell, "b.txt to leave", move |_, cx| {
        watched.read(cx).is_section(0)
    });
    let card = pane.read_with(cx, |pane, _| {
        pane.composer_card()
            .map(|(path, thread)| (path.to_string(), thread.cloned()))
    });
    assert_eq!(card, Some((SECTION.to_string(), Some(id.clone()))));
    assert_eq!(
        pane.read_with(cx, |pane, cx| pane.composer_text(cx))
            .as_deref(),
        Some("Half a reply")
    );
    let focused_field = cx.update(|window, cx| {
        pane.read(cx)
            .composer()
            .is_some_and(|composer| composer.input.focus_handle(cx).is_focused(window))
    });
    assert!(focused_field, "the reply field keeps the keyboard");
    cx.simulate_input(", finished.");
    press(cx, &shell, "secondary-enter");
    let replied = thread(&shell, &id, cx);
    assert_eq!(replied.replies.len(), 1);
    assert_eq!(replied.replies[0].body, "Half a reply, finished.");
}

/// DESIGN §4.1, §4.5: what the section's bar says of the cards it holds.
#[test]
fn the_sections_bar_says_what_its_cards_are() {
    use crate::diff_pane::section::section_words;
    assert_eq!(
        section_words(1, 0, 0),
        "comment on files the review doesn't change now"
    );
    assert_eq!(
        section_words(3, 0, 0),
        "comments on files the review doesn't change now"
    );
    // GitHub's threads without a place in the diff (PLAN §9.6).
    assert_eq!(section_words(0, 2, 0), "outdated on GitHub");
    assert_eq!(section_words(0, 0, 1), "on GitHub, can't be placed here");
    assert_eq!(
        section_words(1, 2, 1),
        "1 comment on files the review doesn't change now · 2 outdated on GitHub · 1 on GitHub, \
         can't be placed here"
    );
}
