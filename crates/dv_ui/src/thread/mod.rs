//! Threads in the stream (DESIGN §4.1 "Thread card", PLAN §2.5): the inline cards, the composer
//! that opens a thread or replies to one, and the words both use.
//!
//! A card shows a thread's state and anchor, its opening comment and replies (Markdown through
//! [`crate::markdown`], sanitised), the reviewer's actions (Resolve, Reopen) and a reply field.
//! `open` threads wear the accent ring dot, `addressed` ones the agent's blue note with Resolve
//! and Reopen, `resolved` ones a green check and collapse to one line until opened, and an
//! outdated one (relocation can't find its anchored text, PLAN §8) a yellow chip and its lines
//! as they were. A thread that moved shows its new lines, "· moved" after them. On a finished
//! review the actions and the reply field stay, disabled.
//!
//! The composer is one multi-line field: on a new thread it sits under the selected lines with
//! "Comment on new · lines 12–14" (under its file's header with "Comment on file" for a comment
//! on the whole file, PLAN §8); as a reply it takes the place of the card's reply field.
//! `Ctrl Enter` posts, `Esc` cancels (`crate::actions::composer`).
//!
//! The views here hold no state of their own: the stream ([`crate::diff_pane`]) keeps which
//! thread is focused, which resolved ones are open and the composer, and hands each card its
//! handlers, as [`crate::banner::Banner`] does.
//!
//! Automation targets: `thread/<id>` with `/resolve`, `/reopen`, `/toggle`, `/reply` and
//! `/view_file` (a card away from the unchanged file it stands in);
//! `composer` with `/input`, `/cancel` and `/post`.

mod render;

#[cfg(test)]
mod tests;

use std::fmt::Write as _;

use dv_core::review::{Anchor, AnchorSide, Author, AuthorKind, Thread, ThreadId, ThreadState};
use gpui::{Entity, Subscription};
use gpui_component::input::TextareaState;

use crate::failure::Failure;
pub use render::{ComposerBox, ThreadCard};

/// Rows the composer's field starts with and grows to.
pub const COMPOSER_ROWS: (usize, usize) = (3, 12);
/// Rows a reply's field starts with and grows to.
pub const REPLY_ROWS: (usize, usize) = (2, 10);

/// What the composer writes: a new thread on lines of one side of one file, or on the whole
/// file (lines `0..=0`, [`ComposerTarget::file`]), or a reply to `reply_to` (whose anchor the
/// other fields repeat).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposerTarget {
    pub path: String,
    pub side: AnchorSide,
    pub start_line: u32,
    pub end_line: u32,
    pub reply_to: Option<ThreadId>,
}

impl ComposerTarget {
    /// A new comment on the whole file at `path`, on `side` of it (the old side for a deleted
    /// file).
    pub fn file(path: String, side: AnchorSide) -> Self {
        Self {
            path,
            side,
            start_line: 0,
            end_line: 0,
            reply_to: None,
        }
    }

    /// Whether it writes a new comment on the whole file.
    pub fn is_file(&self) -> bool {
        self.reply_to.is_none() && self.start_line == 0 && self.end_line == 0
    }

    /// The composer's heading: "Comment on new · lines 12–14", "Comment on file" (whichever
    /// side: a deleted file's old one too, DESIGN §4.1), or "Reply".
    pub fn title(&self) -> String {
        match self.reply_to {
            Some(_) => "Reply".to_string(),
            None if self.is_file() => "Comment on file".to_string(),
            None => format!(
                "Comment on {}",
                anchor_label(self.side, self.start_line, self.end_line)
            ),
        }
    }
}

/// The open composer: where it writes, its field, and how its last post went.
pub struct Composer {
    pub target: ComposerTarget,
    pub input: Entity<TextareaState>,
    /// A post is on its way to the store.
    pub posting: bool,
    /// Why the last post failed.
    pub error: Option<Failure>,
    pub _subscription: Subscription,
}

/// `old` or `new`, as anchors and the composer name a side.
pub fn side_word(side: AnchorSide) -> &'static str {
    match side {
        AnchorSide::Old => "old",
        AnchorSide::New => "new",
    }
}

/// "new · line 31", "old · lines 12–14" (DESIGN §4.1); "file" for a comment on the whole file
/// (lines 0), "old · file" on its old side.
pub fn anchor_label(side: AnchorSide, start: u32, end: u32) -> String {
    if (start, end) == (0, 0) {
        return match side {
            AnchorSide::New => "file".to_string(),
            AnchorSide::Old => "old · file".to_string(),
        };
    }
    let side = side_word(side);
    if start == end {
        format!("{side} · line {start}")
    } else {
        format!("{side} · lines {start}–{end}")
    }
}

/// A card's anchor as it shows now: `anchor`'s side at `lines` (else the anchor's own lines),
/// with "· moved" when those are lines it moved to (DESIGN §4.1: "new · lines 34–36 · moved").
pub fn shown_anchor_label(anchor: &Anchor, lines: Option<(u32, u32)>, moved: bool) -> String {
    let (start, end) = lines.unwrap_or((anchor.start_line, anchor.end_line));
    let label = anchor_label(anchor.side, start, end);
    if moved {
        format!("{label} · moved")
    } else {
        label
    }
}

/// "code changed since round 1": the anchor of an outdated card drawn where its code went
/// since that round (PLAN §8, DESIGN §4.1).
pub fn trail_label(round: u32) -> String {
    format!("code changed since round {round}")
}

/// `path:31` or `path:12–14`: where a thread sits, as the comments list and prompts name it;
/// `path (file)` for a comment on the whole file ([`file_location`]).
pub fn location_label(thread: &Thread) -> String {
    let anchor = &thread.anchor;
    if anchor.is_file_level() {
        return file_location(&anchor.path, anchor.side);
    }
    path_lines(&anchor.path, anchor.start_line, anchor.end_line)
}

/// `path (file)`, `path (file, old)` on the old side: a comment on the whole file, as the
/// comments list, prompts and the sheet name it, and as the CLI prints it (PLAN §10.3).
pub fn file_location(path: &str, side: AnchorSide) -> String {
    match side {
        AnchorSide::New => format!("{path} (file)"),
        AnchorSide::Old => format!("{path} (file, old)"),
    }
}

/// `path:31` or `path:12–14`.
pub fn path_lines(path: &str, start: u32, end: u32) -> String {
    if start == end {
        format!("{path}:{start}")
    } else {
        format!("{path}:{start}–{end}")
    }
}

/// How long ago `at` was, from `now` (both ms since the epoch): "just now", "5m ago", "3h ago",
/// "2d ago", "3w ago".
pub fn relative_time(at: u64, now: u64) -> String {
    let seconds = now.saturating_sub(at) / 1000;
    match seconds {
        0..60 => "just now".to_string(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        86_400..604_800 => format!("{}d ago", seconds / 86_400),
        _ => format!("{}w ago", seconds / 604_800),
    }
}

/// The letter a round avatar shows: the name's first letter or digit, lowercase as the board
/// draws it; `?` for a name without one.
pub fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map_or_else(|| "?".to_string(), |c| c.to_lowercase().collect())
}

/// The word beside an author's name: `reviewer`, `agent` or `github`.
pub fn role_word(author: &Author) -> &'static str {
    author.kind.as_str()
}

/// The tag beside an author's name on a comment: an agent shown by its model
/// ([`crate::agents::shown_name`]) is tagged with its own name (`claude-code`), anyone else
/// with their role.
pub fn role_tag(author: &Author) -> &str {
    match author.model {
        Some(_) => &author.name,
        None => role_word(author),
    }
}

/// What one-line rows show of a Markdown body: its first line as plain text
/// ([`crate::markdown::first_line_plain`]), so `**cap**` reads as `cap` there as it does,
/// bold, on the card.
pub fn first_line(body: &str) -> String {
    crate::markdown::first_line_plain(body)
}

/// A thread's state chip text: "Open", "Addressed by Claude Fable 5.2" (the agent as it
/// shows, [`crate::agents::shown_name`]), "Resolved".
pub fn state_label(thread: &Thread) -> String {
    match thread.state {
        ThreadState::Open => "Open".to_string(),
        ThreadState::Addressed => match &thread.last_change {
            Some(change) if change.author.kind == AuthorKind::Agent => {
                format!("Addressed by {}", crate::agents::shown_name(&change.author))
            }
            Some(change) if change.author.kind == AuthorKind::Reviewer => "Addressed".to_string(),
            _ => "Addressed by agent".to_string(),
        },
        ThreadState::Resolved => "Resolved".to_string(),
        ThreadState::Annotation => "Annotation".to_string(),
    }
}

/// The note the last state change left (the agent's "what I did"), if any.
pub fn state_note(thread: &Thread) -> Option<&str> {
    thread
        .last_change
        .as_ref()
        .and_then(|change| change.note.as_deref())
        .filter(|note| !note.trim().is_empty())
}

/// The Markdown "Copy as prompt" puts on the clipboard (DESIGN §4.1 panel): the open threads,
/// each with where it shows now (`path:line`, its location beside it in `threads`, as the
/// comments list names it), the anchored code, the comment and its replies, so an agent can be
/// handed the review in one paste. `heading` says what they are ("Open review comments",
/// [`prompt_heading`] for one thread of any state) and `label` names the target ("working tree
/// vs main").
pub fn prompt_markdown(heading: &str, label: &str, threads: &[(&Thread, String)]) -> String {
    let mut out = String::new();
    let count = threads.len();
    let _ = writeln!(
        out,
        "{heading} on {label} ({count} {}):",
        if count == 1 { "comment" } else { "comments" }
    );
    for (thread, location) in threads {
        let anchor = &thread.anchor;
        // `location` says "(old)" for an old-side thread (`ReviewSession::location`).
        let _ = write!(out, "\n## {location}\n\nThread `{}`.\n\n", thread.id);
        if anchor.is_file_level() {
            let _ = writeln!(out, "On the whole file.\n");
        }
        if !anchor.text.is_empty() {
            let code = anchor.text.join("\n");
            let fence = fence_for(&code);
            let _ = writeln!(
                out,
                "{fence}{}\n{code}\n{fence}\n",
                fence_language(&anchor.path)
            );
        }
        let _ = writeln!(out, "{}", thread.body.trim());
        for reply in thread.replies.iter().filter(|reply| !reply.is_deleted()) {
            let _ = writeln!(
                out,
                "\n> **{}** ({}):",
                reply.author.name,
                role_word(&reply.author)
            );
            for line in reply.body.trim().lines() {
                if line.is_empty() {
                    out.push_str(">\n");
                } else {
                    let _ = writeln!(out, "> {line}");
                }
            }
        }
    }
    out
}

/// The heading "Copy as prompt" gives the threads it copies: "Open review comments" for the
/// open ones (the panel's button).
pub const OPEN_PROMPT_HEADING: &str = "Open review comments";

/// The heading for one thread copied alone (a comments row's menu), worded for its state:
/// "Open review comment", "Addressed review comment", "Resolved review comment".
pub fn prompt_heading(state: ThreadState) -> &'static str {
    match state {
        ThreadState::Open => "Open review comment",
        ThreadState::Addressed => "Addressed review comment",
        ThreadState::Resolved => "Resolved review comment",
        ThreadState::Annotation => "Agent's annotation",
    }
}

/// A code fence longer than any run of backticks in `code`, so the code can't close it.
fn fence_for(code: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in code.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// The fence's info string: the file's extension (`ts`, `rs`), or nothing.
fn fence_language(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty() && extension.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            extension
        }
        _ => "",
    }
}
