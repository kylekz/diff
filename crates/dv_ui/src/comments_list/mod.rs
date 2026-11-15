//! The panel's comments (DESIGN §4.1 "Panel", PLAN §11 `comments_list`), under the file list:
//! "Open comments" with the number of threads still waiting (open, and addressed by the agent
//! but not yet resolved), one row per thread — its state, `path:line` where it shows now and
//! the first line of its comment — and "Copy as prompt", which puts the open threads on the
//! clipboard as Markdown (through the clipboard seam, `crate::system`). Addressed threads come
//! first, then open ones, each in stream order (PLAN §3 workflow A step 5: the stream's "Not in
//! the diff" section first, in the order its threads were added, then by file). An outdated thread
//! (PLAN §8) is flagged and shows its first anchored line as it was; a comment on the whole file
//! names its file, `path (file)`, instead of `path:line`; one the interdiff doesn't
//! show carries a quiet "all changes" tag and names where it stands in all changes. A thread
//! whose file's text hasn't landed yet shows the line it was written on, quietly, until it is
//! placed (every file a thread shows in loads at once, so that is brief), and "Copy as prompt"
//! says so beside it. The agent's annotations (PLAN §19), waiting on no one, are counted
//! after the waiting threads and listed on demand, as the resolved threads are at the end.
//!
//! A click on an annotation's row shows the annotations again when `A` hid them, then shows
//! it as a thread's row does; one the stream never draws (outdated, or its file left the
//! review) opens its whole text under the row instead. Its menu holds Ask alone (none for one
//! the stream doesn't draw).
//!
//! A click on a row shows the thread in the stream ([`DiffPane::reveal_thread`]), which also
//! focuses it, switching to all changes first for an "all changes" row; the focused thread's
//! row is highlighted. A right click opens the row's menu (DESIGN §4.1): the card's actions for
//! the thread's state (Resolve; Resolve and Reopen; Reopen), Reply (the thread shown, its reply
//! composer open) and Copy as prompt (that thread alone, as "Copy as prompt" words it); on a
//! finished review the writes are disabled, as on the card. A click outside it, or on an item,
//! closes it.
//!
//! **On GitHub** (DESIGN §4.5, PLAN §9.6): a PR review lists GitHub's threads on its pull
//! request in a group of their own after the review's: the ones GitHub doesn't call resolved,
//! in stream order, each with GitHub's mark, where it stands, its author's login and its
//! comment's first line ("outdated" for one without a place, "pending" for the viewer's own
//! unsubmitted one), then "n resolved on GitHub · show". A click shows the thread's card; its
//! menu holds Resolve or Unresolve (where GitHub allows the viewer), Open on GitHub and Copy
//! as prompt. "Copy as prompt" hands an agent GitHub's unresolved threads after the
//! review's open ones.
//!
//! Automation targets: `comments_list`, `comments_list/copy`, `comments_list/resolved` (show or
//! hide the resolved threads), `comments_list/annotations` (the annotations),
//! `comments_list/github_resolved` (GitHub's resolved threads), `comments_list/<thread id>`
//! (`gh:<node id>` for one of GitHub's), and the row menu's `comments_list/menu` with
//! `/resolve`, `/unresolve`, `/reopen`, `/reply`, `/ask`, `/open` and `/copy`.

mod render;

#[cfg(test)]
mod tests;

use dv_core::github::Standing;
use dv_core::review::{Thread, ThreadId, ThreadKey, ThreadState};
use gpui::{Context, Entity, Pixels, Point, Subscription, Window};

use crate::diff_pane::DiffPane;
use crate::session::{Card, Comparison, GithubShown, ReviewSession, ThreadPlacement};
use crate::thread::{
    GithubPrompt, OPEN_PROMPT_HEADING, first_line, one_line, prompt_heading, prompt_markdown,
    prompt_markdown_with,
};

/// One thread as the list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentRow {
    pub id: ThreadId,
    pub state: ThreadState,
    /// Relocation can't find its anchored text, or its file left the diff (PLAN §8).
    pub outdated: bool,
    /// The interdiff on screen doesn't show it: an old-side thread, or its file isn't in it
    /// (the "all changes" tag).
    pub not_in_comparison: bool,
    /// Its file's text hasn't landed yet: `location` is where it was written, not where it is.
    pub pending: bool,
    /// `path:31`, `path:12–14` (`path:31 (old)` on the old side): where it stands now, else
    /// where it was anchored; `path (file)` for a comment on the whole file.
    pub location: String,
    /// A comment on the whole file (PLAN §8): `location` names the file, not lines.
    pub file_level: bool,
    /// The comment's first line.
    pub summary: String,
    /// The first anchored line as it was, for an outdated thread.
    pub snippet: Option<String>,
    /// It stands in a file the review doesn't change (PLAN §8).
    pub unlisted: bool,
    /// The stream can draw its card (shown or not): every thread but an annotation whose code
    /// is gone or whose file left the review (PLAN §19.3).
    pub drawable: bool,
}

/// One of GitHub's threads as the list shows it, in its "On GitHub" group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubRow {
    /// GitHub's node id.
    pub id: String,
    pub resolved: bool,
    /// It has no line in the diff shown: GitHub calls it outdated and it was made on another
    /// commit, dv can't place it, or relocation can't find its code (PLAN §9.6).
    pub outdated: bool,
    /// dv can't tell which code it is on (`Standing::Unplaced`): its tag says "can't be
    /// placed" where another without a line says "outdated".
    pub unplaced: bool,
    /// The viewer's own comment, not submitted yet.
    pub pending_on_github: bool,
    /// Its file's text hasn't landed yet: `location` is where GitHub has it.
    pub pending: bool,
    /// The interdiff on screen doesn't show it.
    pub not_in_comparison: bool,
    /// Where it stands now, as a review's thread's row names it; `path (file)` for one on
    /// the whole file.
    pub location: String,
    pub file_level: bool,
    /// Who opened it.
    pub author: String,
    /// The first comment's first line.
    pub summary: String,
}

impl GithubRow {
    pub fn key(&self) -> ThreadKey {
        ThreadKey::Github(self.id.clone())
    }
}

/// GitHub's threads on `session`'s pull request as the list shows them: those GitHub doesn't
/// call resolved, in stream order (the "Not in the diff" section's first, then by file and
/// line), then, with `resolved`, the resolved ones.
pub fn github_rows(session: &ReviewSession, resolved: bool) -> Vec<GithubRow> {
    let mut threads: Vec<(&GithubShown, ThreadPlacement)> = session
        .github_threads()
        .iter()
        .filter(|shown| resolved || !shown.thread.resolved)
        .map(|shown| (shown, session.card_placement(Card::Github(shown))))
        .collect();
    threads.sort_by_key(|(shown, placement)| {
        let group = u8::from(shown.thread.resolved);
        if placement.is_not_in_diff() {
            return (group, 0, String::new(), 0);
        }
        let file = placement
            .file()
            .and_then(|path| session.files().iter().position(|e| e.path == path))
            .map_or(usize::MAX, |index| index + 1);
        let line = placement
            .named_lines(&shown.anchor)
            .map_or(shown.anchor.start_line, |(start, _)| start);
        (group, file, shown.anchor.path.clone(), line)
    });
    threads
        .into_iter()
        .map(|(shown, placement)| {
            let first = shown.thread.comments.first();
            GithubRow {
                id: shown.thread.id.clone(),
                resolved: shown.thread.resolved,
                outdated: placement.is_outdated(),
                unplaced: shown.standing == Standing::Unplaced,
                pending_on_github: shown.thread.is_pending(),
                pending: placement.is_pending(),
                not_in_comparison: placement.is_not_in_comparison(),
                location: session.card_location(Card::Github(shown)),
                file_level: shown.thread.is_file_level(),
                author: first
                    .map(|first| one_line(&first.author).into_owned())
                    .unwrap_or_default(),
                summary: first_line(Card::Github(shown).body()),
            }
        })
        .collect()
}

/// GitHub's threads "Copy as prompt" hands an agent after the review's open ones: those
/// GitHub doesn't call resolved, in stream order, each with where it stands now.
pub fn github_prompt_threads(session: &ReviewSession) -> Vec<GithubPrompt<'_>> {
    github_rows(session, false)
        .into_iter()
        .filter_map(|row| {
            let location = if row.pending {
                format!(
                    "{} (where GitHub has it; dv hasn't placed it yet)",
                    row.location
                )
            } else {
                row.location
            };
            Some(GithubPrompt {
                shown: session.github_thread(&row.id)?,
                location,
                url: session.github_url(&row.id),
            })
        })
        .collect()
}

/// Where a state's threads go in the list: addressed first, then open, then the annotations,
/// then resolved.
fn group(state: ThreadState) -> u8 {
    match state {
        ThreadState::Addressed => 0,
        ThreadState::Open => 1,
        ThreadState::Annotation => 2,
        ThreadState::Resolved => 3,
    }
}

/// The threads of `session`'s review: the addressed ones, then the open ones, each in stream
/// order (the "Not in the diff" section's first, in the order they were added; then by file,
/// then the line they show at; the ones the stream doesn't show last), then, with
/// `annotations`, the agent's annotations, then, with `resolved`, the resolved ones.
pub fn rows(session: &ReviewSession, resolved: bool, annotations: bool) -> Vec<CommentRow> {
    let mut threads: Vec<(&Thread, ThreadPlacement)> = session
        .review()
        .live_threads()
        .filter(|thread| match thread.state {
            ThreadState::Resolved => resolved,
            ThreadState::Annotation => annotations,
            ThreadState::Open | ThreadState::Addressed => true,
        })
        .map(|thread| (thread, session.placement(thread)))
        .collect();
    let order = |thread: &Thread, placement: &ThreadPlacement| {
        if placement.is_not_in_diff() {
            // The section heads the stream; its threads keep the order they were added (the
            // sort is stable).
            return (group(thread.state), 0, String::new(), 0);
        }
        let file = placement
            .file()
            .and_then(|path| session.files().iter().position(|e| e.path == path))
            .map_or(usize::MAX, |index| index + 1);
        let line = placement
            .named_lines(&thread.anchor)
            .map_or(thread.anchor.start_line, |(start, _)| start);
        (group(thread.state), file, thread.anchor.path.clone(), line)
    };
    threads.sort_by_key(|(thread, placement)| order(thread, placement));
    threads
        .into_iter()
        .map(|(thread, placement)| {
            let outdated = placement.is_outdated();
            CommentRow {
                id: thread.id.clone(),
                state: thread.state,
                outdated,
                not_in_comparison: placement.is_not_in_comparison(),
                pending: placement.is_pending(),
                location: session.location(thread),
                file_level: thread.anchor.is_file_level(),
                summary: first_line(&thread.body),
                snippet: outdated
                    .then(|| thread.anchor.text.first().cloned())
                    .flatten()
                    .map(|line| line.trim().to_string()),
                unlisted: placement.is_unlisted(),
                drawable: thread.state != ThreadState::Annotation
                    || !(outdated || placement.is_not_in_diff()),
            }
        })
        .collect()
}

/// The threads "Copy as prompt" hands an agent: the open ones, in stream order, each with
/// where it stands now (`ReviewSession::location`, as its row names it). One not placed yet
/// says that its lines are the ones it was written on, so an agent doesn't take them for
/// where the code is now.
pub fn prompt_threads(session: &ReviewSession) -> Vec<(&Thread, String)> {
    rows(session, false, false)
        .into_iter()
        .filter(|row| row.state == ThreadState::Open)
        .filter_map(|row| Some((session.review().thread(&row.id)?, prompt_location(&row))))
        .collect()
}

/// Where "Copy as prompt" says `row`'s thread is: its row's location, and for one not placed
/// yet, that those are the lines it was written on.
pub fn prompt_location(row: &CommentRow) -> String {
    if row.pending {
        format!(
            "{} (where it was written; dv hasn't placed it yet)",
            row.location
        )
    } else {
        row.location.clone()
    }
}

/// A row's right-click menu: whose thread, and where it was opened.
#[derive(Clone, Debug)]
struct RowMenu {
    thread: ThreadKey,
    position: Point<Pixels>,
}

/// The panel's comments (see the module docs).
pub struct CommentsList {
    session: Option<Entity<ReviewSession>>,
    diff_pane: Entity<DiffPane>,
    show_resolved: bool,
    /// GitHub's resolved threads are listed too.
    show_github_resolved: bool,
    /// The agent's annotations are listed (PLAN §19.3).
    show_annotations: bool,
    /// The annotation whose whole text opens under its row: one the stream doesn't draw.
    expanded: Option<ThreadId>,
    menu: Option<RowMenu>,
    _session_events: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl CommentsList {
    pub fn new(diff_pane: Entity<DiffPane>, cx: &mut Context<Self>) -> Self {
        // The focused thread's row follows the stream.
        let subscriptions = vec![cx.observe(&diff_pane, |_, _, cx| cx.notify())];
        Self {
            session: None,
            diff_pane,
            show_resolved: false,
            show_github_resolved: false,
            show_annotations: false,
            expanded: None,
            menu: None,
            _session_events: None,
            _subscriptions: subscriptions,
        }
    }

    /// Lists `session`'s threads (or nothing).
    pub fn set_session(&mut self, session: Option<Entity<ReviewSession>>, cx: &mut Context<Self>) {
        self._session_events = session.as_ref().map(|session| {
            cx.observe(session, |this, _, cx| {
                this.close_stale_menu(cx);
                cx.notify();
            })
        });
        self.session = session;
        self.show_resolved = false;
        self.show_github_resolved = false;
        self.show_annotations = false;
        self.expanded = None;
        self.menu = None;
        cx.notify();
    }

    /// The rows shown now.
    pub fn rows(&self, cx: &gpui::App) -> Vec<CommentRow> {
        self.session
            .as_ref()
            .map(|session| rows(session.read(cx), self.show_resolved, self.show_annotations))
            .unwrap_or_default()
    }

    /// GitHub's threads shown now: the unresolved ones and, once asked for, the resolved.
    pub fn github_rows(&self, cx: &gpui::App) -> Vec<GithubRow> {
        self.session
            .as_ref()
            .map(|session| github_rows(session.read(cx), self.show_github_resolved))
            .unwrap_or_default()
    }

    /// Whether GitHub's resolved threads are listed too.
    pub fn shows_github_resolved(&self) -> bool {
        self.show_github_resolved
    }

    pub fn set_show_github_resolved(&mut self, show: bool, cx: &mut Context<Self>) {
        if self.show_github_resolved != show {
            self.show_github_resolved = show;
            self.close_stale_menu(cx);
            cx.notify();
        }
    }

    /// Whether the agent's annotations are listed.
    pub fn shows_annotations(&self) -> bool {
        self.show_annotations
    }

    pub fn set_show_annotations(&mut self, show: bool, cx: &mut Context<Self>) {
        if self.show_annotations != show {
            self.show_annotations = show;
            if !show {
                self.expanded = None;
            }
            self.close_stale_menu(cx);
            cx.notify();
        }
    }

    /// The annotation whose whole text shows under its row.
    pub fn expanded(&self) -> Option<&ThreadId> {
        self.expanded.as_ref()
    }

    /// Whether the resolved threads are listed too.
    pub fn shows_resolved(&self) -> bool {
        self.show_resolved
    }

    pub fn set_show_resolved(&mut self, show: bool, cx: &mut Context<Self>) {
        if self.show_resolved != show {
            self.show_resolved = show;
            self.close_stale_menu(cx);
            cx.notify();
        }
    }

    /// Closes the row menu once its row is no longer listed (its thread resolved while the
    /// resolved ones are hidden, or deleted).
    fn close_stale_menu(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = &self.menu else {
            return;
        };
        let listed = match &menu.thread {
            ThreadKey::Local(id) => self.rows(cx).iter().any(|row| row.id == *id),
            ThreadKey::Github(node) => self.github_rows(cx).iter().any(|row| row.id == *node),
        };
        if !listed {
            self.close_menu(cx);
        }
    }

    /// Shows GitHub's thread `node` in the stream (a click on its row), in all changes when
    /// the interdiff shown doesn't draw it.
    pub fn reveal_github(&mut self, node: &str, cx: &mut Context<Self>) {
        self.close_menu(cx);
        let key = ThreadKey::Github(node.to_string());
        self.show_all_changes_for(&key, cx);
        self.diff_pane
            .update(cx, |pane, cx| pane.reveal_thread(&key, cx));
    }

    /// Resolve or Unresolve from a GitHub row's menu, as its card's button does.
    pub fn toggle_github_resolved(&mut self, node: &str, cx: &mut Context<Self>) {
        self.close_menu(cx);
        self.diff_pane
            .update(cx, |pane, cx| pane.toggle_github_resolved(node, cx));
        cx.notify();
    }

    /// Open on GitHub from a GitHub row's menu: the thread's page when its link is one of
    /// the pull request's host (`ReviewSession::github_url`), through the system seam.
    pub fn open_github(&mut self, node: &str, cx: &mut Context<Self>) {
        self.close_menu(cx);
        let url = self
            .session
            .as_ref()
            .and_then(|session| session.read(cx).github_url(node).map(str::to_string));
        if let Some(url) = url {
            crate::system::open_url(&url, cx);
        }
    }

    /// Copy as prompt from a GitHub row's menu: that thread alone.
    pub fn copy_github_thread(&mut self, node: &str, cx: &mut Context<Self>) {
        self.close_menu(cx);
        let Some(session) = &self.session else {
            return;
        };
        let text = {
            let session = session.read(cx);
            let Some(shown) = session.github_thread(node) else {
                return;
            };
            prompt_markdown_with(
                OPEN_PROMPT_HEADING,
                &session.prompt_label(),
                &[],
                &[GithubPrompt {
                    shown,
                    location: session.card_location(Card::Github(shown)),
                    url: session.github_url(node),
                }],
            )
        };
        crate::system::write_clipboard(text, cx);
        cx.notify();
    }

    /// Shows `thread` in the stream (a click on its row): one the interdiff doesn't show is
    /// shown in all changes, which the stream switches to first. An annotation shows the
    /// annotations again when they are hidden; one the stream never draws opens (or closes)
    /// its whole text under its row instead (module docs).
    pub fn reveal(&mut self, thread: &ThreadId, cx: &mut Context<Self>) {
        self.close_menu(cx);
        match self.annotation_drawable(thread, cx) {
            Some(false) => {
                self.expanded = match self.expanded.take() {
                    Some(open) if &open == thread => None,
                    _ => Some(thread.clone()),
                };
                cx.notify();
                return;
            }
            Some(true) => self.show_annotations_in_stream(cx),
            None => {}
        }
        self.show_all_changes_for(&thread.into(), cx);
        self.diff_pane
            .update(cx, |pane, cx| pane.reveal_thread(thread, cx));
    }

    /// For an annotation, whether the stream can draw its card (`None`: not an annotation).
    fn annotation_drawable(&self, thread: &ThreadId, cx: &gpui::App) -> Option<bool> {
        let session = self.session.as_ref()?.read(cx);
        let found = session
            .review()
            .thread(thread)
            .filter(|found| found.state == ThreadState::Annotation)?;
        let placement = session.placement(found);
        Some(!(placement.is_outdated() || placement.is_not_in_diff()))
    }

    /// Shows the annotations in the stream again, when `A` hid them.
    fn show_annotations_in_stream(&self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |session, cx| session.set_annotations_shown(true, cx));
        }
    }

    /// Switches the stream to all changes when the interdiff shown doesn't draw `thread`.
    fn show_all_changes_for(&self, thread: &ThreadKey, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let elsewhere = {
            let session = session.read(cx);
            session
                .card(thread)
                .is_some_and(|found| session.card_placement(found).is_not_in_comparison())
        };
        if elsewhere {
            session.update(cx, |session, cx| {
                session.set_comparison(Comparison::All, cx)
            });
        }
    }

    /// The thread of the review's own whose row's menu is open.
    pub fn menu_thread(&self) -> Option<&ThreadId> {
        self.menu_key().and_then(ThreadKey::local)
    }

    /// The thread whose row's menu is open: one of the review's own, or one of GitHub's.
    pub fn menu_key(&self) -> Option<&ThreadKey> {
        self.menu.as_ref().map(|menu| &menu.thread)
    }

    /// Opens `thread`'s row menu at `position` (a right click on its row).
    pub fn open_menu(
        &mut self,
        thread: impl Into<ThreadKey>,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.menu = Some(RowMenu {
            thread: thread.into(),
            position,
        });
        cx.notify();
    }

    pub fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    /// Resolve or Reopen from the row menu, as the card's buttons do (nothing on a finished
    /// review).
    pub fn set_state(&mut self, thread: &ThreadId, state: ThreadState, cx: &mut Context<Self>) {
        self.close_menu(cx);
        self.diff_pane
            .update(cx, |pane, cx| pane.set_thread_state(thread, state, cx));
        cx.notify();
    }

    /// Reply from the row menu: the thread shown (in all changes for an "all changes" row) and
    /// its reply composer open in its card (nothing on a finished review).
    pub fn reply(&mut self, thread: &ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(cx);
        let Some(session) = self.session.clone() else {
            return;
        };
        if session.read(cx).is_read_only() {
            return;
        }
        // Asking about an annotation needs its card: shown again when `A` hid it.
        if self.annotation_drawable(thread, cx) == Some(true) {
            self.show_annotations_in_stream(cx);
        }
        self.show_all_changes_for(&thread.into(), cx);
        self.diff_pane
            .update(cx, |pane, cx| pane.reply_to_thread(thread, window, cx));
        cx.notify();
    }

    /// Copy as prompt from the row menu: `thread` alone, in the words "Copy as prompt" uses,
    /// headed for its state ("Resolved review comment on …").
    pub fn copy_thread(&mut self, thread: &ThreadId, cx: &mut Context<Self>) {
        self.close_menu(cx);
        let Some(session) = &self.session else {
            return;
        };
        let text = {
            let session = session.read(cx);
            let Some(row) = rows(session, true, true)
                .into_iter()
                .find(|row| &row.id == thread)
            else {
                return;
            };
            let Some(found) = session.review().thread(thread) else {
                return;
            };
            prompt_markdown(
                prompt_heading(found.state),
                &session.prompt_label(),
                &[(found, prompt_location(&row))],
            )
        };
        crate::system::write_clipboard(text, cx);
        cx.notify();
    }

    /// "Copy as prompt": the open threads, then GitHub's unresolved ones on a PR review, as
    /// Markdown, on the clipboard (recorded under automation). Nothing happens without one.
    pub fn copy_as_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let text = {
            let session = session.read(cx);
            let threads = prompt_threads(session);
            let github = github_prompt_threads(session);
            if threads.is_empty() && github.is_empty() {
                return;
            }
            prompt_markdown_with(
                OPEN_PROMPT_HEADING,
                &session.prompt_label(),
                &threads,
                &github,
            )
        };
        crate::system::write_clipboard(text, cx);
    }
}
