//! Rounds in the stream (PLAN §2.3, §8; DESIGN §4.1, §5): `I` and the panel's toggle between
//! the interdiff and all changes, keeping the reading position; the empty interdiff's "Show all
//! changes"; `C` on an interdiff's old column; a card following its moved lines; and a comments
//! row the interdiff doesn't show switching to all changes.

use dv_core::review::{ThreadId, ThreadKey, Verdict};
use gpui::Modifiers;
use pretty_assertions::assert_eq;

use super::*;
use crate::session::{Comparison, ReviewSession};
use crate::shell::EmptyState;
use crate::test_support::bounds;

fn session(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Entity<ReviewSession> {
    shell.read_with(cx, |shell, _| {
        shell.session().cloned().expect("a review is open")
    })
}

fn comparison(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Comparison {
    session(shell, cx).read_with(cx, |session, _| session.comparison())
}

/// `a/long.txt`'s 80 lines with lines 10, 40 and 70 edited (40 being the agent's edit after
/// round 1).
fn long_after_round(repo: &FixtureRepo) {
    let long: String = (1..=80)
        .map(|i| match i {
            10 => "line ten\n".to_string(),
            40 => "line forty\n".to_string(),
            70 => "line seventy\n".to_string(),
            i => format!("line {i}\n"),
        })
        .collect();
    repo.write("a/long.txt", &long);
}

/// Finishes round 1 asking for changes (the sheet's Submit), then has the agent change line 40
/// of `a/long.txt`: the stream shows the interdiff since round 1, `a/long.txt` alone, loaded.
fn round_one_then_an_edit(repo: &FixtureRepo, shell: &Entity<Shell>, cx: &mut VisualTestContext) {
    let session = session(shell, cx);
    let finishing = session.update(cx, |session, cx| {
        session.finish_round(Some(Verdict::RequestChanges), None, cx)
    });
    settle_drawn(cx, shell, "the interdiff", |shell, cx| {
        shell.empty_state(cx) == Some(EmptyState::NoChangesSince(1))
    });
    cx.foreground_executor()
        .block_test(finishing)
        .expect("the round finishes");
    long_after_round(repo);
    let pane = pane(shell, cx);
    settle_drawn(cx, shell, "the agent's edit", move |_, cx| {
        let pane = pane.read(cx);
        pane.blocks.len() == 1 && pane.lines("a/long.txt").is_some()
    });
}

/// The index of new line `number` in `path`'s loaded lines.
fn new_index(
    pane: &Entity<DiffPane>,
    path: &str,
    number: u32,
    cx: &mut VisualTestContext,
) -> usize {
    pane.read_with(cx, |pane, _| {
        pane.lines(path)
            .and_then(|lines| lines.iter().position(|line| line.new_no == Some(number)))
            .expect("the line is loaded")
    })
}

fn put_cursor(
    pane: &Entity<DiffPane>,
    path: &str,
    side: AnchorSide,
    number: u32,
    cx: &mut VisualTestContext,
) {
    let line = pane.read_with(cx, |pane, _| {
        pane.lines(path)
            .and_then(|lines| {
                lines.iter().position(|line| match side {
                    AnchorSide::Old => line.old_no == Some(number),
                    AnchorSide::New => line.new_no == Some(number),
                })
            })
            .expect("the line is loaded")
    });
    pane.update(cx, |pane, cx| {
        pane.click_line(path, LinePos { line, side }, false, cx)
    });
}

/// Whether the cursor's row is on screen.
fn cursor_on_screen(pane: &Entity<DiffPane>, cx: &mut VisualTestContext) -> bool {
    pane.read_with(cx, |pane, _| {
        let height = pane.viewport_height();
        pane.cursor_item()
            .and_then(|item| pane.offset_of(item, height))
            .is_some_and(|y| (0. ..height).contains(&y))
    })
}

#[gpui::test]
fn i_and_the_panel_toggle_switch_the_comparison_keeping_the_reading_position(
    cx: &mut TestAppContext,
) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    round_one_then_an_edit(&repo, &shell, cx);
    assert_eq!(comparison(&shell, cx), Comparison::SinceRound(1));
    // The interdiff's old column is the round's snapshot: no comment starts there.
    assert!(plus("a/long.txt", "new", 40, cx).is_some());
    assert_eq!(plus("a/long.txt", "old", 40, cx), None);
    let index = new_index(&pane, "a/long.txt", 40, cx);
    pane.update(cx, |pane, cx| {
        pane.click_line(
            "a/long.txt",
            LinePos {
                line: index,
                side: AnchorSide::New,
            },
            false,
            cx,
        )
    });
    // `I`: all changes, every file, the cursor on the same line.
    press(cx, &shell, "i");
    let watched = pane.clone();
    settle_drawn(cx, &shell, "all changes", move |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).comparison() == Comparison::All)
            && watched.read(cx).blocks.len() == 3
    });
    assert_eq!(cursor(&pane, cx), new_line("a/long.txt", 40));
    assert!(cursor_on_screen(&pane, cx));
    let status = shell.read_with(cx, |shell, cx| shell.status_line(cx).review);
    assert_eq!(
        status.as_deref(),
        Some("Changes requested · round 2 · no comments")
    );
    // The panel's "Since round 1" goes back, the cursor still there.
    let since = bounds(cx, "comparison-since").expect("the panel's toggle");
    cx.simulate_click(since.center(), Modifiers::none());
    let watched = pane.clone();
    settle_drawn(cx, &shell, "the interdiff", move |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).comparison() == Comparison::SinceRound(1))
            && watched.read(cx).blocks.len() == 1
    });
    assert_eq!(cursor(&pane, cx), new_line("a/long.txt", 40));
    assert!(cursor_on_screen(&pane, cx));
    let status = shell.read_with(cx, |shell, cx| shell.status_line(cx).review);
    assert_eq!(
        status.as_deref(),
        Some("Changes requested · round 2 · no comments · since round 1")
    );
}

#[gpui::test]
fn an_empty_interdiff_offers_all_changes(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let session = session(&shell, cx);
    let finishing = session.update(cx, |session, cx| {
        session.finish_round(Some(Verdict::RequestChanges), None, cx)
    });
    settle_drawn(cx, &shell, "the empty interdiff", |shell, cx| {
        shell.empty_state(cx) == Some(EmptyState::NoChangesSince(1))
    });
    cx.foreground_executor()
        .block_test(finishing)
        .expect("the round finishes");
    let button = bounds(cx, "show-all-changes").expect("Show all changes");
    cx.simulate_click(button.center(), Modifiers::none());
    settle_drawn(cx, &shell, "all changes", |shell, cx| {
        shell.empty_state(cx).is_none()
            && shell
                .session()
                .is_some_and(|session| session.read(cx).files().len() == 3)
    });
    assert_eq!(comparison(&shell, cx), Comparison::All);
}

#[gpui::test]
fn c_on_an_interdiffs_old_column_does_nothing(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    round_one_then_an_edit(&repo, &shell, cx);
    // The old column is round 1's snapshot.
    put_cursor(&pane, "a/long.txt", AnchorSide::Old, 40, cx);
    press(cx, &shell, "c");
    assert!(pane.read_with(cx, |pane, _| pane.composer().is_none()));
    // The new column takes a comment.
    press(cx, &shell, "right c");
    let target = pane.read_with(cx, |pane, _| {
        pane.composer()
            .map(|composer| (composer.target.side, composer.target.start_line))
    });
    assert_eq!(target, Some((AnchorSide::New, 40)));
}

#[gpui::test]
fn a_card_follows_its_lines_when_they_move(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let session = session(&shell, cx);
    let anchor = session
        .read_with(cx, |session, _| {
            session.anchor_for("b.txt", AnchorSide::New, 4, 4)
        })
        .expect("b.txt is loaded");
    let adding = session.update(cx, |session, cx| {
        session.add_thread(anchor, "Four?".to_string(), cx)
    });
    settle_drawn(cx, &shell, "the thread", |_, _| true);
    let id: ThreadId = cx
        .foreground_executor()
        .block_test(adding)
        .expect("the thread is added");
    // The agent adds two lines at the top of b.txt: `four` is line 6 now.
    repo.write("b.txt", "zero\nhalf\none\n2\nthree\nfour\n");
    let watched = session.clone();
    let wanted = id.clone();
    settle_drawn(cx, &shell, "the move", move |_, cx| {
        let session = watched.read(cx);
        let thread = session.review().thread(&wanted).expect("the thread");
        session.placement(thread).lines(&thread.anchor) == Some((6, 6))
    });
    // Its card sits under new line 6, and says it moved.
    let at = new_index(&pane, "b.txt", 6, cx);
    pane.read_with(cx, |pane, _| {
        let rows = &pane.blocks[1].rows;
        let card = rows
            .iter()
            .position(|row| row.thread().and_then(ThreadKey::local) == Some(&id))
            .expect("the card has a row");
        assert_eq!(
            rows[card],
            Row::Insert(rows::Insert {
                line: at,
                side: AnchorSide::New,
                item: rows::InsertItem::Thread(id.clone().into()),
            })
        );
    });
    session.read_with(cx, |session, _| {
        let thread = session.review().thread(&id).expect("the thread");
        let placement = session.placement(thread);
        assert!(placement.shows_moved(thread));
        assert_eq!(
            crate::thread::shown_anchor_label(
                &thread.anchor,
                placement.lines(&thread.anchor),
                placement.shows_moved(thread),
            ),
            "new · line 6 · moved"
        );
    });
}

#[gpui::test]
fn a_comments_row_the_interdiff_doesnt_show_switches_to_all_changes(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let session = session(&shell, cx);
    let anchor = session
        .read_with(cx, |session, _| {
            session.anchor_for("b.txt", AnchorSide::New, 2, 2)
        })
        .expect("b.txt is loaded");
    let adding = session.update(cx, |session, cx| {
        session.add_thread(anchor, "Why 2?".to_string(), cx)
    });
    settle_drawn(cx, &shell, "the thread", |_, _| true);
    let id: ThreadId = cx
        .foreground_executor()
        .block_test(adding)
        .expect("the thread is added");
    // Round 1, then the agent changes a/long.txt only: b.txt isn't in the interdiff.
    round_one_then_an_edit(&repo, &shell, cx);
    let list = shell.read_with(cx, |shell, _| shell.comments_list().clone());
    let row = list.read_with(cx, |list, cx| list.rows(cx));
    assert_eq!(row.len(), 1);
    assert!(row[0].not_in_comparison);
    assert!(pane.read_with(cx, |pane, _| pane.thread_item(&id).is_none()));
    // `]` passes over it.
    pane.update(cx, |pane, cx| {
        pane.focused_thread = None;
        cx.notify();
    });
    press(cx, &shell, "]");
    assert_eq!(
        pane.read_with(cx, |pane, _| pane.focused_thread().cloned()),
        None
    );
    // A click on its row shows all changes and focuses it there.
    list.update(cx, |list, cx| list.reveal(&id, cx));
    let watched = pane.clone();
    let wanted = id.clone();
    settle_drawn(cx, &shell, "the thread in all changes", move |_, cx| {
        watched.read(cx).thread_item(&wanted).is_some()
    });
    assert_eq!(comparison(&shell, cx), Comparison::All);
    assert_eq!(
        pane.read_with(cx, |pane, _| pane.focused_thread().cloned()),
        Some(id)
    );
    let row = list.read_with(cx, |list, cx| list.rows(cx));
    assert!(!row[0].not_in_comparison);
}

/// The composer's file, side, first line and text.
fn draft(
    pane: &Entity<DiffPane>,
    cx: &mut VisualTestContext,
) -> Option<(String, AnchorSide, u32, String)> {
    pane.read_with(cx, |pane, cx| {
        let composer = pane.composer()?;
        Some((
            composer.target.path.clone(),
            composer.target.side,
            composer.target.start_line,
            pane.composer_text(cx)?,
        ))
    })
}

/// Switches the comparison as the panel's toggle does, and waits for its list.
fn switch(shell: &Entity<Shell>, to: Comparison, cx: &mut VisualTestContext) {
    session(shell, cx).update(cx, |session, cx| session.set_comparison(to, cx));
    settle_drawn(cx, shell, "the comparison", move |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).comparison() == to)
    });
}

#[gpui::test]
fn a_draft_waits_out_a_comparison_that_doesnt_list_its_file(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    round_one_then_an_edit(&repo, &shell, cx);
    // In all changes, a draft on b.txt, which the interdiff doesn't list.
    press(cx, &shell, "i");
    let watched = pane.clone();
    settle_drawn(cx, &shell, "all changes", move |_, cx| {
        watched.read(cx).lines("b.txt").is_some()
    });
    put_cursor(&pane, "b.txt", AnchorSide::New, 4, cx);
    press(cx, &shell, "c");
    cx.simulate_input("a long draft about four");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    let typed = Some((
        "b.txt".to_string(),
        AnchorSide::New,
        4,
        "a long draft about four".to_string(),
    ));
    assert_eq!(draft(&pane, cx), typed);
    // The panel's toggle to the interdiff keeps it (not drawn: b.txt isn't listed there)...
    switch(&shell, Comparison::SinceRound(1), cx);
    assert_eq!(draft(&pane, cx), typed);
    assert!(pane.read_with(cx, |pane, _| pane.composer_item().is_none()));
    // ...and back in all changes it is where it was.
    switch(&shell, Comparison::All, cx);
    assert_eq!(draft(&pane, cx), typed);
    assert!(pane.read_with(cx, |pane, _| pane.composer_item().is_some()));

    // Hidden by the interdiff, `C` brings it back: all changes, the draft taking the keyboard.
    switch(&shell, Comparison::SinceRound(1), cx);
    shell.update_in(cx, |shell, window, cx| {
        shell.focus_handle(cx).focus(window, cx)
    });
    press(cx, &shell, "c");
    let watched = pane.clone();
    settle_drawn(cx, &shell, "the draft in all changes", move |_, cx| {
        watched.read(cx).composer_item().is_some()
    });
    assert_eq!(comparison(&shell, cx), Comparison::All);
    assert_eq!(draft(&pane, cx), typed);
    let focused = pane.update_in(cx, |pane, window, cx| {
        pane.composer()
            .is_some_and(|composer| composer.input.focus_handle(cx).is_focused(window))
    });
    assert!(focused, "the draft has the keyboard");

    // A round closing moves to the interdiff; the draft survives that too.
    let finishing = session(&shell, cx).update(cx, |session, cx| {
        session.finish_round(Some(Verdict::RequestChanges), None, cx)
    });
    settle_drawn(cx, &shell, "the next interdiff", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).comparison() == Comparison::SinceRound(2))
    });
    cx.foreground_executor()
        .block_test(finishing)
        .expect("the round finishes");
    assert_eq!(draft(&pane, cx), typed);
}

/// The old line `path`'s composer row is drawn under, if it is drawn.
fn composer_old_line(
    pane: &Entity<DiffPane>,
    path: &str,
    cx: &mut VisualTestContext,
) -> Option<u32> {
    pane.read_with(cx, |pane, _| {
        let block = pane.blocks.iter().find(|block| block.path == path)?;
        let line = block.rows.iter().find_map(|row| match row {
            rows::Row::Insert(rows::Insert {
                line,
                item: rows::InsertItem::Composer,
                ..
            }) => Some(*line),
            _ => None,
        })?;
        pane.lines(path)?.get(line)?.old_no
    })
}

#[gpui::test]
fn an_old_side_draft_waits_for_its_files_own_diff_after_the_switch(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    round_one_then_an_edit(&repo, &shell, cx);
    repo.write("b.txt", "one\n2\nthree\nfour\nfive\n");
    let watched = pane.clone();
    settle_drawn(cx, &shell, "b.txt in the interdiff", move |_, cx| {
        watched.read(cx).blocks.len() == 2
    });
    press(cx, &shell, "i");
    let watched = pane.clone();
    settle_drawn(cx, &shell, "all changes", move |_, cx| {
        watched.read(cx).lines("b.txt").is_some() && watched.read(cx).blocks.len() == 3
    });
    // A draft on old line 2 ("two", which round 1's snapshot no longer has).
    put_cursor(&pane, "b.txt", AnchorSide::Old, 2, cx);
    press(cx, &shell, "c");
    cx.simulate_input("why drop two");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    assert_eq!(composer_old_line(&pane, "b.txt", cx), Some(2));
    switch(&shell, Comparison::SinceRound(1), cx);
    assert_eq!(composer_old_line(&pane, "b.txt", cx), None);

    // Back to all changes, the files' loads held: the listing lands, and b.txt still shows
    // the interdiff's diff, whose old column is round 1's snapshot.
    let session = session(&shell, cx);
    session.update(cx, |session, cx| {
        session.hold_loads(true, cx);
        session.set_comparison(Comparison::All, cx);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while session.read_with(cx, |session, _| session.comparison()) != Comparison::All {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(400));
        cx.run_until_parked();
        assert!(
            std::time::Instant::now() < deadline,
            "the listing never landed"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    draw(cx);
    assert_eq!(
        session.read_with(cx, |session, _| session.shown_comparison("b.txt")),
        Some(Comparison::SinceRound(1)),
        "the interdiff's diff still shows"
    );
    assert!(pane.read_with(cx, |pane, _| pane.lines("b.txt").is_some()));
    assert_eq!(
        composer_old_line(&pane, "b.txt", cx),
        None,
        "not drawn against round 1's snapshot"
    );

    // b.txt's own diff lands: the draft is drawn on the line it was written about.
    session.update(cx, |session, cx| session.hold_loads(false, cx));
    let watched = pane.clone();
    settle_drawn(
        cx,
        &shell,
        "b.txt's diff of all changes",
        move |shell, cx| {
            shell.session().is_some_and(|session| {
                session.read(cx).shown_comparison("b.txt") == Some(Comparison::All)
            }) && watched.read(cx).composer_item().is_some()
        },
    );
    assert_eq!(composer_old_line(&pane, "b.txt", cx), Some(2));
    assert_eq!(
        draft(&pane, cx),
        Some((
            "b.txt".to_string(),
            AnchorSide::Old,
            2,
            "why drop two".to_string()
        ))
    );
}

#[gpui::test]
fn an_old_side_draft_keeps_its_line_across_the_comparisons(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    round_one_then_an_edit(&repo, &shell, cx);
    // b.txt changes after the round too, so both comparisons list it.
    repo.write("b.txt", "one\n2\nthree\nfour\nfive\n");
    let watched = pane.clone();
    settle_drawn(cx, &shell, "b.txt in the interdiff", move |_, cx| {
        watched.read(cx).blocks.len() == 2
    });
    press(cx, &shell, "i");
    let watched = pane.clone();
    settle_drawn(cx, &shell, "all changes", move |_, cx| {
        watched.read(cx).lines("b.txt").is_some() && watched.read(cx).blocks.len() == 3
    });
    // Old line 2 ("two") was removed in round 1: the round's snapshot has no such line.
    put_cursor(&pane, "b.txt", AnchorSide::Old, 2, cx);
    press(cx, &shell, "c");
    cx.simulate_input("why drop two");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    let typed = Some((
        "b.txt".to_string(),
        AnchorSide::Old,
        2,
        "why drop two".to_string(),
    ));
    assert_eq!(draft(&pane, cx), typed);
    switch(&shell, Comparison::SinceRound(1), cx);
    assert_eq!(
        draft(&pane, cx),
        typed,
        "untouched by the snapshot's old side"
    );
    assert!(
        pane.read_with(cx, |pane, _| pane.composer_item().is_none()),
        "not drawn against the snapshot"
    );
    switch(&shell, Comparison::All, cx);
    assert_eq!(draft(&pane, cx), typed);
    // Posted, it is on the line it was written about.
    pane.update_in(cx, |pane, window, cx| pane.post_composer(window, cx));
    settle_drawn(cx, &shell, "the post", |_, _| true);
    let posted = session(&shell, cx).read_with(cx, |session, _| {
        session
            .review()
            .live_threads()
            .map(|thread| {
                (
                    thread.anchor.side,
                    thread.anchor.start_line,
                    thread.anchor.text.clone(),
                )
            })
            .collect::<Vec<_>>()
    });
    assert_eq!(posted, [(AnchorSide::Old, 2, vec!["two".to_string()])]);
}

#[gpui::test]
fn an_outdated_card_shows_where_its_code_went_since_its_round(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let session = session(&shell, cx);
    let anchor = session
        .read_with(cx, |session, _| {
            session.anchor_for("a/long.txt", AnchorSide::New, 10, 10)
        })
        .expect("a/long.txt is loaded");
    let adding = session.update(cx, |session, cx| {
        session.add_thread(anchor, "Ten in words?".to_string(), cx)
    });
    settle_drawn(cx, &shell, "the thread", |_, _| true);
    let id: ThreadId = cx
        .foreground_executor()
        .block_test(adding)
        .expect("the thread is added");
    let finishing = session.update(cx, |session, cx| {
        session.finish_round(Some(Verdict::RequestChanges), None, cx)
    });
    settle_drawn(cx, &shell, "the interdiff", |shell, cx| {
        shell.empty_state(cx) == Some(EmptyState::NoChangesSince(1))
    });
    cx.foreground_executor()
        .block_test(finishing)
        .expect("the round finishes");
    // The agent replaces line 10 with two lines that share nothing with it.
    let long: String = (1..=80)
        .map(|i| match i {
            10 => "struct Q;\nimpl Q {}\n".to_string(),
            70 => "line seventy\n".to_string(),
            i => format!("line {i}\n"),
        })
        .collect();
    repo.write("a/long.txt", &long);
    let watched = session.clone();
    let wanted = id.clone();
    settle_drawn(cx, &shell, "its trail", move |_, cx| {
        let session = watched.read(cx);
        let thread = session.review().thread(&wanted).expect("the thread");
        session.placement(thread).trail().is_some()
    });
    // Outdated, its card sits under new line 11, the last line that replaced it, rather than
    // heading the file.
    let at = new_index(&pane, "a/long.txt", 11, cx);
    pane.read_with(cx, |pane, _| {
        let rows = &pane.blocks[0].rows;
        assert!(!rows.contains(&Row::Heading(InsertItem::Thread(id.clone().into()))));
        assert!(rows.contains(&Row::Insert(rows::Insert {
            line: at,
            side: AnchorSide::New,
            item: rows::InsertItem::Thread(id.clone().into()),
        })));
    });
    session.read_with(cx, |session, _| {
        let thread = session.review().thread(&id).expect("the thread");
        let placement = session.placement(thread);
        assert!(placement.is_outdated());
        assert_eq!(
            placement.trail().map(|trail| trail.round),
            Some(1),
            "code changed since round 1"
        );
        assert_eq!(session.location(thread), "a/long.txt:10–11");
    });
}

#[gpui::test]
fn a_reply_being_typed_keeps_its_card_while_its_history_is_read(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let session = session(&shell, cx);
    let anchor = session
        .read_with(cx, |session, _| {
            session.anchor_for("a/long.txt", AnchorSide::New, 10, 10)
        })
        .expect("a/long.txt is loaded");
    let adding = session.update(cx, |session, cx| {
        session.add_thread(anchor, "Ten in words?".to_string(), cx)
    });
    settle_drawn(cx, &shell, "the thread", |_, _| true);
    let id: ThreadId = cx
        .foreground_executor()
        .block_test(adding)
        .expect("the thread is added");
    let finishing = session.update(cx, |session, cx| {
        session.finish_round(Some(Verdict::RequestChanges), None, cx)
    });
    settle_drawn(cx, &shell, "the interdiff", |shell, cx| {
        shell.empty_state(cx) == Some(EmptyState::NoChangesSince(1))
    });
    cx.foreground_executor()
        .block_test(finishing)
        .expect("the round finishes");
    switch(&shell, Comparison::All, cx);
    // History reads wait, so what shows meanwhile can be looked at.
    session.update(cx, |session, cx| session.hold_histories(true, cx));
    pane.update(cx, |pane, cx| pane.focus_thread(&id, cx));
    press(cx, &shell, "r");
    cx.simulate_input("Half a reply");
    settle_drawn(cx, &shell, "typing", |_, _| true);

    // The agent rewrites the commented line while the reviewer writes.
    let long: String = (1..=80)
        .map(|i| match i {
            10 => "struct Q;\nimpl Q {}\n".to_string(),
            70 => "line seventy\n".to_string(),
            i => format!("line {i}\n"),
        })
        .collect();
    repo.write("a/long.txt", &long);
    let watched = pane.clone();
    settle_drawn(cx, &shell, "the agent's edit", move |_, cx| {
        watched
            .read(cx)
            .lines("a/long.txt")
            .is_some_and(|lines| lines.iter().any(|line| line.text == "impl Q {}"))
    });
    let card = |pane: &Entity<DiffPane>, cx: &mut VisualTestContext| {
        pane.read_with(cx, |pane, _| {
            pane.composer_card()
                .map(|(path, thread)| (path.to_string(), thread.cloned()))
        })
    };
    let focused_field = |pane: &Entity<DiffPane>, cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            pane.read(cx)
                .composer()
                .is_some_and(|composer| composer.input.focus_handle(cx).is_focused(window))
        })
    };
    // Its history missing, the thread keeps the placement it had: the card, the reply and
    // the keyboard stay.
    session.read_with(cx, |session, _| {
        let thread = session.review().thread(&id).expect("the thread");
        assert!(!session.placement(thread).is_pending());
    });
    assert_eq!(
        card(&pane, cx),
        Some(("a/long.txt".to_string(), Some(id.clone())))
    );
    assert!(
        focused_field(&pane, cx),
        "the reply field keeps the keyboard"
    );

    // The history lands: the card goes where the code went, the reply with it.
    session.update(cx, |session, cx| session.hold_histories(false, cx));
    let watched = session.clone();
    let wanted = id.clone();
    settle_drawn(cx, &shell, "its trail", move |_, cx| {
        let session = watched.read(cx);
        let thread = session.review().thread(&wanted).expect("the thread");
        session.placement(thread).trail().is_some()
    });
    let at = new_index(&pane, "a/long.txt", 11, cx);
    pane.read_with(cx, |pane, _| {
        let block = pane
            .blocks
            .iter()
            .find(|block| block.path == "a/long.txt")
            .expect("its block");
        assert!(block.rows.contains(&Row::Insert(rows::Insert {
            line: at,
            side: AnchorSide::New,
            item: rows::InsertItem::Thread(id.clone().into()),
        })));
    });
    assert_eq!(
        card(&pane, cx),
        Some(("a/long.txt".to_string(), Some(id.clone())))
    );
    assert!(
        focused_field(&pane, cx),
        "the reply field keeps the keyboard"
    );
    assert_eq!(
        pane.read_with(cx, |pane, cx| pane.composer_text(cx))
            .as_deref(),
        Some("Half a reply")
    );
}
