//! Threads in the stream over real fixture repos: the composer (`C`, typing, `Ctrl Enter`,
//! `Esc`), the cards' places (inline, outdated), `]`/`[`, `R` and `X`, a finished review
//! taking no comment, the composer keeping or handing back the keyboard as its row scrolls
//! away, collapses or goes, the last file's header reaching the top past a collapsed card, and
//! a line's `+` starting a comment with the mouse (a click, a drag over lines).

use dv_core::review::{Thread, ThreadId, ThreadState, Verdict};
use gpui::{Modifiers, MouseButton};

use pretty_assertions::assert_eq;

use super::*;
use crate::session::ReviewSession;
use crate::thread::ComposerTarget;

fn session(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Entity<ReviewSession> {
    shell.read_with(cx, |shell, _| {
        shell.session().cloned().expect("a review is open")
    })
}

fn threads(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Vec<Thread> {
    let session = session(shell, cx);
    session.read_with(cx, |session, _| {
        session.review().live_threads().cloned().collect()
    })
}

/// Adds a thread on lines `from`..=`to` of `path`'s `side` through the session, as the composer
/// would, and waits for its card.
fn add_thread(
    shell: &Entity<Shell>,
    path: &str,
    side: AnchorSide,
    lines: (u32, u32),
    body: &str,
    cx: &mut VisualTestContext,
) -> ThreadId {
    let session = session(shell, cx);
    let adding = session.update(cx, |session, cx| {
        let anchor = session
            .anchor_for(path, side, lines.0, lines.1)
            .expect("the lines are loaded");
        session.add_thread(anchor, body.to_string(), cx)
    });
    settle_drawn(cx, shell, "the thread to land", |_, _| true);
    let id = cx
        .foreground_executor()
        .block_test(adding)
        .expect("the thread is added");
    let pane = pane(shell, cx);
    let wanted = id.clone();
    settle_drawn(cx, shell, "its card", move |_, cx| {
        pane.read(cx).thread_item(&wanted).is_some()
    });
    id
}

fn composer_target(pane: &Entity<DiffPane>, cx: &mut VisualTestContext) -> Option<ComposerTarget> {
    pane.read_with(cx, |pane, _| {
        pane.composer().map(|composer| composer.target.clone())
    })
}

fn focused(pane: &Entity<DiffPane>, cx: &mut VisualTestContext) -> Option<ThreadId> {
    pane.read_with(cx, |pane, _| pane.focused_thread().cloned())
}

#[gpui::test]
fn c_opens_the_composer_on_the_selection_and_ctrl_enter_posts_the_thread(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    // b.txt: the cursor on the new `2`, the selection down to `three`.
    press(cx, &shell, "j shift-down c");
    assert_eq!(
        composer_target(&pane, cx),
        Some(ComposerTarget {
            path: "b.txt".to_string(),
            side: AnchorSide::New,
            start_line: 2,
            end_line: 3,
            reply_to: None,
        })
    );
    // Its row follows the selection's last row, the context line `three` (diff line 3).
    pane.read_with(cx, |pane, _| {
        let rows = &pane.blocks[1].rows;
        let at = rows
            .iter()
            .position(|row| {
                matches!(row, Row::Insert(insert) if insert.item == rows::InsertItem::Composer)
            })
            .expect("the composer has a row");
        assert_eq!(
            rows[at - 1],
            Row::Pair {
                old: Some(3),
                new: Some(3)
            }
        );
    });
    // The field has the keyboard: letters go into it, not to the stream's keys.
    cx.simulate_input("Rename this j");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    assert_eq!(
        pane.read_with(cx, |pane, cx| pane.composer_text(cx))
            .as_deref(),
        Some("Rename this j")
    );
    assert_eq!(current(&pane, cx).as_deref(), Some("b.txt"));

    press(cx, &shell, "secondary-enter");
    let posted = threads(&shell, cx);
    assert_eq!(posted.len(), 1);
    let thread = &posted[0];
    assert_eq!(thread.anchor.path, "b.txt");
    assert_eq!(thread.anchor.side, AnchorSide::New);
    assert_eq!((thread.anchor.start_line, thread.anchor.end_line), (2, 3));
    assert_eq!(thread.anchor.text, ["2", "three"]);
    assert_eq!(thread.anchor.context_before, ["one"]);
    assert_eq!(thread.body, "Rename this j");
    assert_eq!(thread.author.kind, dv_core::review::AuthorKind::Reviewer);
    // It is in the store, not only in the view.
    let session = session(&shell, cx);
    let stored = session.read_with(cx, |session, _| {
        session
            .store()
            .load(session.review_id())
            .expect("the log loads")
    });
    assert_eq!(stored.review.threads.len(), 1);
    // The composer closed, the new thread is focused with its card inline, the selection is
    // gone and the keyboard is back on the stream.
    pane.read_with(cx, |pane, _| {
        assert!(pane.composer().is_none());
        assert_eq!(pane.focused_thread(), Some(&thread.id));
        assert!(pane.thread_item(&thread.id).is_some());
        assert_eq!(pane.selection(), None);
    });
    press(cx, &shell, "j");
    assert_eq!(current(&pane, cx).as_deref(), Some("c.txt"));
}

#[gpui::test]
fn escape_cancels_the_composer_which_otherwise_takes_the_cursors_line(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    press(cx, &shell, "j c");
    assert_eq!(
        composer_target(&pane, cx).map(|target| (target.start_line, target.end_line)),
        Some((2, 2))
    );
    cx.simulate_input("draft");
    press(cx, &shell, "escape");
    assert_eq!(composer_target(&pane, cx), None);
    assert!(threads(&shell, cx).is_empty());
    let focused_main = shell.update_in(cx, |shell, window, cx| {
        shell.focus_handle(cx).is_focused(window)
    });
    assert!(focused_main, "the keyboard returns to the stream");
    // An empty post is refused in the composer itself.
    press(cx, &shell, "c secondary-enter");
    pane.read_with(cx, |pane, _| {
        let composer = pane.composer().expect("the composer stays");
        assert_eq!(
            composer.error.as_ref().map(|error| error.message.as_str()),
            Some("Write something first")
        );
    });
}

#[gpui::test]
fn brackets_focus_threads_r_replies_and_x_resolves(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let first = add_thread(&shell, "a/long.txt", AnchorSide::New, (10, 10), "One", cx);
    let second = add_thread(&shell, "b.txt", AnchorSide::Old, (2, 2), "Two", cx);
    pane.update(cx, |pane, cx| {
        pane.focused_thread = None;
        cx.notify();
    });
    press(cx, &shell, "]");
    assert_eq!(focused(&pane, cx), Some(first.clone()));
    press(cx, &shell, "]");
    assert_eq!(focused(&pane, cx), Some(second.clone()));
    // The last thread stays put; `[` goes back.
    press(cx, &shell, "]");
    assert_eq!(focused(&pane, cx), Some(second.clone()));
    press(cx, &shell, "[");
    assert_eq!(focused(&pane, cx), Some(first.clone()));

    // `R` opens the reply composer inside the focused card.
    press(cx, &shell, "r");
    assert_eq!(
        composer_target(&pane, cx).and_then(|target| target.reply_to),
        Some(first.clone())
    );
    cx.simulate_input("Still wrong");
    press(cx, &shell, "secondary-enter");
    let replied = threads(&shell, cx);
    assert_eq!(replied[0].replies.len(), 1);
    assert_eq!(replied[0].replies[0].body, "Still wrong");
    assert_eq!(composer_target(&pane, cx), None);

    // `X` resolves the focused thread, which collapses; again, it reopens.
    press(cx, &shell, "x");
    assert_eq!(threads(&shell, cx)[0].state, ThreadState::Resolved);
    assert!(pane.read_with(cx, |pane, _| {
        !pane.is_thread_open(&first, ThreadState::Resolved)
    }));
    // A resolved thread is passed over by `]`/`[`.
    press(cx, &shell, "] [");
    assert_eq!(focused(&pane, cx), Some(second.clone()));
    pane.update(cx, |pane, cx| pane.focus_thread(&first, cx));
    press(cx, &shell, "x");
    assert_eq!(threads(&shell, cx)[0].state, ThreadState::Open);
}

#[gpui::test]
fn a_thread_whose_lines_changed_heads_its_file_as_outdated(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let id = add_thread(&shell, "a/long.txt", AnchorSide::New, (10, 10), "Ten?", cx);
    pane.read_with(cx, |pane, _| {
        assert!(matches!(&pane.blocks[0].rows[1], Row::Gap(_)));
    });
    // The agent rewrites the anchored line beyond recognition: the watch refreshes the file,
    // and relocation finds it nowhere (PLAN §8).
    let long: String = (1..=80)
        .map(|i| match i {
            10 => "fn zebra() {}\n".to_string(),
            70 => "line seventy\n".to_string(),
            i => format!("line {i}\n"),
        })
        .collect();
    repo.write("a/long.txt", &long);
    let watched = pane.clone();
    let wanted = id.clone();
    settle_drawn(cx, &shell, "the thread to be outdated", move |_, cx| {
        watched.read(cx).blocks[0].rows.get(1)
            == Some(&Row::Heading(InsertItem::Thread(wanted.clone())))
    });
    let session = session(&shell, cx);
    let outdated = session.read_with(cx, |session, _| {
        session
            .placement(session.review().thread(&id).expect("the thread"))
            .is_outdated()
    });
    assert!(outdated);
    // It still takes the keyboard's `]`.
    pane.update(cx, |pane, cx| {
        pane.focused_thread = None;
        cx.notify();
    });
    press(cx, &shell, "]");
    assert_eq!(focused(&pane, cx), Some(id));
}

#[gpui::test]
fn a_finished_review_takes_no_comment(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let id = add_thread(&shell, "b.txt", AnchorSide::New, (2, 2), "Fine", cx);
    let session = session(&shell, cx);
    let finishing = session.update(cx, |session, cx| {
        session.finish_round(Some(Verdict::Approve), None, cx)
    });
    settle_drawn(cx, &shell, "the round", |_, _| true);
    cx.foreground_executor()
        .block_test(finishing)
        .expect("the round finishes");
    settle_drawn(cx, &shell, "the fold", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).is_read_only())
    });
    press(cx, &shell, "j c");
    assert_eq!(composer_target(&pane, cx), None);
    assert_eq!(
        plus("b.txt", "new", 2, cx),
        None,
        "no + on a finished review"
    );
    pane.update(cx, |pane, cx| {
        pane.set_thread_state(&id, ThreadState::Resolved, cx)
    });
    settle_drawn(cx, &shell, "nothing", |_, _| true);
    assert_eq!(threads(&shell, cx)[0].state, ThreadState::Open);
}

#[gpui::test]
fn the_composer_and_cards_stay_while_their_file_loads_again(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let card = add_thread(&shell, "b.txt", AnchorSide::New, (2, 2), "Two?", cx);
    // A comment in progress on b.txt's addition.
    pane.update(cx, |pane, cx| {
        let pos = LinePos {
            line: 4,
            side: AnchorSide::New,
        };
        pane.click_line("b.txt", pos, false, cx);
    });
    press(cx, &shell, "c");
    cx.simulate_input("half a sent");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    let rows_before = summary(&pane, "b.txt", cx).rows;
    let has_rows = |pane: &DiffPane| {
        let rows = &pane.blocks[1].rows;
        let composer = rows.iter().any(
            |row| matches!(row, Row::Insert(insert) if insert.item == rows::InsertItem::Composer),
        );
        let thread = rows.iter().any(|row| row.thread() == Some(&card));
        (composer, thread)
    };
    // The agent saves b.txt again: it loads again, and until the new diff lands its rows, the
    // card and the composer the reviewer is typing in stay where they are.
    let session = session(&shell, cx);
    session.update(cx, |session, cx| session.reload_file("b.txt", cx));
    pane.read_with(cx, |pane, _| {
        assert_eq!(has_rows(pane), (true, true));
        assert_eq!(pane.blocks[1].rows.len(), rows_before);
    });
    settle_drawn(cx, &shell, "the new diff", |_, _| true);
    pane.read_with(cx, |pane, cx| {
        assert_eq!(has_rows(pane), (true, true));
        assert_eq!(pane.composer_text(cx).as_deref(), Some("half a sent"));
    });
}

/// Whether the keyboard is on the stream (the main area).
fn stream_focused(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> bool {
    shell.update_in(cx, |shell, window, cx| {
        shell.focus_handle(cx).is_focused(window)
    })
}

fn composer_text(pane: &Entity<DiffPane>, cx: &mut VisualTestContext) -> Option<String> {
    pane.read_with(cx, |pane, cx| pane.composer_text(cx))
}

#[gpui::test]
fn a_composer_scrolled_out_of_view_keeps_the_keyboard(cx: &mut TestAppContext) {
    let repo = stream_fixture(300);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    // Every line of the long file shown, the composer on its first change.
    press(cx, &shell, "n shift-e");
    pane.update(cx, |pane, cx| {
        pane.expand("a/long.txt", 0, Expand::All, cx);
        pane.expand("a/long.txt", 0, Expand::All, cx);
    });
    press(cx, &shell, "c");
    cx.simulate_input("draft");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    // The wheel takes the stream far past it.
    pane.update(cx, |pane, cx| {
        let far = pane.starts[0] + 200;
        pane.list.scroll_to(ListOffset {
            item_ix: far,
            offset_in_item: px(0.),
        });
        cx.notify();
    });
    draw(cx);
    let item = pane.read_with(cx, |pane, _| pane.composer_item().expect("its row"));
    assert!(pane.read_with(cx, |pane, _| pane.is_off_screen(item)));
    // Typing still reaches the field, which comes back into view.
    cx.simulate_input("xyz");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    assert_eq!(composer_text(&pane, cx).as_deref(), Some("draftxyz"));
    assert!(!pane.read_with(cx, |pane, _| pane.is_off_screen(item)));
    // Scrolled away again, `Esc` in it still cancels it and hands the keyboard back.
    pane.update(cx, |pane, cx| {
        let far = pane.starts[0] + 200;
        pane.list.scroll_to(ListOffset {
            item_ix: far,
            offset_in_item: px(0.),
        });
        cx.notify();
    });
    draw(cx);
    press(cx, &shell, "escape");
    assert_eq!(composer_target(&pane, cx), None);
    assert!(stream_focused(&shell, cx));
}

#[gpui::test]
fn a_composer_whose_file_collapses_hands_back_the_keyboard_and_c_takes_it_up(
    cx: &mut TestAppContext,
) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    press(cx, &shell, "j c");
    cx.simulate_input("draft");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    // The panel's Viewed checkbox (a click that leaves the keyboard where it is) collapses
    // the composer's file.
    let list = shell.read_with(cx, |shell, _| shell.file_list().clone());
    list.update(cx, |list, cx| list.toggle_viewed("b.txt", cx));
    let session = session(&shell, cx);
    settle_drawn(cx, &shell, "the viewed mark", |_, cx| {
        session.read(cx).is_viewed("b.txt")
    });
    assert!(summary(&pane, "b.txt", cx).collapsed);
    // The draft is kept, and the stream has the keyboard: its keys work.
    assert_eq!(composer_text(&pane, cx).as_deref(), Some("draft"));
    assert!(stream_focused(&shell, cx));
    press(cx, &shell, "j");
    assert_eq!(current(&pane, cx).as_deref(), Some("c.txt"));
    // `C` opens the file again and takes the draft up where it was.
    press(cx, &shell, "c");
    assert!(!summary(&pane, "b.txt", cx).collapsed);
    assert_eq!(
        composer_target(&pane, cx).map(|target| (target.path, target.start_line)),
        Some(("b.txt".to_string(), 2))
    );
    cx.simulate_input("!");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    assert_eq!(composer_text(&pane, cx).as_deref(), Some("draft!"));
}

#[gpui::test]
fn c_without_a_cursor_starts_on_the_first_line_in_view_and_takes_up_a_draft(
    cx: &mut TestAppContext,
) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    // A review just opened has no cursor: `C` puts it on the first line in view, as `↓` does.
    assert_eq!(cursor(&pane, cx), None);
    press(cx, &shell, "c");
    assert_eq!(cursor(&pane, cx), new_line("a/long.txt", 7));
    assert_eq!(
        composer_target(&pane, cx).map(|target| (target.path, target.start_line)),
        Some(("a/long.txt".to_string(), 7))
    );
    cx.simulate_input("draft");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    // Its file and the next one viewed (collapsed), `J` lands on a file with no line for the
    // cursor...
    let list = shell.read_with(cx, |shell, _| shell.file_list().clone());
    let session = session(&shell, cx);
    for path in ["a/long.txt", "b.txt"] {
        list.update(cx, |list, cx| list.toggle_viewed(path, cx));
        settle_drawn(cx, &shell, "the viewed mark", |_, cx| {
            session.read(cx).is_viewed(path)
        });
    }
    press(cx, &shell, "j");
    assert_eq!(current(&pane, cx).as_deref(), Some("b.txt"));
    assert_eq!(cursor(&pane, cx), None);
    // ...where `C` still takes the draft up, opening its file again.
    press(cx, &shell, "c");
    assert!(!summary(&pane, "a/long.txt", cx).collapsed);
    cx.simulate_input("!");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    assert_eq!(composer_text(&pane, cx).as_deref(), Some("draft!"));
}

#[gpui::test]
fn a_draft_that_cant_be_shown_is_kept_rather_than_replaced(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    press(cx, &shell, "j c");
    cx.simulate_input("draft");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    // A composer with no row to draw in (as when its file's diff failed to load).
    pane.update(cx, |pane, _| {
        pane.composer.as_mut().expect("a composer").target.path = "not/shown.txt".to_string();
    });
    // With the keyboard on the stream, `C` leaves the draft where it is instead of opening a
    // new, empty composer.
    cx.update(|window, cx| pane.read(cx).focus_handle.clone().focus(window, cx));
    press(cx, &shell, "c");
    assert_eq!(composer_text(&pane, cx).as_deref(), Some("draft"));
    assert_eq!(
        composer_target(&pane, cx).map(|target| target.path),
        Some("not/shown.txt".to_string())
    );
    assert!(stream_focused(&shell, cx));
}

#[gpui::test]
fn a_composer_whose_file_leaves_the_diff_is_dropped(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    press(cx, &shell, "j c");
    cx.simulate_input("draft");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    // The agent reverts b.txt.
    repo.write("b.txt", "one\ntwo\nthree\n");
    let watched = pane.clone();
    settle_drawn(cx, &shell, "b.txt to leave", move |_, cx| {
        watched.read(cx).blocks.len() == 2
    });
    assert_eq!(composer_target(&pane, cx), None);
    assert!(stream_focused(&shell, cx));
    press(cx, &shell, "k");
    assert_eq!(current(&pane, cx).as_deref(), Some("a/long.txt"));
    // `C` comments on the cursor's line again.
    press(cx, &shell, "c");
    assert_eq!(
        composer_target(&pane, cx).map(|target| target.path),
        Some("a/long.txt".to_string())
    );
}

#[gpui::test]
fn a_reply_keeps_its_card_open_when_the_thread_resolves(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let id = add_thread(&shell, "b.txt", AnchorSide::New, (2, 2), "Two?", cx);
    pane.update_in(cx, |pane, window, cx| pane.start_reply(&id, window, cx));
    cx.simulate_input("half");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    // The card's Resolve (a click that leaves the keyboard in the field).
    pane.update(cx, |pane, cx| {
        pane.set_thread_state(&id, ThreadState::Resolved, cx)
    });
    let resolved = id.clone();
    let session = session(&shell, cx);
    settle_drawn(cx, &shell, "the resolve", move |_, cx| {
        session
            .read(cx)
            .review()
            .thread(&resolved)
            .is_some_and(|thread| thread.state == ThreadState::Resolved)
    });
    // The card stays open around the reply, which still takes the keys.
    assert!(pane.read_with(cx, |pane, _| {
        pane.is_thread_open(&id, ThreadState::Resolved)
    }));
    cx.simulate_input(" more");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    assert_eq!(composer_text(&pane, cx).as_deref(), Some("half more"));
}

#[gpui::test]
fn the_last_files_header_reaches_the_top_once_its_card_collapsed(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    // A thread on the last file, resolved: its card collapses to a line.
    let id = add_thread(&shell, "c.txt", AnchorSide::Old, (1, 1), "Why?", cx);
    press(cx, &shell, "j j");
    pane.update(cx, |pane, cx| {
        pane.set_thread_state(&id, ThreadState::Resolved, cx)
    });
    let session = session(&shell, cx);
    settle_drawn(cx, &shell, "the resolve", move |_, cx| {
        session
            .read(cx)
            .review()
            .thread(&id)
            .is_some_and(|thread| thread.state == ThreadState::Resolved)
    });
    let at_top = |pane: &DiffPane| {
        let top = pane.list.logical_scroll_top();
        (
            top.item_ix == pane.starts[2] && top.offset_in_item == px(0.),
            pane.pinned_file().map(str::to_string),
            pane.top_file().map(str::to_string),
        )
    };
    let shown = (true, None, Some("c.txt".to_string()));
    // `K` then `J`: its header at the top, nothing pinned over it.
    press(cx, &shell, "k j");
    assert_eq!(pane.read_with(cx, |pane, _| at_top(pane)), shown);
    // A click on its row in the panel.
    press(cx, &shell, "k");
    pane.update(cx, |pane, cx| pane.reveal_file("c.txt", cx));
    draw(cx);
    assert_eq!(pane.read_with(cx, |pane, _| at_top(pane)), shown);
}

fn selected(pane: &Entity<DiffPane>, cx: &mut VisualTestContext) -> Option<(u32, u32)> {
    pane.read_with(cx, |pane, _| {
        pane.selection()
            .map(|selection| (selection.start_line, selection.end_line))
    })
}

fn line_target(path: &str, side: AnchorSide, lines: (u32, u32)) -> Option<ComposerTarget> {
    Some(ComposerTarget {
        path: path.to_string(),
        side,
        start_line: lines.0,
        end_line: lines.1,
        reply_to: None,
    })
}

#[gpui::test]
fn a_lines_plus_comments_on_it_and_a_drag_from_it_on_the_lines_it_passes(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    // b.txt in split view: old `one two three`, new `one 2 three four`.
    press(cx, &shell, "j");
    let two = plus("b.txt", "new", 2, cx).expect("new line 2 has a +");

    // A click: the composer on that line.
    cx.simulate_click(two.center(), Modifiers::none());
    settle_drawn(cx, &shell, "the composer", |_, _| true);
    assert_eq!(
        composer_target(&pane, cx),
        line_target("b.txt", AnchorSide::New, (2, 2))
    );
    press(cx, &shell, "escape");
    assert_eq!(composer_target(&pane, cx), None);

    // A drag from new line 2: down to 4, up past where it started to 1, over the old half of
    // the row holding new line 3 (the drag keeps to the new side), then down to 4 again.
    let one = plus("b.txt", "new", 1, cx).expect("new line 1 has a +");
    let four = plus("b.txt", "new", 4, cx).expect("new line 4 has a +");
    let old_three = plus("b.txt", "old", 3, cx).expect("old line 3 has a +");
    let drag_to = |cx: &mut VisualTestContext, to: Bounds<Pixels>| {
        cx.simulate_mouse_move(to.center(), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    };
    cx.simulate_mouse_down(two.center(), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(cursor(&pane, cx), new_line("b.txt", 2));
    drag_to(cx, four);
    assert_eq!(selected(&pane, cx), Some((2, 4)));
    drag_to(cx, one);
    assert_eq!(selected(&pane, cx), Some((1, 2)));
    drag_to(cx, old_three);
    assert_eq!(selected(&pane, cx), Some((2, 3)));
    drag_to(cx, four);
    assert_eq!(
        composer_target(&pane, cx),
        None,
        "nothing opens while the button is down"
    );
    // Let go over the panel, outside the stream: the composer opens on the lines picked.
    let panel = bounds(cx, "panel").expect("the panel");
    cx.simulate_mouse_up(panel.center(), MouseButton::Left, Modifiers::none());
    settle_drawn(cx, &shell, "the composer", |_, _| true);
    assert_eq!(
        composer_target(&pane, cx),
        line_target("b.txt", AnchorSide::New, (2, 4))
    );
    press(cx, &shell, "escape");

    // Unified: a removed line's + comments on the old side.
    press(cx, &shell, "s");
    let removed = plus("b.txt", "old", 2, cx).expect("removed line 2 has a +");
    cx.simulate_click(removed.center(), Modifiers::none());
    settle_drawn(cx, &shell, "the composer", |_, _| true);
    assert_eq!(
        composer_target(&pane, cx),
        line_target("b.txt", AnchorSide::Old, (2, 2))
    );
}

#[gpui::test]
fn a_reply_opened_near_the_bottom_comes_wholly_into_view(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    // A card above gives the stream room to put this one's top low in the viewport.
    add_thread(
        &shell,
        "a/long.txt",
        AnchorSide::New,
        (70, 70),
        "Why a word?",
        cx,
    );
    let thread = add_thread(&shell, "b.txt", AnchorSide::New, (2, 2), "Why a digit?", cx);
    let pane = pane(&shell, cx);
    // Its card's top just inside the viewport's bottom edge.
    pane.update(cx, |pane, cx| pane.focus_thread(&thread, cx));
    settle_drawn(cx, &shell, "the card focused", |_, _| true);
    pane.update(cx, |pane, cx| {
        let item = pane.thread_item(&thread).expect("its card");
        let height = pane.viewport_height();
        // Room for the card as it is, not for the composer it grows by.
        pane.scroll_item_to(item, height - pane.item_height(item) - 10.);
        pane.viewport_changed(cx);
    });
    draw(cx);
    press(cx, &shell, "r");
    settle_drawn(cx, &shell, "the composer laid out", |_, _| true);
    let (bottom, height) = pane.read_with(cx, |pane, _| {
        let item = pane.composer_item().expect("the composer's card");
        let height = pane.viewport_height();
        let top = pane.offset_of(item, height * 2.).expect("in view");
        (top + pane.item_height(item), height)
    });
    assert!(
        bottom <= height,
        "the card ends at {bottom}, past the viewport's {height}"
    );
}

/// A post's answer can reach the pane after the review's reload has already drawn the new
/// card (a loaded machine): the card is revealed then, and nothing is left waiting to scroll
/// the stream back to it on a later change.
#[gpui::test]
fn a_post_answered_after_its_card_is_drawn_leaves_nothing_to_reveal_later(cx: &mut TestAppContext) {
    let repo = stream_fixture(80);
    let (_dir, shell, cx) = open(&repo, cx);
    let pane = pane(&shell, cx);
    let thread = add_thread(&shell, "b.txt", AnchorSide::New, (2, 2), "Why a digit?", cx);
    let target = ComposerTarget {
        path: "b.txt".to_string(),
        side: AnchorSide::New,
        start_line: 2,
        end_line: 2,
        reply_to: None,
    };
    pane.update_in(cx, |pane, window, cx| {
        pane.posted(target, Ok(thread.clone()), window, cx);
    });
    settle_drawn(cx, &shell, "the answer", |_, _| true);
    assert_eq!(focused(&pane, cx), Some(thread));

    // The reader moves on to c.txt; a later change to the rows (`S`) leaves them there.
    press(cx, &shell, "j j");
    pane.read_with(cx, |pane, _| assert_eq!(pane.top_file(), Some("c.txt")));
    press(cx, &shell, "s");
    pane.read_with(cx, |pane, _| assert_eq!(pane.top_file(), Some("c.txt")));
}
