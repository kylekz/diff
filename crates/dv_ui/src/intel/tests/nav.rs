//! Where places go and what the references list reads (PLAN §18.1, §18.2): another side's rows
//! read from the text the server read, the review's files listed first, go to definition from
//! the working tree's file view staying there, a place waiting for its file's diff dropped once
//! the reviewer moved on, `Ctrl`+click's line kept for `Alt ←`, and a rescan closing every open
//! document.

use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent};
use pretty_assertions::assert_eq;

use super::*;

/// One row of the References tab: `(path, line, text, changed, side)`.
type Row = (String, u32, Option<String>, bool, Option<AnchorSide>);

/// The References tab's rows.
fn rows(intel: &Entity<CodeIntel>, cx: &mut VisualTestContext) -> Vec<Row> {
    intel.read_with(cx, |intel, _| {
        intel
            .references()
            .expect("a list")
            .shown()
            .into_iter()
            .flat_map(|(file, rows)| {
                rows.into_iter().map(move |(row, changed)| {
                    (
                        file.label().to_string(),
                        row.line,
                        row.text.clone(),
                        changed,
                        file.side,
                    )
                })
            })
            .collect()
    })
}

fn wait_for_list(intel: &Entity<CodeIntel>, shell: &Entity<Shell>, cx: &mut VisualTestContext) {
    let listed = intel.clone();
    wait(cx, shell, "the references", move |cx| {
        listed
            .read(cx)
            .references()
            .is_some_and(|list| !list.is_loading() && !list.is_reading())
    });
}

#[gpui::test]
fn old_side_references_read_the_old_text_and_list_the_reviews_files_first(cx: &mut TestAppContext) {
    let repo = project();
    let root = WorkspacePaths::new(&repo.path().display().to_string()).uri_text("");
    let answer: Answer = Arc::new(move |message| {
        (message["method"] == "textDocument/references").then(|| {
            at(
                &root,
                json!([location("package.json", 0, 1), location("src/a.ts", 1, 13),]),
            )
        })
    });
    let (_dir, shell, cx, _fakes) = open_review(&repo, Some(answer), cx);
    wait(cx, &shell, "the server", |_| true);
    let intel = intel(&shell, cx);
    // `one` on the old side of src/a.ts's line 2, which the review changed.
    let (at_new, diff) = symbol(&shell, "src/a.ts", 2, "one", cx);
    let at_old = SymbolAt {
        side: AnchorSide::Old,
        ..at_new
    };
    intel.update(cx, |intel, cx| {
        intel.references_at(at_old, Pane::Stream, &diff, cx)
    });
    wait_for_list(&intel, &shell, cx);
    // The review's file first, its row the old side's text (as the server read it), marked
    // changed; then the rest in path order, read from the working tree.
    assert_eq!(
        rows(&intel, cx),
        [
            (
                "src/a.ts".to_string(),
                2,
                Some("export const one = 1;".to_string()),
                true,
                Some(AnchorSide::Old),
            ),
            (
                "package.json".to_string(),
                1,
                Some("{\"name\": \"web\"}".to_string()),
                false,
                None,
            ),
        ]
    );
    // A click on the row goes to that side's line.
    let location = intel.read_with(cx, |intel, _| {
        intel.references().unwrap().files.as_ref().unwrap()[0].rows[0]
            .location
            .clone()
    });
    let asked = intel.read_with(cx, |intel, _| {
        let list = intel.references().unwrap();
        Asked {
            at: list.from.clone(),
            pane: list.pane,
            worktree: !list.against_worktree,
            definition: false,
        }
    });
    intel.update(cx, |intel, cx| intel.go_to(location, Some(asked), cx));
    cx.run_until_parked();
    let stream = shell.read_with(cx, |shell, _| shell.diff_pane().clone());
    assert_eq!(
        stream
            .read_with(cx, |pane, _| pane.cursor())
            .map(|c| (c.side, c.line)),
        Some((AnchorSide::Old, 2))
    );
}

#[gpui::test]
fn go_to_definition_in_the_working_trees_file_view_stays_on_the_working_tree(
    cx: &mut TestAppContext,
) {
    let repo = project();
    repo.git(&["add", "src/a.ts"]);
    let root = WorkspacePaths::new(&repo.path().display().to_string()).uri_text("");
    let answer: Answer = Arc::new(move |message| match message["method"].as_str() {
        Some("textDocument/definition") => Some(at(&root, json!([location("src/b.ts", 0, 16)]))),
        Some("textDocument/hover") => Some(json!({"contents": "function helper"})),
        _ => None,
    });
    let (_dir, shell, cx, _fakes) = open_target(&repo, Some(answer), Target::Staged, cx);
    wait(cx, &shell, "the server", |_| true);
    let intel = intel(&shell, cx);
    let (at, diff) = symbol(&shell, "src/a.ts", 2, "helper", cx);
    // From the stream (the index): a place in another file is the working tree's.
    intel.update(cx, |intel, cx| {
        intel.definition(at, Pane::Stream, &diff, cx)
    });
    let view = shell.read_with(cx, |shell, _| shell.file_view().clone());
    let shown = view.clone();
    wait(cx, &shell, "the file view", move |cx| {
        shown
            .read(cx)
            .file_view_summary(cx)
            .is_some_and(|summary| !summary.loading && summary.worktree)
    });
    // From the working tree's file view: the server reads that text from the disk, so a place
    // in the same file stays in the working tree's file view, unlabelled.
    let (at_view, view_diff) = view.read_with(cx, |view, _| {
        view.symbol_at_caret_for_tests()
            .expect("the caret's symbol")
    });
    assert!(at_view.worktree);
    assert!(intel.read_with(cx, |intel, cx| intel.reads_worktree(&at_view, cx)));
    intel.update(cx, |intel, cx| {
        intel.hover(
            at_view.clone(),
            Pane::FileView,
            Default::default(),
            &view_diff,
            cx,
        )
    });
    let card = intel.clone();
    wait(cx, &shell, "the hover", move |cx| {
        card.read(cx).hover_card().is_some()
    });
    assert!(!intel.read_with(cx, |intel, _| intel.hover_card().unwrap().against_worktree));
    intel.update(cx, |intel, cx| {
        intel.definition(at_view, Pane::FileView, &view_diff, cx)
    });
    wait(cx, &shell, "the place", |_| true);
    let summary = view
        .read_with(cx, |view, cx| view.file_view_summary(cx))
        .unwrap();
    assert_eq!(
        (summary.path.as_str(), summary.worktree),
        ("src/b.ts", true)
    );
}

#[gpui::test]
fn a_place_waiting_for_its_files_diff_is_dropped_once_the_reviewer_moves_on(
    cx: &mut TestAppContext,
) {
    // Twelve long changed files after src/a.ts: the last isn't loaded until something asks.
    let repo = FixtureRepo::new();
    repo.write(
        "package.json",
        "{\"name\": \"web\"}
",
    );
    repo.write(
        "src/a.ts",
        "import { helper } from './z';
export const one = 1;
",
    );
    let long = |n: usize, edited: bool| -> String {
        (1..=60)
            .map(|line| {
                format!(
                    "export const v{n}_{line} = {};
",
                    if edited { line * 2 } else { line }
                )
            })
            .collect()
    };
    for n in 0..12 {
        repo.write(format!("src/z{n:02}.ts"), long(n, false));
    }
    repo.commit("base");
    repo.write(
        "src/a.ts",
        "import { helper } from './z';
export const one = helper(0);
",
    );
    for n in 0..12 {
        repo.write(format!("src/z{n:02}.ts"), long(n, true));
    }
    let root = WorkspacePaths::new(&repo.path().display().to_string()).uri_text("");
    let answer: Answer = Arc::new(move |message| {
        (message["method"] == "textDocument/definition")
            .then(|| at(&root, json!([location("src/z11.ts", 1, 13)])))
    });
    let (_dir, shell, cx, _fakes) = open_review(&repo, Some(answer), cx);
    wait(cx, &shell, "the server", |_| true);
    let intel = intel(&shell, cx);
    let session = shell.read_with(cx, |shell, _| shell.session().cloned().unwrap());
    assert!(session.read_with(cx, |session, _| session.shown_diff("src/z11.ts").is_none()));
    let (at, diff) = symbol(&shell, "src/a.ts", 2, "helper", cx);
    // The place's file is listed and not loaded: it waits for its diff, which is held back.
    session.update(cx, |session, cx| session.hold_loads(true, cx));
    intel.update(cx, |intel, cx| {
        intel.definition(at, Pane::Stream, &diff, cx)
    });
    // (Loads held, nothing settles: the answer is waited for by hand.)
    let deadline = Instant::now() + Duration::from_secs(5);
    while intel.read_with(cx, |intel, _| intel.last_definitions().is_none()) {
        assert!(Instant::now() < deadline, "the definition never answered");
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    // The reviewer opens another file meanwhile: the place is forgotten.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_file_view("src/a.ts", None, true, window, cx)
        })
    });
    session.update(cx, |session, cx| session.hold_loads(false, cx));
    wait(cx, &shell, "the loads", |_| true);
    let summary = shell.read_with(cx, |shell, cx| {
        shell.file_view().read(cx).file_view_summary(cx)
    });
    assert!(shell.read_with(cx, |shell, _| shell.is_file_view_open()));
    assert_eq!(summary.map(|summary| summary.path), Some("src/a.ts".into()));
    let stream = shell.read_with(cx, |shell, _| shell.diff_pane().clone());
    assert_ne!(
        stream
            .read_with(cx, |pane, _| pane.cursor())
            .map(|c| c.path),
        Some("src/z11.ts".into())
    );
}

#[gpui::test]
fn ctrl_click_puts_the_cursor_on_its_line_which_alt_left_comes_back_to(cx: &mut TestAppContext) {
    let repo = project();
    let root = WorkspacePaths::new(&repo.path().display().to_string()).uri_text("");
    let answer: Answer = Arc::new(move |message| {
        (message["method"] == "textDocument/definition")
            .then(|| at(&root, json!([location("src/b.ts", 0, 16)])))
    });
    let (_dir, shell, cx, _fakes) = open_review(&repo, Some(answer), cx);
    wait(cx, &shell, "the server", |_| true);
    let stream = shell.read_with(cx, |shell, _| shell.diff_pane().clone());
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    // The cursor elsewhere first: on line 1.
    stream.update(cx, |pane, cx| {
        pane.show_place("src/a.ts", AnchorSide::New, 1, 0, cx)
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let (at, _) = symbol(&shell, "src/a.ts", 2, "helper", cx);
    let point = stream
        .read_with(cx, |pane, _| {
            pane.char_point_for_tests("src/a.ts", AnchorSide::New, 2, at.range.start + 1)
        })
        .expect("line 2 drew");
    let modifiers = cx.update(|_, cx| Modifiers::secondary_key(cx));
    cx.simulate_event(MouseDownEvent {
        button: MouseButton::Left,
        position: point,
        modifiers,
        click_count: 1,
        first_mouse: false,
    });
    cx.simulate_event(MouseUpEvent {
        button: MouseButton::Left,
        position: point,
        modifiers,
        click_count: 1,
    });
    let view = shell.read_with(cx, |shell, _| shell.file_view().clone());
    let shown = view.clone();
    wait(cx, &shell, "the definition", move |cx| {
        shown.read(cx).file_view_path() == Some("src/b.ts")
    });
    // Alt ← comes back to the line clicked, not where the cursor was before.
    cx.simulate_keystrokes("alt-left");
    cx.run_until_parked();
    assert!(!shell.read_with(cx, |shell, _| shell.is_file_view_open()));
    assert_eq!(
        stream
            .read_with(cx, |pane, _| pane.cursor())
            .map(|c| (c.path, c.line)),
        Some(("src/a.ts".into(), 2))
    );
}

#[gpui::test]
fn a_rescan_closes_every_open_document(cx: &mut TestAppContext) {
    let repo = project();
    let answer: Answer = Arc::new(|message| {
        (message["method"] == "textDocument/hover").then(|| json!({"contents": "x"}))
    });
    let (_dir, shell, cx, fakes) = open_review(&repo, Some(answer), cx);
    wait(cx, &shell, "the server", |_| true);
    let intel = intel(&shell, cx);
    // The working tree's text stays open once answered.
    let (at, diff) = symbol(&shell, "src/a.ts", 2, "helper", cx);
    intel.update(cx, |intel, cx| {
        intel.hover(at, Pane::Stream, Default::default(), &diff, cx)
    });
    let card = intel.clone();
    wait(cx, &shell, "the hover", move |cx| {
        card.read(cx).hover_card().is_some()
    });
    assert!(fakes.with_method("textDocument/didClose").is_empty());
    // A rescan: anything may have changed, so every open document is closed.
    let worktree = intel.read_with(cx, |intel, _| intel.worktree().clone());
    let servers = cx.update(|_, cx| cx.services().language_servers.clone());
    servers.update(cx, |servers, cx| {
        servers.files_changed(&worktree, &[], true, cx)
    });
    wait(cx, &shell, "the change", |_| true);
    let deadline = Instant::now() + Duration::from_secs(5);
    while fakes.with_method("textDocument/didClose").is_empty() {
        assert!(Instant::now() < deadline, "the document was never closed");
        std::thread::sleep(Duration::from_millis(5));
    }
    let closed = fakes.with_method("textDocument/didClose");
    assert!(
        closed[0]["params"]["textDocument"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.ends_with("src/a.ts")),
        "{closed:?}"
    );
}

#[gpui::test]
fn a_hand_edit_of_lsp_a_write_took_in_still_reaches_the_servers_on_check_again(
    cx: &mut TestAppContext,
) {
    let repo = project();
    let (_dir, shell, cx, _fakes) = open_review(&repo, None, cx);
    wait(cx, &shell, "the review", |_| true);
    let (settings, servers) = cx.update(|_, cx| {
        (
            cx.services().settings.clone(),
            cx.services().language_servers.clone(),
        )
    });
    let path = settings.read_with(cx, |settings, _| settings.path().to_path_buf());
    // The reviewer writes `lsp` by hand; then dv writes the file (a mode change), taking it in.
    let lsp = json!({"typescript": {"command": ["/x/tsc", "--lsp", "--stdio"]}});
    std::fs::write(&path, json!({"mode": "dark", "lsp": lsp}).to_string()).unwrap();
    settings.update(cx, |settings, cx| {
        settings.set_mode(crate::services::Mode::Light, cx)
    });
    wait(cx, &shell, "the write", |_| true);
    assert_ne!(
        servers.read_with(cx, |servers, _| servers.settings().clone()),
        dv_lsp::LspSettings::from_json(&lsp)
    );
    // Check again reads it, and the servers have it.
    intel(&shell, cx).update(cx, |intel, cx| intel.check_again(cx));
    wait(cx, &shell, "the recheck", |_| true);
    assert_eq!(
        servers.read_with(cx, |servers, _| servers.settings().clone()),
        dv_lsp::LspSettings::from_json(&lsp)
    );
}

#[gpui::test]
fn a_large_file_outside_the_repository_loads_anyway_unless_dv_couldnt_read_it(
    cx: &mut TestAppContext,
) {
    let repo = project();
    // A library's declarations over the display caps (25,000 lines), and one over the 4 MiB
    // dv reads of a file outside the repository.
    let outside = repo.outside_dir("lib");
    let long: String = (1..=25_000)
        .map(|n| {
            format!(
                "declare const x{n}: number;
"
            )
        })
        .collect();
    std::fs::write(outside.join("long.d.ts"), &long).unwrap();
    std::fs::write(outside.join("huge.d.ts"), "x".repeat(5 * 1024 * 1024)).unwrap();
    let lib = WorkspacePaths::new(&outside.display().to_string());
    let (long_uri, huge_uri) = (lib.uri_text("long.d.ts"), lib.uri_text("huge.d.ts"));
    let calls = Arc::new(Mutex::new(0));
    let answer: Answer = Arc::new(move |message| {
        if message["method"] != "textDocument/definition" {
            return None;
        }
        let mut calls = calls.lock();
        *calls += 1;
        let range =
            json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}});
        Some(json!([{"uri": if *calls == 1 { &long_uri } else { &huge_uri }, "range": range}]))
    });
    let (_dir, shell, cx, _fakes) = open_review(&repo, Some(answer), cx);
    wait(cx, &shell, "the server", |_| true);
    let intel = intel(&shell, cx);
    let view = shell.read_with(cx, |shell, _| shell.file_view().clone());
    let (at, diff) = symbol(&shell, "src/a.ts", 2, "helper", cx);
    let summary = |cx: &mut VisualTestContext| {
        view.read_with(cx, |view, cx| view.file_view_summary(cx))
            .expect("the file view")
    };
    let read = view.clone();
    let landed = move |cx: &App| {
        read.read(cx)
            .file_view_summary(cx)
            .is_some_and(|summary| !summary.loading)
    };
    intel.update(cx, |intel, cx| {
        intel.definition(at.clone(), Pane::Stream, &diff, cx)
    });
    wait(cx, &shell, "the long file", landed.clone());
    assert_eq!(summary(cx).notice, Some("too_large"));
    // Read whole already: Load anyway shows its lines.
    view.update(cx, |view, cx| view.file_view_load_anyway(cx));
    wait(cx, &shell, "its lines", |_| true);
    let shown = summary(cx);
    assert_eq!((shown.notice, shown.line_count), (None, 25_000));
    // Past what dv reads: too large, and nothing more to load.
    intel.update(cx, |intel, cx| {
        intel.definition(at, Pane::Stream, &diff, cx)
    });
    let huge = view.clone();
    wait(cx, &shell, "the huge file", move |cx| {
        huge.read(cx)
            .file_view_summary(cx)
            .is_some_and(|summary| summary.path.ends_with("huge.d.ts") && !summary.loading)
    });
    assert_eq!(summary(cx).notice, Some("too_large"));
    view.update(cx, |view, cx| view.file_view_load_anyway(cx));
    wait(cx, &shell, "nothing", |_| true);
    assert_eq!(summary(cx).notice, Some("too_large"));
}
