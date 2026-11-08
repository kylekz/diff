//! The "Finish round" sheet in a real window over a real fixture repo: `Ctrl Enter` opens it on
//! the verdict the open threads call for, the keys choose and write, and submitting snapshots
//! the round and moves the review's status.

use dv_core::review::{
    Anchor, AnchorSide, Author, ReviewStatus, Thread, ThreadId, ThreadState, Verdict,
};
use dv_core::{Location, Target};
use dv_test_support::FixtureRepo;
use gpui::{Entity, Focusable as _, TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;

use super::*;
use crate::services::reviews::OpenRequest;
use crate::shell::Shell;
use crate::test_support::{add_repo, open_shell, settle};

#[test]
fn the_verdict_follows_the_open_threads_and_the_sheet_fits_the_window() {
    assert_eq!(default_verdict(0), Verdict::Approve);
    assert_eq!(default_verdict(2), Verdict::RequestChanges);
    assert_eq!(verdict_label(Verdict::RequestChanges), "Request changes");
    assert_eq!(sheet_top(gpui::px(900.)), gpui::px(92.));
    assert_eq!(sheet_top(gpui::px(700.)), gpui::px(24.));
    assert_eq!(sheet_top(gpui::px(1400.)), gpui::px(92.));
}

/// A thread opened in `round` (moved, and the move recorded, in `moved_in`), in `state`.
fn thread_in(id: &str, round: u32, moved_in: Option<u32>, state: ThreadState) -> Thread {
    let anchor = |round: u32, line: u32| Anchor {
        round,
        path: "a.txt".into(),
        old_path: None,
        side: AnchorSide::New,
        start_line: line,
        end_line: line,
        text: vec!["one".into()],
        context_before: Vec::new(),
        context_after: Vec::new(),
    };
    let first = anchor(round, 1);
    Thread {
        id: ThreadId::parse(id).expect("a thread id"),
        anchor: moved_in.map_or_else(|| first.clone(), |round| anchor(round, 4)),
        original_anchor: moved_in.map(|_| first),
        author: Author::reviewer("kyle"),
        body: "Why?".into(),
        created_at: 0,
        edited_at: None,
        deleted_at: None,
        state,
        last_change: None,
        annotation: false,
        replies: Vec::new(),
    }
}

#[test]
fn the_comments_are_the_rounds_own_and_the_earlier_unresolved_ones() {
    let threads = [
        // Round 1's, resolved: done with, not handed back.
        thread_in(
            "t-01jbzv8m2e3q9w5x7y0z1a2b01",
            1,
            None,
            ThreadState::Resolved,
        ),
        // Round 1's, moved and resolved in round 2 (the move recorded in round 2): still round
        // 1's, so not listed.
        thread_in(
            "t-01jbzv8m2e3q9w5x7y0z1a2b02",
            1,
            Some(2),
            ThreadState::Resolved,
        ),
        // Round 1's, moved in round 2 and still open: listed, from round 1.
        thread_in(
            "t-01jbzv8m2e3q9w5x7y0z1a2b03",
            1,
            Some(2),
            ThreadState::Open,
        ),
        // Round 2's own, resolved already: listed.
        thread_in(
            "t-01jbzv8m2e3q9w5x7y0z1a2b04",
            2,
            None,
            ThreadState::Resolved,
        ),
        // The agent's annotations, of this round and the last, nobody asked about: nothing
        // goes back to the agent with them (PLAN §19.1).
        thread_in(
            "t-01jbzv8m2e3q9w5x7y0z1a2b05",
            2,
            None,
            ThreadState::Annotation,
        ),
        thread_in(
            "t-01jbzv8m2e3q9w5x7y0z1a2b06",
            1,
            None,
            ThreadState::Annotation,
        ),
    ];
    let listed: Vec<String> = round_comments(&threads, 2)
        .into_iter()
        .map(|thread| thread.id.to_string())
        .collect();
    assert_eq!(
        listed,
        [
            "t-01jbzv8m2e3q9w5x7y0z1a2b03",
            "t-01jbzv8m2e3q9w5x7y0z1a2b04"
        ]
    );
    assert_eq!(opened_in(&threads[2]), 1);
    assert_eq!(threads[2].anchor.round, 2);
}

fn fixture() -> FixtureRepo {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "one\ntwo\n");
    repo.commit("base");
    repo.write("a.txt", "one\n2\n");
    repo.write("new.txt", "fresh\n");
    repo
}

/// A shell showing the working review of `repo`, its two files loaded.
fn open<'a>(
    repo: &FixtureRepo,
    cx: &'a mut TestAppContext,
) -> (tempfile::TempDir, Entity<Shell>, &'a mut VisualTestContext) {
    let (dir, shell, cx) = open_shell(cx);
    add_repo(repo, &shell, cx);
    let task = cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open(
                Location::Local(repo.path().to_path_buf()),
                OpenRequest::Target(Target::Working),
                window,
                cx,
            )
        })
    });
    draw(cx);
    settle(cx, &shell, "the files to load", |shell, cx| {
        shell.session().is_some_and(|session| {
            let session = session.read(cx);
            session.files().len() == 2
                && session.files().iter().all(|entry| {
                    session
                        .file_state(&entry.path)
                        .is_some_and(|s| s.diff().is_some())
                })
        })
    });
    cx.foreground_executor()
        .block_test(task)
        .expect("the review opens");
    (dir, shell, cx)
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

fn press(cx: &mut VisualTestContext, shell: &Entity<Shell>, keys: &str) {
    cx.simulate_keystrokes(keys);
    draw(cx);
    settle(cx, shell, keys, |_, _| true);
    draw(cx);
}

fn session(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Entity<ReviewSession> {
    shell.read_with(cx, |shell, _| shell.session().cloned().expect("a review"))
}

fn sheet(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Option<Entity<FinishSheet>> {
    shell.read_with(cx, |shell, _| shell.sheet().cloned())
}

fn verdict(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Option<Verdict> {
    sheet(shell, cx).map(|sheet| sheet.read_with(cx, |sheet, _| sheet.verdict()))
}

fn add_thread(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> ThreadId {
    let session = session(shell, cx);
    let adding = session.update(cx, |session, cx| {
        let anchor = session
            .anchor_for("a.txt", AnchorSide::New, 2, 2)
            .expect("the lines are loaded");
        session.add_thread(anchor, "Spell it out".to_string(), cx)
    });
    settle(cx, shell, "the thread", |_, _| true);
    cx.foreground_executor()
        .block_test(adding)
        .expect("the thread is added")
}

#[gpui::test]
fn ctrl_enter_opens_the_sheet_on_request_changes_while_a_thread_is_open(cx: &mut TestAppContext) {
    let repo = fixture();
    let (_dir, shell, cx) = open(&repo, cx);
    add_thread(&shell, cx);
    press(cx, &shell, "secondary-enter");
    assert_eq!(verdict(&shell, cx), Some(Verdict::RequestChanges));
    // The verdicts have the keyboard: arrows and digits choose.
    press(cx, &shell, "up");
    assert_eq!(verdict(&shell, cx), Some(Verdict::Approve));
    press(cx, &shell, "down");
    assert_eq!(verdict(&shell, cx), Some(Verdict::RequestChanges));
    press(cx, &shell, "1");
    assert_eq!(verdict(&shell, cx), Some(Verdict::Approve));
    press(cx, &shell, "2");
    assert_eq!(verdict(&shell, cx), Some(Verdict::RequestChanges));
    // `Esc` closes it and gives the keyboard back to the stream.
    press(cx, &shell, "escape");
    assert!(sheet(&shell, cx).is_none());
    let main = shell.update_in(cx, |shell, window, cx| {
        shell.focus_handle(cx).is_focused(window)
    });
    assert!(main);
    let status = session(&shell, cx).read_with(cx, |session, _| session.review().status);
    assert_eq!(status, ReviewStatus::Open);
}

#[gpui::test]
fn submitting_snapshots_the_round_and_requests_changes(cx: &mut TestAppContext) {
    let repo = fixture();
    let (_dir, shell, cx) = open(&repo, cx);
    add_thread(&shell, cx);
    press(cx, &shell, "secondary-enter");
    // `Tab` moves into the summary, where letters type and arrows don't choose.
    press(cx, &shell, "tab");
    cx.simulate_input("Two things to fix.");
    press(cx, &shell, "up");
    assert_eq!(verdict(&shell, cx), Some(Verdict::RequestChanges));
    let summary = sheet(&shell, cx)
        .expect("the sheet")
        .read_with(cx, |sheet, cx| sheet.summary_text(cx));
    assert_eq!(summary, "Two things to fix.");
    press(cx, &shell, "tab secondary-enter");
    settle(cx, &shell, "the round", |shell, cx| {
        shell.sheet().is_none()
            && shell
                .session()
                .is_some_and(|session| session.read(cx).review().rounds.len() == 1)
    });
    let session = session(&shell, cx);
    let (review, viewed) = session.read_with(cx, |session, _| {
        (session.review().clone(), session.viewed_count())
    });
    assert_eq!(review.status, ReviewStatus::ChangesRequested);
    assert_eq!(review.current_round(), 2);
    let round = &review.rounds[0];
    assert_eq!(round.verdict, Some(Verdict::RequestChanges));
    assert_eq!(round.body.as_deref(), Some("Two things to fix."));
    // The snapshot is at the round's ref, and holds the working tree as it stood.
    let snapshot = repo.git(&[
        "rev-parse",
        format!("refs/dv/reviews/{}/1", review.id).as_str(),
    ]);
    assert_eq!(snapshot.trim(), round.new_oid.as_str());
    let tree = repo.git(&["show", format!("{}:new.txt", round.new_oid).as_str()]);
    assert_eq!(tree, "fresh\n");
    // Viewed is per round: the new round starts with nothing viewed.
    assert_eq!(viewed, 0);
    // The stream moves on to what changes since the round: nothing yet (PLAN §2.3).
    let status = shell.read_with(cx, |shell, cx| shell.status_line(cx).review);
    assert_eq!(
        status.as_deref(),
        Some("Changes requested · round 2 · 1 open · since round 1")
    );
    let empty = shell.read_with(cx, |shell, cx| shell.empty_state(cx));
    assert_eq!(empty, Some(crate::shell::EmptyState::NoChangesSince(1)));
}

#[gpui::test]
fn the_sheet_stays_while_its_round_is_finished_and_shows_a_failure(cx: &mut TestAppContext) {
    let repo = fixture();
    let (_dir, shell, cx) = open(&repo, cx);
    // A ref where the rounds' refs go makes the snapshot's `update-ref` fail.
    repo.git(&["update-ref", "refs/dv", "HEAD"]);
    press(cx, &shell, "secondary-enter");
    // `Esc` (and Cancel) while the round is being finished do nothing, so the failure shows.
    cx.simulate_keystrokes("secondary-enter escape");
    draw(cx);
    settle(cx, &shell, "the failed round", |_, _| true);
    let sheet = sheet(&shell, cx).expect("the sheet stays open");
    let error = sheet.read_with(cx, |sheet, _| sheet.error().cloned());
    assert!(
        error
            .as_ref()
            .is_some_and(|error| error.message.contains("snapshot")),
        "{error:?}"
    );
    let status = session(&shell, cx).read_with(cx, |session, _| session.review().status);
    assert_eq!(status, ReviewStatus::Open);
    // Once it has landed, `Esc` closes it.
    press(cx, &shell, "escape");
    assert!(self::sheet(&shell, cx).is_none());
}

#[gpui::test]
fn nothing_more_is_submitted_while_the_round_is_finished(cx: &mut TestAppContext) {
    let repo = fixture();
    let (_dir, shell, cx) = open(&repo, cx);
    add_thread(&shell, cx);
    press(cx, &shell, "secondary-enter");
    // A second `Ctrl Enter` after an `Esc` that does nothing finishes nothing more.
    cx.simulate_keystrokes("secondary-enter escape secondary-enter 1 secondary-enter");
    draw(cx);
    settle(cx, &shell, "the round", |shell, _| shell.sheet().is_none());
    let session = session(&shell, cx);
    session.read_with(cx, |session, _| {
        let review = session.review();
        assert_eq!(review.rounds.len(), 1);
        assert_eq!(review.status, ReviewStatus::ChangesRequested);
        assert!(session.warnings().is_empty(), "{:?}", session.warnings());
    });
}

#[gpui::test]
fn no_other_review_opens_while_a_round_is_finished(cx: &mut TestAppContext) {
    let repo = fixture();
    let (_dir, shell, cx) = open(&repo, cx);
    add_thread(&shell, cx);
    press(cx, &shell, "secondary-enter");
    let location = Location::Local(repo.path().to_path_buf());
    // The round starts and another review is asked for before the write can run.
    let (submitting, opening, refused) = cx.update(|window, cx| {
        window.dispatch_keystroke(
            gpui::Keystroke::parse_with_os("secondary-enter", cx.platform_info().os).unwrap(),
            cx,
        );
        let opening = shell.update(cx, |shell, cx| {
            shell.open(location, OpenRequest::Target(Target::Staged), window, cx)
        });
        let shell = shell.read(cx);
        let submitting = shell
            .sheet()
            .is_some_and(|sheet| sheet.read(cx).is_submitting());
        (submitting, shell.opening().is_some(), opening)
    });
    assert!(submitting, "the round is being finished");
    assert!(!opening, "nothing opens meanwhile");
    let refused = cx.foreground_executor().block_test(refused);
    assert!(refused.is_err_and(|failure| failure.message.contains("being finished")));
    settle(cx, &shell, "the round", |shell, _| shell.sheet().is_none());
    session(&shell, cx).read_with(cx, |session, _| {
        assert_eq!(session.review().target, Target::Working);
        assert_eq!(session.review().status, ReviewStatus::ChangesRequested);
    });
}

#[gpui::test]
fn a_menu_opens_over_the_sheet_and_leaves_its_summary(cx: &mut TestAppContext) {
    let repo = fixture();
    let (_dir, shell, cx) = open(&repo, cx);
    press(cx, &shell, "secondary-enter tab");
    cx.simulate_input("A long summary.");
    draw(cx);
    let summary = |cx: &mut VisualTestContext| {
        sheet(&shell, cx).map(|sheet| sheet.read_with(cx, |sheet, cx| sheet.summary_text(cx)))
    };
    assert_eq!(summary(cx).as_deref(), Some("A long summary."));
    // `Ctrl O` from the summary opens the switcher over the sheet, which keeps its text.
    press(cx, &shell, "secondary-o");
    assert!(shell.read_with(cx, |shell, _| shell.switcher().is_some()));
    assert_eq!(summary(cx).as_deref(), Some("A long summary."));
    // `Esc` closes the switcher alone, and the keyboard is back in the summary.
    press(cx, &shell, "escape");
    assert!(shell.read_with(cx, |shell, _| shell.switcher().is_none()));
    cx.simulate_input(" More.");
    draw(cx);
    assert_eq!(summary(cx).as_deref(), Some("A long summary. More."));
    // The source picker too.
    shell.update_in(cx, |shell, window, cx| shell.open_source_picker(window, cx));
    draw(cx);
    assert!(shell.read_with(cx, |shell, _| shell.source_picker().is_some()));
    assert!(sheet(&shell, cx).is_some());
    press(cx, &shell, "escape");
    assert!(shell.read_with(cx, |shell, _| shell.source_picker().is_none()));
    assert_eq!(summary(cx).as_deref(), Some("A long summary. More."));
    // The sheet's own `Esc` closes it.
    press(cx, &shell, "tab escape");
    assert!(sheet(&shell, cx).is_none());
}

#[gpui::test]
fn approving_finishes_the_review_and_a_new_one_can_start(cx: &mut TestAppContext) {
    let repo = fixture();
    let (_dir, shell, cx) = open(&repo, cx);
    press(cx, &shell, "secondary-enter");
    assert_eq!(verdict(&shell, cx), Some(Verdict::Approve));
    press(cx, &shell, "secondary-enter");
    settle(cx, &shell, "the approval", |shell, cx| {
        shell.sheet().is_none()
            && shell
                .session()
                .is_some_and(|session| session.read(cx).is_read_only())
    });
    let first = session(&shell, cx).read_with(cx, |session, _| session.review_id().clone());
    // The status bar names the round that finished it, not a next one that never opens.
    let status = shell.read_with(cx, |shell, cx| shell.status_line(cx).review);
    assert_eq!(status.as_deref(), Some("Approved · round 1 · no comments"));
    // A finished review has no round to finish.
    press(cx, &shell, "secondary-enter");
    assert!(sheet(&shell, cx).is_none());
    shell.update_in(cx, |shell, window, cx| shell.start_new_review(window, cx));
    settle(cx, &shell, "the new review", |shell, cx| {
        shell
            .session()
            .is_some_and(|session| session.read(cx).review_id() != &first)
    });
    let status = session(&shell, cx).read_with(cx, |session, _| session.review().status);
    assert_eq!(status, ReviewStatus::Open);
}

#[gpui::test]
fn the_sheet_counts_the_reviews_changes_and_names_threads_where_they_show(cx: &mut TestAppContext) {
    let repo = fixture();
    let (_dir, shell, cx) = open(&repo, cx);
    let thread = add_thread(&shell, cx);
    press(cx, &shell, "secondary-enter");
    press(cx, &shell, "secondary-enter");
    settle(cx, &shell, "round 1", |shell, cx| {
        shell.sheet().is_none()
            && shell.session().is_some_and(|session| {
                session.read(cx).comparison() == crate::session::Comparison::SinceRound(1)
            })
    });
    // The agent inserts a line above the thread's: only a.txt changed since the round.
    repo.write("a.txt", "zero\none\n2\n");
    settle(cx, &shell, "the interdiff", |shell, cx| {
        shell.session().is_some_and(|session| {
            let session = session.read(cx);
            session.files().len() == 1 && session.target_files().len() == 2
        })
    });
    let session = session(&shell, cx);
    session.update(cx, |session, cx| {
        session.ensure_loaded(["a.txt".to_string()], cx)
    });
    settle(cx, &shell, "a.txt", |shell, cx| {
        shell.session().is_some_and(|session| {
            let session = session.read(cx);
            session
                .review()
                .thread(&thread)
                .is_some_and(|found| session.placement(found).lines(&found.anchor) == Some((3, 3)))
        })
    });
    session.read_with(cx, |session, _| {
        // The review's two files and their totals, not the interdiff's one.
        assert_eq!(super::render::finished_changes(session), (2, 3, 1));
        // The moved thread is named where it shows now, as the comments list names it.
        let found = session.review().thread(&thread).unwrap();
        assert_eq!(found.anchor.start_line, 2);
        assert_eq!(session.location(found), "a.txt:3");
        let prompt = crate::comments_list::prompt_threads(session);
        assert_eq!(prompt.len(), 1);
        assert_eq!(prompt[0].1, "a.txt:3");
    });
}
