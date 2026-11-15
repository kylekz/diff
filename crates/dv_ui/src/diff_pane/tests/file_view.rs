//! The file view over real fixture repos (PLAN §18.1, DESIGN §4.1 "File view"): `F` and the
//! header menu open a file whole with its changed lines marked, `Esc` returns to the stream
//! where it was, `Alt ←` / `Alt →` step through the history, an unchanged file reads from the
//! new side and takes a comment that the stream shows in its "Not in the diff" section, and
//! `Ctrl F` and `Ctrl C` work on the file shown.

use dv_core::review::Placement;
use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, point, px};
use pretty_assertions::assert_eq;

use super::*;
use crate::diff_pane::file_view::FileViewSummary;
use crate::system::SystemRequest;

/// [`stream_fixture`] plus `u.txt`, which the review doesn't change.
fn view_fixture() -> FixtureRepo {
    let repo = FixtureRepo::new();
    let long: String = (1..=80).map(|i| format!("line {i}\n")).collect();
    repo.write("a/long.txt", &long);
    repo.write("b.txt", "one\ntwo\nthree\n");
    repo.write("u.txt", "alpha\nbeta\ngamma one\ndelta\n");
    repo.commit("base");
    let edited = long
        .replace("line 10\n", "line ten\n")
        .replace("line 70\n", "line seventy\n");
    repo.write("a/long.txt", &edited);
    repo.write("b.txt", "one\n2\nthree\nfour\n");
    repo
}

fn file_view(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Entity<DiffPane> {
    shell.read_with(cx, |shell, _| shell.file_view().clone())
}

fn summary_of(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Option<FileViewSummary> {
    let view = file_view(shell, cx);
    shell.read_with(cx, |shell, cx| {
        shell
            .is_file_view_open()
            .then(|| view.read(cx).file_view_summary(cx))
            .flatten()
    })
}

/// Waits for the file view to show its text.
fn settle_view(shell: &Entity<Shell>, cx: &mut VisualTestContext) {
    let view = file_view(shell, cx);
    settle_drawn(cx, shell, "the file view's text", move |_, cx| {
        view.read(cx)
            .file_view_summary(cx)
            .is_some_and(|summary| !summary.loading)
    });
}

fn history(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> (usize, usize) {
    shell.read_with(cx, |shell, _| shell.history().counts())
}

#[gpui::test]
fn f_opens_the_current_file_whole_and_esc_returns_to_the_stream_where_it_was(
    cx: &mut TestAppContext,
) {
    let repo = view_fixture();
    let (_dir, shell, cx) = open_target(&repo, Target::Working, 2, cx);
    let stream = pane(&shell, cx);
    press(cx, &shell, "down down down down");
    assert_eq!(cursor(&stream, cx), new_line("a/long.txt", 10));
    press(cx, &shell, "f");
    settle_view(&shell, cx);
    let view = file_view(&shell, cx);
    let shown = summary_of(&shell, cx).expect("the file view shows");
    assert_eq!(shown.path, "a/long.txt");
    assert_eq!(shown.side, AnchorSide::New);
    assert_eq!(shown.line_count, 80, "the new side whole, no removed lines");
    assert!(shown.changed);
    assert_eq!(shown.changed_lines, [10, 70]);
    // The cursor is where it was in the stream; every line shows, no gap, no footer.
    assert_eq!(cursor(&view, cx), new_line("a/long.txt", 10));
    view.read_with(cx, |view, _| {
        let rows = &view.blocks[0].rows;
        assert_eq!(rows.len(), 1 + 80);
        assert!(rows[1..].iter().all(|row| matches!(row, Row::Line(_))));
    });
    // The stream's keys work on it: the cursor moves within the file.
    press(cx, &shell, "down");
    assert_eq!(cursor(&view, cx), new_line("a/long.txt", 11));
    // S and V do nothing here.
    press(cx, &shell, "s v");
    assert_eq!(
        view.read_with(cx, |view, _| view.view_mode()),
        ViewMode::Unified
    );
    // Esc goes back to the stream, at the line it left.
    press(cx, &shell, "escape");
    assert!(summary_of(&shell, cx).is_none());
    assert_eq!(cursor(&stream, cx), new_line("a/long.txt", 10));
    assert_eq!(history(&shell, cx), (2, 0));
    let focused = shell.update_in(cx, |shell, window, cx| {
        stream.read(cx).focus_handle(cx).is_focused(window) && !shell.is_file_view_open()
    });
    assert!(focused, "the keyboard is back on the stream");
}

#[gpui::test]
fn alt_left_and_right_step_back_and_forth_between_the_views(cx: &mut TestAppContext) {
    let repo = view_fixture();
    let (_dir, shell, cx) = open_target(&repo, Target::Working, 2, cx);
    let stream = pane(&shell, cx);
    press(cx, &shell, "j");
    assert_eq!(cursor(&stream, cx), new_line("b.txt", 2));
    // The header menu's View file opens it at the cursor's line.
    stream.update(cx, |pane, cx| pane.view_file("b.txt", cx));
    settle_view(&shell, cx);
    let view = file_view(&shell, cx);
    assert_eq!(cursor(&view, cx), new_line("b.txt", 2));
    press(cx, &shell, "down down");
    assert_eq!(history(&shell, cx), (1, 0));
    press(cx, &shell, "alt-left");
    assert!(summary_of(&shell, cx).is_none(), "back in the stream");
    assert_eq!(cursor(&stream, cx), new_line("b.txt", 2));
    assert_eq!(history(&shell, cx), (0, 1));
    press(cx, &shell, "alt-right");
    settle_view(&shell, cx);
    assert_eq!(
        summary_of(&shell, cx).map(|view| view.path).as_deref(),
        Some("b.txt")
    );
    assert_eq!(cursor(&view, cx), new_line("b.txt", 4), "where it was left");
    assert_eq!(history(&shell, cx), (1, 0));
    // A panel row that shows the stream leaves the file view, as a jump.
    let list = shell.read_with(cx, |shell, _| shell.file_list().clone());
    list.update(cx, |list, cx| list.reveal("a/long.txt", cx));
    settle_drawn(cx, &shell, "the stream", |_, _| true);
    assert!(summary_of(&shell, cx).is_none());
    assert_eq!(current(&stream, cx).as_deref(), Some("a/long.txt"));
    assert_eq!(history(&shell, cx), (2, 0));
}

#[gpui::test]
fn an_unchanged_file_takes_a_comment_that_stands_in_it_not_in_the_diff(cx: &mut TestAppContext) {
    let repo = view_fixture();
    let (_dir, shell, cx) = open_target(&repo, Target::Working, 2, cx);
    let stream = pane(&shell, cx);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_file_view("u.txt", Some(3), true, window, cx)
        })
    });
    settle_view(&shell, cx);
    let view = file_view(&shell, cx);
    let shown = summary_of(&shell, cx).expect("the file view shows");
    assert_eq!((shown.line_count, shown.changed), (4, false));
    assert!(shown.changed_lines.is_empty());
    assert_eq!(cursor(&view, cx), new_line("u.txt", 3));
    // `C` comments on the cursor's line, as in the stream.
    press(cx, &shell, "c");
    cx.simulate_input("Why gamma?");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    press(cx, &shell, "secondary-enter");
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().expect("open"));
    let (thread, placement, not_in_diff) = session.read_with(cx, |session, _| {
        let thread = session
            .review()
            .live_threads()
            .next()
            .cloned()
            .expect("the thread was added");
        let placement = session.placement(&thread);
        let section: Vec<_> = session
            .threads_not_in_diff()
            .map(|t| t.id.clone())
            .collect();
        (thread, placement, section)
    });
    assert_eq!(thread.anchor.path, "u.txt");
    assert_eq!((thread.anchor.start_line, thread.anchor.end_line), (3, 3));
    assert_eq!(thread.anchor.text, ["gamma one"]);
    assert_eq!(thread.anchor.context_before, ["alpha", "beta"]);
    assert_eq!(
        placement.placement(),
        &Placement::Unlisted {
            path: "u.txt".to_string(),
            start_line: 3,
            end_line: 3,
            how: None,
        }
    );
    assert!(placement.is_not_in_diff() && !placement.is_outdated());
    assert_eq!(not_in_diff, std::slice::from_ref(&thread.id));
    // Its card shows inline in the file view, under its line.
    settle_drawn(cx, &shell, "its card", |_, _| true);
    view.read_with(cx, |view, _| {
        let rows = &view.blocks[0].rows;
        let at = rows
            .iter()
            .position(|row| row.thread().and_then(ThreadKey::local) == Some(&thread.id))
            .expect("the card is in the file view");
        assert_eq!(rows[at - 1], Row::Line(2));
    });
    // Back in the stream, it heads the "Not in the diff" section.
    press(cx, &shell, "escape");
    assert!(summary_of(&shell, cx).is_none());
    let section = stream.read_with(cx, |pane, cx| pane.section_summary(cx));
    assert_eq!(
        section.map(|section| section.threads),
        Some(vec![thread.id.clone()])
    );
    let location = session.read_with(cx, |session, _| session.location(&thread));
    assert_eq!(location, "u.txt:3");
}

#[gpui::test]
fn ctrl_f_searches_the_file_shown_and_ctrl_c_copies_from_it(cx: &mut TestAppContext) {
    let repo = view_fixture();
    let (_dir, shell, cx) = open_target(&repo, Target::Working, 2, cx);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_file_view("u.txt", None, true, window, cx)
        })
    });
    settle_view(&shell, cx);
    let view = file_view(&shell, cx);
    press(cx, &shell, "secondary-f");
    cx.simulate_input("a");
    settle_drawn(cx, &shell, "the search", |_, _| true);
    let found = view.read_with(cx, |view, _| view.find_summary().expect("open"));
    // alpha (2), beta (1), gamma one (2), delta (1): u.txt alone, the stream's files unsearched.
    assert_eq!((found.count, found.complete, found.skipped), (6, true, 0));
    assert_eq!(found.current, Some(1));
    press(cx, &shell, "enter enter enter");
    assert_eq!(cursor(&view, cx), new_line("u.txt", 3));
    press(cx, &shell, "escape");
    let open = view.read_with(cx, |view, _| view.find_summary().map(|find| find.open));
    assert_eq!(open, Some(false));
    assert!(
        summary_of(&shell, cx).is_some(),
        "Esc closed the bar, not the view"
    );
    // A triple-click selects line 4's text; Ctrl C copies it.
    let cell = bounds(cx, "file_view/code/4").expect("line 4 drew");
    let at = point(cell.origin.x + px(8.), cell.center().y);
    for event in [true, false] {
        if event {
            cx.simulate_event(MouseDownEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::none(),
                click_count: 3,
                first_mouse: false,
            });
        } else {
            cx.simulate_event(MouseUpEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::none(),
                click_count: 3,
            });
        }
    }
    draw(cx);
    let selected = view.read_with(cx, |view, _| view.text_selection().map(|s| s.text));
    assert_eq!(selected.as_deref(), Some("delta"));
    press(cx, &shell, "secondary-c");
    let copied: Vec<String> = cx.update(|_, cx| {
        crate::system::requests(cx)
            .0
            .into_iter()
            .filter_map(|request| match request {
                SystemRequest::Clipboard(text) => Some(text),
                _ => None,
            })
            .collect()
    });
    assert_eq!(copied, ["delta"]);
}

/// Whether `pane`'s composer field has the keyboard.
fn composer_focused(pane: &Entity<DiffPane>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| {
        pane.read(cx)
            .composer()
            .is_some_and(|composer| composer.input.focus_handle(cx).is_focused(window))
    })
}

#[gpui::test]
fn a_draft_in_the_file_view_is_never_dropped_by_a_switch(cx: &mut TestAppContext) {
    let repo = view_fixture();
    let (_dir, shell, cx) = open_target(&repo, Target::Working, 2, cx);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_file_view("u.txt", Some(2), true, window, cx)
        })
    });
    settle_view(&shell, cx);
    let view = file_view(&shell, cx);
    press(cx, &shell, "c");
    cx.simulate_input("half a thought");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    let draft = |cx: &mut VisualTestContext| view.read_with(cx, |view, cx| view.composer_text(cx));
    // Another file (Ctrl P, View file, F): nothing switches, the draft takes the keyboard.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_file_view("b.txt", None, true, window, cx)
        })
    });
    settle_drawn(cx, &shell, "the switch", |_, _| true);
    assert_eq!(
        summary_of(&shell, cx).map(|view| view.path).as_deref(),
        Some("u.txt")
    );
    assert_eq!(draft(cx).as_deref(), Some("half a thought"));
    assert!(composer_focused(&view, cx));
    // Alt ← and the close button: the same.
    let history_before = history(&shell, cx);
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.go_back(window, cx)));
    settle_drawn(cx, &shell, "back", |_, _| true);
    assert!(summary_of(&shell, cx).is_some());
    assert_eq!(history(&shell, cx), history_before, "nothing stepped");
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.close_file_view(true, window, cx)));
    settle_drawn(cx, &shell, "close", |_, _| true);
    assert!(summary_of(&shell, cx).is_some());
    assert_eq!(draft(cx).as_deref(), Some("half a thought"));
    // Esc discards it (its explicit discard), and Esc again closes the view.
    press(cx, &shell, "escape");
    assert_eq!(draft(cx), None);
    press(cx, &shell, "escape");
    assert!(summary_of(&shell, cx).is_none());
    // An empty composer goes with its file.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_file_view("u.txt", Some(1), true, window, cx)
        })
    });
    settle_view(&shell, cx);
    press(cx, &shell, "c");
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_file_view("b.txt", None, true, window, cx)
        })
    });
    settle_view(&shell, cx);
    assert_eq!(
        summary_of(&shell, cx).map(|view| view.path).as_deref(),
        Some("b.txt")
    );
    assert_eq!(draft(cx), None);
}

#[gpui::test]
fn a_reply_from_the_comments_list_keeps_the_keyboard_as_the_file_view_closes(
    cx: &mut TestAppContext,
) {
    let repo = view_fixture();
    let (_dir, shell, cx) = open_target(&repo, Target::Working, 2, cx);
    let stream = pane(&shell, cx);
    press(cx, &shell, "j");
    press(cx, &shell, "c");
    cx.simulate_input("first");
    press(cx, &shell, "secondary-enter");
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().expect("open"));
    let thread = session.read_with(cx, |session, _| {
        session
            .review()
            .live_threads()
            .next()
            .expect("posted")
            .id
            .clone()
    });
    press(cx, &shell, "f");
    settle_view(&shell, cx);
    let list = shell.read_with(cx, |shell, _| shell.comments_list().clone());
    cx.update(|window, cx| list.update(cx, |list, cx| list.reply(&thread, window, cx)));
    settle_drawn(cx, &shell, "the reply", |_, _| true);
    assert!(summary_of(&shell, cx).is_none(), "the stream shows");
    assert!(
        composer_focused(&stream, cx),
        "the reply field has the keyboard"
    );
    cx.simulate_input("zzz");
    settle_drawn(cx, &shell, "typing", |_, _| true);
    assert_eq!(
        stream
            .read_with(cx, |pane, cx| pane.composer_text(cx))
            .as_deref(),
        Some("zzz")
    );
}

#[gpui::test]
fn f_on_a_removed_line_opens_the_file_view_at_the_line_after_it(cx: &mut TestAppContext) {
    let repo = view_fixture();
    let (_dir, shell, cx) = open_target(&repo, Target::Working, 2, cx);
    let stream = pane(&shell, cx);
    // b.txt: `two` became `2`; the cursor on the old side's `two` (split view, ←).
    press(cx, &shell, "j left");
    assert_eq!(
        cursor(&stream, cx),
        Some(("b.txt".to_string(), AnchorSide::Old, 2))
    );
    press(cx, &shell, "f");
    settle_view(&shell, cx);
    let view = file_view(&shell, cx);
    assert_eq!(cursor(&view, cx), new_line("b.txt", 2));
}
