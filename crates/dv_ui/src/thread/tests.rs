use dv_core::review::{
    Anchor, AnchorSide, Author, Comment, CommentId, StateChange, Thread, ThreadId, ThreadState,
};
use pretty_assertions::assert_eq;

use super::*;

fn thread(path: &str, side: AnchorSide, lines: (u32, u32), text: &[&str], body: &str) -> Thread {
    Thread {
        id: ThreadId::parse("t-01jbzv8m2e3q9w5x7y0z1a2b3c").expect("a thread id"),
        anchor: Anchor {
            round: 1,
            path: path.to_string(),
            old_path: None,
            side,
            start_line: lines.0,
            end_line: lines.1,
            text: text.iter().map(|line| line.to_string()).collect(),
            context_before: Vec::new(),
            context_after: Vec::new(),
        },
        original_anchor: None,
        author: Author::reviewer("kyle"),
        body: body.to_string(),
        created_at: 0,
        edited_at: None,
        deleted_at: None,
        state: ThreadState::Open,
        last_change: None,
        annotation: false,
        replies: Vec::new(),
    }
}

fn reply(author: Author, body: &str, deleted: bool) -> Comment {
    Comment {
        id: CommentId::parse("c-01jbzv8m2e3q9w5x7y0z1a2b3d").expect("a comment id"),
        author,
        body: body.to_string(),
        created_at: 0,
        edited_at: None,
        deleted_at: deleted.then_some(1),
    }
}

#[test]
fn anchors_read_as_design_writes_them() {
    assert_eq!(anchor_label(AnchorSide::New, 31, 31), "new · line 31");
    assert_eq!(anchor_label(AnchorSide::Old, 12, 14), "old · lines 12–14");
    let one = thread("src/a.ts", AnchorSide::New, (7, 7), &["x"], "b");
    assert_eq!(location_label(&one), "src/a.ts:7");
    let range = thread("src/a.ts", AnchorSide::Old, (3, 5), &["x"], "b");
    assert_eq!(location_label(&range), "src/a.ts:3–5");
    // A card shows where its thread stands now, and says when it moved (DESIGN §4.1).
    let moved = thread("src/a.ts", AnchorSide::New, (31, 33), &["x", "y", "z"], "b");
    assert_eq!(
        shown_anchor_label(&moved.anchor, Some((34, 36)), true),
        "new · lines 34–36 · moved"
    );
    assert_eq!(
        shown_anchor_label(&moved.anchor, None, false),
        "new · lines 31–33"
    );
    assert_eq!(path_lines("src/a.ts", 9, 9), "src/a.ts:9");
}

#[test]
fn the_composer_is_headed_by_its_lines_or_as_a_reply() {
    let mut target = ComposerTarget {
        path: "a.ts".to_string(),
        side: AnchorSide::New,
        start_line: 12,
        end_line: 14,
        reply_to: None,
    };
    assert_eq!(target.title(), "Comment on new · lines 12–14");
    target.reply_to = Some(ThreadId::parse("t-01jbzv8m2e3q9w5x7y0z1a2b3c").unwrap());
    assert_eq!(target.title(), "Reply");
}

#[test]
fn times_read_relative_to_now() {
    let now = 10_000_000_000;
    assert_eq!(relative_time(now - 5_000, now), "just now");
    assert_eq!(relative_time(now - 5 * 60_000, now), "5m ago");
    assert_eq!(relative_time(now - 3 * 3_600_000, now), "3h ago");
    assert_eq!(relative_time(now - 2 * 86_400_000, now), "2d ago");
    assert_eq!(relative_time(now - 21 * 86_400_000, now), "3w ago");
    // A clock that went backwards reads as now, never a negative time.
    assert_eq!(relative_time(now + 60_000, now), "just now");
}

#[test]
fn names_and_bodies_shorten_for_one_line() {
    assert_eq!(initial("kyle"), "k");
    assert_eq!(initial("Kyle"), "k");
    assert_eq!(initial("  _dv fixture"), "d");
    assert_eq!(initial("—"), "?");
    assert_eq!(first_line("\n\n  First line.  \nSecond"), "First line.");
    assert_eq!(first_line(""), "");
    // Rows show the text the card renders, not its Markdown.
    assert_eq!(
        first_line("add a sweep or **cap** the size.\nmore"),
        "add a sweep or cap the size."
    );
}

#[test]
fn the_state_chip_names_who_addressed_a_thread() {
    let mut subject = thread("a.ts", AnchorSide::New, (1, 1), &["x"], "b");
    assert_eq!(state_label(&subject), "Open");
    subject.state = ThreadState::Addressed;
    subject.last_change = Some(StateChange {
        state: ThreadState::Addressed,
        note: Some("Capped at 1,000.".to_string()),
        author: Author::agent("claude-code"),
        at: 0,
    });
    assert_eq!(state_label(&subject), "Addressed by claude-code");
    assert_eq!(state_note(&subject), Some("Capped at 1,000."));
    // An agent that named its model shows by it.
    subject.last_change.as_mut().unwrap().author =
        Author::agent("claude-code").with_model("Claude Fable 5.2");
    assert_eq!(state_label(&subject), "Addressed by Claude Fable 5.2");
    subject.state = ThreadState::Resolved;
    assert_eq!(state_label(&subject), "Resolved");
}

#[test]
fn a_comments_tag_names_the_agent_behind_a_model() {
    // Shown by its model, an agent is tagged with its own name; anyone else with their role.
    let fable = Author::agent("claude-code").with_model("Claude Fable 5.2");
    assert_eq!(crate::agents::shown_name(&fable), "Claude Fable 5.2");
    assert_eq!(role_tag(&fable), "claude-code");
    assert_eq!(role_tag(&Author::agent("codex")), "agent");
    assert_eq!(role_tag(&Author::github("octocat")), "github");
    assert_eq!(role_tag(&Author::reviewer("Kyle")), "reviewer");
}

#[test]
fn copy_as_prompt_carries_where_the_code_the_comment_and_the_replies() {
    let mut first = thread(
        "src/lib/http.ts",
        AnchorSide::New,
        (14, 15),
        &["  for (;;) {", "    retry();"],
        "Retry loop swallows the last error.\n",
    );
    first.replies = vec![
        reply(
            Author::agent("claude-code"),
            "Rethrown after the last try.\n\nSee the test.",
            false,
        ),
        reply(Author::reviewer("kyle"), "gone", true),
    ];
    let second = thread(
        "README",
        AnchorSide::Old,
        (2, 2),
        &["Use ``` fences"],
        "Keep this line.",
    );
    // Each under where it shows now: the first moved down two lines since it was written.
    let text = prompt_markdown(
        OPEN_PROMPT_HEADING,
        "working tree vs main",
        &[
            (&first, "src/lib/http.ts:16–17".to_string()),
            // An old-side thread, named as `ReviewSession::location` names it.
            (&second, format!("{} (old)", location_label(&second))),
        ],
    );
    assert_eq!(
        text,
        "Open review comments on working tree vs main (2 comments):\n\
         \n## src/lib/http.ts:16–17\n\nThread `t-01jbzv8m2e3q9w5x7y0z1a2b3c`.\n\n\
         ```ts\n  for (;;) {\n    retry();\n```\n\n\
         Retry loop swallows the last error.\n\
         \n> **claude-code** (agent):\n> Rethrown after the last try.\n>\n> See the test.\n\
         \n## README:2 (old)\n\nThread `t-01jbzv8m2e3q9w5x7y0z1a2b3c`.\n\n\
         ````\nUse ``` fences\n````\n\n\
         Keep this line.\n"
    );
}

#[test]
fn a_comment_on_a_whole_file_reads_as_the_file() {
    assert_eq!(anchor_label(AnchorSide::New, 0, 0), "file");
    assert_eq!(anchor_label(AnchorSide::Old, 0, 0), "old · file");
    let target = ComposerTarget::file("src/a.ts".to_string(), AnchorSide::New);
    assert!(target.is_file());
    assert_eq!(target.title(), "Comment on file");
    let deleted = ComposerTarget::file("src/gone.ts".to_string(), AnchorSide::Old);
    assert_eq!(deleted.title(), "Comment on file");
    let file = thread("src/a.ts", AnchorSide::New, (0, 0), &[], "Split this file");
    assert!(file.anchor.is_file_level());
    assert_eq!(location_label(&file), "src/a.ts (file)");
    assert_eq!(shown_anchor_label(&file.anchor, None, false), "file");
    let old = thread("src/gone.ts", AnchorSide::Old, (0, 0), &[], "Why?");
    assert_eq!(location_label(&old), "src/gone.ts (file, old)");
    // Copy as prompt says so, with no code.
    let text = prompt_markdown(
        OPEN_PROMPT_HEADING,
        "working tree vs main",
        &[(&file, location_label(&file))],
    );
    assert_eq!(
        text,
        "Open review comments on working tree vs main (1 comment):\n\
         \n## src/a.ts (file)\n\nThread `t-01jbzv8m2e3q9w5x7y0z1a2b3c`.\n\n\
         On the whole file.\n\n\
         Split this file\n"
    );
}
