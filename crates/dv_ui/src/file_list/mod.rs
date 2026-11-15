//! The panel's file list (DESIGN §4.1 "Panel", PLAN §11 `file_list`): "Changes" with the
//! review's totals and how much of it is viewed, a filter field, and one 28px row per changed
//! file — its viewed checkbox, its status letter, its path with the directory dimmed, its
//! totals and its open comments.
//!
//! The rows follow the stream: the current file ([`DiffPane::current_file`]) is highlighted and
//! kept in view, and a click on a row asks the stream to show that file
//! ([`DiffPane::reveal_file`]). The checkbox marks the file viewed in the round in progress
//! through the session's writes (`review::ops::mark_viewed`, never a hand-built event); a
//! finished review's checkboxes are disabled, not hidden (DESIGN §4.4).
//!
//! The filter keeps the files whose path (or old path) contains every word typed, ignoring case.
//! In it, `Enter` shows the first file left and `Esc` clears it; both hand the keyboard back to
//! the stream ([`FileListEvent::Done`]).
//!
//! While the first listing loads, skeleton rows stand in for up to [`SKELETON_FOR`], then a quiet
//! line (DESIGN §4.4).
//!
//! Automation targets: `file_list/filter`, `file_list/<path>`, `file_list/<path>/viewed`.

mod render;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use dv_core::diff::{FileEntry, FileStatus};
use gpui::{
    AppContext as _, Context, Entity, EventEmitter, Focusable as _, Hsla, Subscription, Task,
    UniformListScrollHandle, Window,
};
use gpui_component::input::{InputEvent, InputState};

use crate::actions::file_list::{ClearFilter, RevealFirst};
use crate::diff_pane::{DiffPane, DiffPaneEvent};
use crate::session::ReviewSession;
use crate::theme::DvTheme;

/// How long the first listing shows skeleton rows before a quiet progress line (DESIGN §4.4).
pub const SKELETON_FOR: Duration = Duration::from_millis(300);

/// The filter field's placeholder.
pub const FILTER_PLACEHOLDER: &str = "Filter files";

/// A path's directory (with its trailing `/`, empty at the root) and its file name.
pub fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(at) => path.split_at(at + 1),
        None => ("", path),
    }
}

/// Whether `entry` passes the filter `query`: every whitespace-separated word, ignoring case,
/// is in its path or its old path. An empty query passes everything.
pub fn matches_filter(entry: &FileEntry, query: &str) -> bool {
    let path = entry.path.to_lowercase();
    let old = entry.old_path.as_deref().map(str::to_lowercase);
    query.split_whitespace().all(|word| {
        let word = word.to_lowercase();
        path.contains(&word) || old.as_deref().is_some_and(|old| old.contains(&word))
    })
}

/// Which hue a status letter takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hue {
    Green,
    Yellow,
    Red,
    Blue,
}

/// How a file's status shows (DESIGN §4.1: colour and letter, never colour alone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusLetter {
    /// The row's letter: `M`, `A`, `D`, `R`, `C`, `T`, `U` (untracked) or `!` (unmerged).
    pub letter: &'static str,
    /// The word the stream's header pill and tooltips use.
    pub word: &'static str,
    pub hue: Hue,
}

impl StatusLetter {
    /// The letter's colour and its soft fill.
    pub fn colors(&self, theme: &DvTheme) -> (Hsla, Hsla) {
        let c = &theme.colors;
        match self.hue {
            Hue::Green => (c.green, c.green_soft),
            Hue::Yellow => (c.yellow, c.yellow_soft),
            Hue::Red => (c.red, c.red_soft),
            Hue::Blue => (c.blue, c.blue_soft),
        }
    }
}

/// The letter for `status`. Untracked files are `U` (DESIGN §4.1), so a conflict, git's own `U`,
/// is `!`.
pub fn status_letter(status: FileStatus) -> StatusLetter {
    let (letter, word, hue) = match status {
        FileStatus::Added => ("A", "Added", Hue::Green),
        FileStatus::Untracked => ("U", "Untracked", Hue::Green),
        FileStatus::Modified => ("M", "Modified", Hue::Yellow),
        FileStatus::TypeChanged => ("T", "Type changed", Hue::Yellow),
        FileStatus::Deleted => ("D", "Deleted", Hue::Red),
        FileStatus::Unmerged => ("!", "Unmerged", Hue::Red),
        FileStatus::Renamed { .. } => ("R", "Renamed", Hue::Blue),
        FileStatus::Copied { .. } => ("C", "Copied", Hue::Blue),
    };
    StatusLetter { letter, word, hue }
}

/// What the file list tells the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileListEvent {
    /// The filter is done with the keyboard: hand it back to the stream.
    Done,
}

/// The panel's file list (see the module docs).
pub struct FileList {
    session: Option<Entity<ReviewSession>>,
    diff_pane: Entity<DiffPane>,
    filter: Entity<InputState>,
    query: String,
    /// Indices into the session's files that the filter leaves, in list order.
    shown: Vec<usize>,
    /// Each file's open threads (`ReviewSession::open_thread_counts`), counted when the
    /// session changes rather than for every row drawn.
    open_threads: HashMap<String, usize>,
    scroll: UniformListScrollHandle,
    /// When the session started its first listing, while it hasn't landed.
    loading_since: Option<Instant>,
    /// Repaints once [`SKELETON_FOR`] has passed.
    skeleton_timer: Option<Task<()>>,
    _session_events: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FileListEvent> for FileList {}

impl FileList {
    pub fn new(diff_pane: Entity<DiffPane>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(FILTER_PLACEHOLDER));
        let subscriptions = vec![
            cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query_changed(cx);
                }
            }),
            cx.subscribe(&diff_pane, |this, _, event: &DiffPaneEvent, cx| {
                if matches!(event, DiffPaneEvent::CurrentFileChanged) {
                    this.scroll_to_current(cx);
                    cx.notify();
                }
            }),
        ];
        Self {
            session: None,
            diff_pane,
            filter,
            query: String::new(),
            shown: Vec::new(),
            open_threads: HashMap::new(),
            scroll: UniformListScrollHandle::new(),
            loading_since: None,
            skeleton_timer: None,
            _session_events: None,
            _subscriptions: subscriptions,
        }
    }

    /// Lists `session`'s files (or nothing). The filter stays as typed.
    pub fn set_session(&mut self, session: Option<Entity<ReviewSession>>, cx: &mut Context<Self>) {
        self._session_events = session
            .as_ref()
            .map(|session| cx.observe(session, |this, _, cx| this.session_changed(cx)));
        self.session = session;
        self.loading_since = None;
        self.session_changed(cx);
    }

    /// Whether the panel stands in skeleton rows (or its quiet line) for the first listing.
    pub fn shows_skeletons(&self) -> bool {
        self.loading_since.is_some()
    }

    /// The filter as typed.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The paths the filter leaves, in list order.
    pub fn shown_paths<'a>(&self, cx: &'a gpui::App) -> Vec<&'a str> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let files = session.read(cx).files();
        self.shown
            .iter()
            .filter_map(|&ix| files.get(ix).map(|entry| entry.path.as_str()))
            .collect()
    }

    /// Puts `text` in the filter, as if typed.
    pub fn set_query(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.filter.update(cx, |filter, cx| {
            filter.set_value(text.to_string(), window, cx)
        });
        self.query_changed(cx);
    }

    /// The filter field's focus handle.
    pub fn filter_focus(&self, cx: &gpui::App) -> gpui::FocusHandle {
        self.filter.read(cx).focus_handle(cx)
    }

    /// Whether the filter field has the keyboard.
    pub fn is_filter_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.filter.read(cx).focus_handle(cx).is_focused(window)
    }

    /// Shows `path` in the stream (a click on its row).
    pub fn reveal(&mut self, path: &str, cx: &mut Context<Self>) {
        self.diff_pane
            .update(cx, |pane, cx| pane.reveal_file(path, cx));
    }

    /// Marks `path` viewed, or not viewed, in the round in progress (its checkbox). Nothing
    /// happens on a finished review, which is read-only.
    pub fn toggle_viewed(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        session.update(cx, |session, cx| session.toggle_viewed(path, cx));
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.filter.read(cx).value().to_string();
        if query != self.query {
            self.query = query;
            self.refilter(cx);
            self.scroll_to_current(cx);
        }
    }

    fn session_changed(&mut self, cx: &mut Context<Self>) {
        // Skeletons stand in for the first listing only, never over a list that is refreshing
        // (DESIGN §4.4).
        let first_listing = self
            .session
            .as_ref()
            .is_some_and(|session| session.read(cx).is_first_load());
        match (first_listing, self.loading_since) {
            (true, None) => {
                self.loading_since = Some(Instant::now());
                self.skeleton_timer = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(SKELETON_FOR).await;
                    this.update(cx, |this, cx| {
                        this.skeleton_timer = None;
                        cx.notify();
                    })
                    .ok();
                }));
            }
            (false, Some(_)) => {
                self.loading_since = None;
                self.skeleton_timer = None;
            }
            _ => {}
        }
        self.open_threads = self
            .session
            .as_ref()
            .map(|session| session.read(cx).open_thread_counts())
            .unwrap_or_default();
        self.refilter(cx);
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        self.shown = match &self.session {
            Some(session) => session
                .read(cx)
                .files()
                .iter()
                .enumerate()
                .filter(|(_, entry)| matches_filter(entry, &self.query))
                .map(|(ix, _)| ix)
                .collect(),
            None => Vec::new(),
        };
        cx.notify();
    }

    /// Scrolls the current file's row into view, if the filter shows it.
    fn scroll_to_current(&mut self, cx: &mut Context<Self>) {
        let (Some(session), Some(current)) =
            (&self.session, self.diff_pane.read(cx).current_file())
        else {
            return;
        };
        let files = session.read(cx).files();
        if let Some(row) = self
            .shown
            .iter()
            .position(|&ix| files.get(ix).is_some_and(|entry| entry.path == current))
        {
            self.scroll
                .scroll_to_item(row, gpui::ScrollStrategy::Nearest);
        }
    }

    fn reveal_first(&mut self, _: &RevealFirst, _: &mut Window, cx: &mut Context<Self>) {
        let first = self.shown_paths(cx).first().map(|path| path.to_string());
        if let Some(path) = first {
            self.reveal(&path, cx);
        }
        cx.emit(FileListEvent::Done);
    }

    fn clear_filter(&mut self, _: &ClearFilter, window: &mut Window, cx: &mut Context<Self>) {
        if !self.query.is_empty() {
            self.set_query("", window, cx);
        }
        cx.emit(FileListEvent::Done);
    }
}
