//! The file view (PLAN §18.1 "The file view", DESIGN §4.1 "File view"): one file of the
//! repository whole, at the review's new side (its old side for a file the review deletes),
//! over the main area in place of the stream, one at a time.
//!
//! **The pane.** The file view is the stream's pane in a one-file scope: a second [`DiffPane`]
//! whose one block is that file, so the cursor, line selection, comments and their cards, text
//! selection, the caret, `Ctrl C` and `Ctrl F` (searching that file only) all work as they do
//! in the stream. Its rows are the stream's unified rows with one line number (the side's), no
//! gaps and no footer, under a header of its own (`render`): the path, the side's label, whether
//! the review changes the file, and a close button.
//!
//! **The text.** A file the stream lists (in the comparison shown) is shown from the diff the
//! stream shows, cut to its side ([`side_only`]): its added lines keep their kind, so they wear
//! the diff's tint and marker. Any other file is read at the review's new side
//! (`ReviewSession::side_file`, `dv_core::diff::side_file`), every line unchanged. Both are the
//! side's text as the diff pipeline decodes it, so a line here is the same line, byte for byte,
//! as in the stream, and anchors taken here compare equal (PLAN §8). The session loads and
//! highlights the file whatever the stream's viewport (`ReviewSession::set_file_view`); until
//! the text is there the view shows skeleton rows. A file the side doesn't hold, binary or too
//! large shows a notice instead of lines.
//!
//! **The working tree.** Go to definition and references in a review whose new side isn't the
//! working tree show their place as the working tree holds it now, as the language server read
//! it (PLAN §18.2): the file view then reads the file there ([`DiffPane::open_file_at`],
//! `ReviewSession::worktree_file`), labelled "working tree", read-only (no comments, no
//! threads: its lines aren't the review's).
//!
//! **Outside the repository.** A place outside the repository (a library's declarations under
//! the toolchain) shows the same way, read-only, the file by its absolute path
//! (`ReviewSession::outside_file`), labelled "Outside the repository · read-only" with its
//! path; hover and go to definition work in it where a server answers for it.
//!
//! **Threads.** Every live thread on the file's side that stands at lines of it shows inline
//! under its last line (placed by the same placement the stream uses: in a listed file, or in
//! a file the review doesn't change, `Placement::Unlisted`); comments on the whole file and
//! outdated threads head it. Any line takes a comment (its `+`, `C`, `Shift ↑`/`↓`), anchored
//! on the text shown (`ReviewSession::anchor_in_text`), whether or not the review changes it.
//!
//! **Opening and closing.** The shell opens it ([`DiffPane::open_file`]) from the header menu's
//! View file, `F`, `Ctrl P` and the history; `Esc` once nothing else in it takes the key, or the
//! close button, asks the shell to show the stream again ([`DiffPaneEvent::CloseFileView`]),
//! which kept its place meanwhile.
//!
//! Automation targets: `file_view` (the pane), `file_view/header`, `file_view/close`,
//! `file_view/line/<n>` (a line number), `file_view/comment/<n>` (its `+`) and
//! `file_view/code/<n>/<column>` (a character of a line's code), besides the find bar's and the
//! cards' own.

mod render;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use dv_core::diff::{DiffLimits, DiffLine, FileDiff, LineKind, Segment, TextDiff};
use dv_core::review::AnchorSide;
use gpui::{App, Context, ListOffset};

use super::rows::{Content, Insert, InsertItem, LinePos, RowsSpec, file_rows, side_number};
use super::{Block, Cursor, DiffPane, DiffPaneEvent};
use crate::failure::Failure;
use crate::services::ViewMode;
use crate::session::{Card, FileState, ReviewSession, SideFileState, ViewFrom};
use crate::thread::ComposerTarget;

/// The file view's scope in its pane.
#[derive(Default)]
pub(super) struct FileView {
    /// The file shown, if one is.
    pub file: Option<OpenFile>,
}

/// The file the file view shows.
pub(super) struct OpenFile {
    pub path: String,
    /// The side shown: the new one, or the old one for a file the review deletes.
    pub side: AnchorSide,
    /// The line (on `side`) to put the cursor on once the text is there.
    pub go_to: Option<u32>,
    /// "Load anyway" on a file read at its side that is over the size caps.
    pub load_anyway: bool,
    /// Where it is read: at the review's side, from the working tree as it stands, or outside
    /// the repository (module docs).
    pub from: ViewFrom,
    /// The line it was opened at (a definition's), which an outside file's label names.
    pub opened_at: Option<u32>,
    /// The text shown, cut to its side, the diff it was cut from and whether it was cut with
    /// "Load anyway".
    cut: Option<(Arc<FileDiff>, bool, Arc<FileDiff>)>,
}

/// Where the file view's text comes from now (module docs, "The text").
pub(super) enum Source {
    /// The diff the stream shows for a listed file.
    Listed(crate::session::Shown),
    /// Read at the side shown.
    Read(crate::session::Shown),
    Loading,
    /// The new side holds no such file.
    Missing,
    Failed(Failure),
}

/// What `state.file_view` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileViewSummary {
    pub path: String,
    pub side: AnchorSide,
    /// The first line in view below the header.
    pub top_line: Option<u32>,
    pub line_count: usize,
    /// The review changes the file (its own list, all changes, whichever comparison shows).
    pub changed: bool,
    /// The numbers of its lines marked changed (as the stream marks them: in an interdiff,
    /// changed since the round), at most the first 200.
    pub changed_lines: Vec<u32>,
    /// The text isn't there yet.
    pub loading: bool,
    /// Read from the working tree as it stands (module docs, "The working tree").
    pub worktree: bool,
    /// A file outside the repository (module docs, "Outside the repository").
    pub outside: bool,
    /// The notice it shows instead of lines (`rows::Notice::key`), if any.
    pub notice: Option<&'static str>,
}

/// `text`'s lines on `side` alone, as the file view shows them: the side's every line in order,
/// numbered on that side only, each keeping its kind (an added line on the new side, a removed
/// one on the old) and its intraline words.
pub fn side_only(text: &TextDiff, side: AnchorSide) -> TextDiff {
    let lines = text
        .lines
        .iter()
        .filter(|line| side_number(line, side).is_some())
        .map(|line| match side {
            AnchorSide::New => DiffLine {
                old_no: None,
                ..line.clone()
            },
            AnchorSide::Old => DiffLine {
                new_no: None,
                ..line.clone()
            },
        })
        .collect();
    let eof = match side {
        AnchorSide::Old => text.old_eof_newline,
        AnchorSide::New => text.new_eof_newline,
    };
    TextDiff {
        lines,
        old_eof_newline: eof,
        new_eof_newline: eof,
        crlf: text.crlf,
    }
}

/// `diff` cut to `side` ([`side_only`]); anything but text as it is.
fn cut(diff: &FileDiff, side: AnchorSide) -> FileDiff {
    match diff {
        FileDiff::Text(text) => FileDiff::Text(side_only(text, side)),
        other => other.clone(),
    }
}

/// A file read whole at `side` as the file view shows it: cut to the side, the old side of a
/// file the review deletes with every line removed (as the stream marks it), and over the size
/// caps (`DiffLimits`) a too-large notice unless `load_anyway`.
fn read_as_shown(diff: &FileDiff, side: AnchorSide, load_anyway: bool) -> FileDiff {
    let FileDiff::Text(text) = cut(diff, side) else {
        return diff.clone();
    };
    let limits = DiffLimits::default();
    let bytes: u64 = text
        .lines
        .iter()
        .map(|line| line.text.len() as u64 + 1)
        .sum();
    if !load_anyway && (text.lines.len() > limits.max_lines || bytes > limits.max_bytes) {
        let len = Some(bytes);
        return match side {
            AnchorSide::Old => FileDiff::TooLarge {
                old_len: len,
                new_len: None,
            },
            AnchorSide::New => FileDiff::TooLarge {
                old_len: None,
                new_len: len,
            },
        };
    }
    let mut text = text;
    if side == AnchorSide::Old {
        for line in &mut text.lines {
            line.kind = LineKind::Removed;
        }
    }
    FileDiff::Text(text)
}

impl DiffPane {
    /// The file view's pane (module docs): no file shown until [`DiffPane::open_file`].
    pub fn new_file_view(cx: &mut Context<Self>) -> Self {
        Self {
            file_view: Some(FileView::default()),
            mode: ViewMode::Unified,
            ..Self::new(cx)
        }
    }

    /// Whether this is the file view's pane.
    pub fn is_file_view(&self) -> bool {
        self.file_view.is_some()
    }

    /// The file the file view shows, if any.
    pub fn file_view_path(&self) -> Option<&str> {
        self.shown_file().map(|file| file.path.as_str())
    }

    /// Whether the file view reads its file from the working tree as it stands.
    pub fn file_view_worktree(&self) -> bool {
        self.file_view_from() == Some(ViewFrom::Worktree)
    }

    /// Where the file view reads its file, while it shows one.
    pub fn file_view_from(&self) -> Option<ViewFrom> {
        self.shown_file().map(|file| file.from)
    }

    /// Whether the file view shows a file as the disk holds it, outside the review's sides:
    /// the working tree as it stands, or a file outside the repository. Read-only, and what a
    /// server reads for it is the disk's (PLAN §18.2).
    pub fn file_view_on_disk(&self) -> bool {
        matches!(
            self.file_view_from(),
            Some(ViewFrom::Worktree | ViewFrom::Outside)
        )
    }

    /// The line the file view's file was opened at, if one was asked for.
    pub(super) fn file_view_opened_at(&self) -> Option<u32> {
        self.shown_file()?.opened_at
    }

    /// The side the file view shows.
    pub fn file_view_side(&self) -> Option<AnchorSide> {
        self.shown_file().map(|file| file.side)
    }

    fn shown_file(&self) -> Option<&OpenFile> {
        self.file_view.as_ref()?.file.as_ref()
    }

    /// The text the file view shows, cut to its side, once it is there.
    pub(super) fn file_view_text(&self) -> Option<Arc<FileDiff>> {
        self.shown_file()?
            .cut
            .as_ref()
            .map(|(_, _, cut)| cut.clone())
    }

    /// Whether showing `path` in place of the file shown would drop a draft: a composer that
    /// holds text on another file (or a reply in a card it would take away). The shell then
    /// gives the draft the keyboard instead ([`DiffPane::take_up_draft`]).
    pub fn draft_blocks(&self, path: Option<&str>, cx: &App) -> bool {
        self.has_draft(cx) && (path.is_none() || self.file_view_path() != path)
    }

    /// Gives a composer holding a draft the keyboard (and brings it into view): `true` when
    /// there was one (DESIGN §5: a draft takes the keyboard instead).
    pub fn take_up_draft(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> bool {
        self.keep_composer(window, cx)
    }

    /// Shows `path` whole (module docs), its cursor on line `line` of the side shown once the
    /// text is there (else the first line). The same file again only moves the cursor.
    pub fn open_file(&mut self, path: &str, line: Option<u32>, cx: &mut Context<Self>) {
        self.open_file_at(path, line, ViewFrom::Review, cx);
    }

    /// [`DiffPane::open_file`], reading the file where `from` says (module docs, "The working
    /// tree", "Outside the repository").
    pub fn open_file_at(
        &mut self,
        path: &str,
        line: Option<u32>,
        from: ViewFrom,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let side = match from {
            ViewFrom::Review => session.read(cx).view_side(path),
            ViewFrom::Worktree | ViewFrom::Outside => AnchorSide::New,
        };
        let same = self
            .shown_file()
            .is_some_and(|file| file.path == path && file.side == side && file.from == from);
        // A draft is never dropped (the shell doesn't ask while one would be).
        if !same && self.has_draft(cx) {
            return;
        }
        let Some(view) = self.file_view.as_mut() else {
            return;
        };
        if !same {
            view.file = Some(OpenFile {
                path: path.to_string(),
                side,
                go_to: line,
                load_anyway: false,
                from,
                opened_at: line,
                cut: None,
            });
            self.cursor = None;
            self.selection = None;
            self.clear_text_selection();
            self.caret = None;
            self.set_focused(None);
            self.files.clear();
            self.measured.clear();
            self.h_offset = 0.;
            // The find bar's matches were the other file's.
            self.find.current = None;
            // An empty composer on the file shown before goes with it.
            if self.composer.take().is_some() {
                cx.emit(DiffPaneEvent::ComposerClosed);
            }
            self.list.reset(1);
            self.list.scroll_to(ListOffset::default());
        } else if let Some(file) = view.file.as_mut() {
            file.go_to = line;
            if line.is_some() {
                file.opened_at = line;
            }
        }
        session.update(cx, |session, cx| {
            session.set_file_view_at(Some(path.to_string()), from, cx)
        });
        self.rebuild_all(cx);
        self.go_to_pending_line(cx);
    }

    /// Whether the file view shows a file outside the repository that `probe_path` didn't read
    /// at all (over the 4 MiB it reads): its too-large notice has no "Load anyway". One read
    /// whole but over the display caps (a library's 40,000-line declarations) has it, as any
    /// file read at a side does.
    pub(super) fn file_view_outside_unread(&self, cx: &App) -> bool {
        self.file_view_from() == Some(ViewFrom::Outside)
            && matches!(
                self.file_view_source(cx),
                Source::Read(shown) if matches!(*shown.diff, FileDiff::TooLarge { .. })
            )
    }

    /// "Load anyway" on the file view's too-large notice: a file the stream lists loads again
    /// without the caps, as the stream's notice does; one read at its side, from the working
    /// tree or outside the repository (read whole already) shows its lines.
    pub fn file_view_load_anyway(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.file_view_path().map(str::to_string) else {
            return;
        };
        let listed = matches!(self.file_view_source(cx), Source::Listed(_));
        if listed {
            if let Some(session) = self.session.clone() {
                session.update(cx, |session, cx| session.load_anyway(&path, cx));
            }
            return;
        }
        if let Some(file) = self.file_view.as_mut().and_then(|view| view.file.as_mut()) {
            file.load_anyway = true;
        }
        self.rebuild_all(cx);
    }

    /// Stops showing a file (the shell shows the stream again): the session no longer loads it
    /// for the view.
    pub fn close_file(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.session.clone() {
            session.update(cx, |session, cx| session.set_file_view(None, cx));
        }
        self.find.open = false;
        cx.notify();
    }

    /// Where the file view's text comes from now (module docs, "The text").
    pub(super) fn file_view_source(&self, cx: &App) -> Source {
        let (Some(file), Some(session)) = (self.shown_file(), self.session.as_ref()) else {
            return Source::Loading;
        };
        let session = session.read(cx);
        let read = |state: SideFileState| match state {
            SideFileState::Loaded(shown) => Source::Read(shown),
            SideFileState::Loading => Source::Loading,
            SideFileState::Missing => Source::Missing,
            SideFileState::Failed(failure) => Source::Failed(failure),
        };
        match file.from {
            ViewFrom::Worktree => return read(session.worktree_file(&file.path)),
            ViewFrom::Outside => return read(session.outside_file(&file.path)),
            ViewFrom::Review => {}
        }
        if session.view_from_stream(&file.path) {
            return match (session.shown(&file.path), session.file_state(&file.path)) {
                (_, Some(FileState::Failed(failure))) => Source::Failed(failure.clone()),
                (Some(shown), _) => Source::Listed(shown),
                (None, _) => Source::Loading,
            };
        }
        read(session.side_file(file.side, &file.path))
    }

    /// The diff and highlights the file view's rows paint, its text cut to its side.
    pub(super) fn file_view_shown(&self, cx: &App) -> Option<crate::session::Shown> {
        let diff = self.file_view_text()?;
        let highlights = match self.file_view_source(cx) {
            Source::Listed(shown) | Source::Read(shown) => shown.highlights,
            _ => None,
        };
        Some(crate::session::Shown { diff, highlights })
    }

    /// The file view's one block (module docs): its text cut to its side, taken as the file's
    /// diff (so the reading position and the cursor follow an edit, `reload`), every line shown.
    pub(super) fn file_view_block(&mut self, cx: &App) -> Option<Block> {
        let path = self.shown_file()?.path.clone();
        let side = self.shown_file()?.side;
        let load_anyway = self.shown_file()?.load_anyway;
        let source = self.file_view_source(cx);
        let (from, read) = match &source {
            Source::Listed(shown) => (Some(shown.diff.clone()), false),
            Source::Read(shown) => (Some(shown.diff.clone()), true),
            _ => (None, false),
        };
        // Cut again only when what it is cut from changed (or "Load anyway").
        let kept = self
            .shown_file()?
            .cut
            .as_ref()
            .filter(|(was, anyway, _)| {
                *anyway == load_anyway && from.as_ref().is_some_and(|from| Arc::ptr_eq(was, from))
            })
            .map(|(_, _, cut)| cut.clone());
        let shown = match (kept, &from) {
            (Some(kept), _) => Some(kept),
            (None, Some(from)) if read => Some(Arc::new(read_as_shown(from, side, load_anyway))),
            (None, Some(from)) => Some(Arc::new(cut(from, side))),
            (None, None) => None,
        };
        if let Some(file) = self.file_view.as_mut().and_then(|view| view.file.as_mut()) {
            file.cut = match (&from, &shown) {
                (Some(from), Some(shown)) => Some((from.clone(), load_anyway, shown.clone())),
                _ => file
                    .cut
                    .take()
                    .filter(|_| matches!(source, Source::Loading)),
            };
        }
        // While it loads again, the text it had stays.
        let shown = shown.or_else(|| self.file_view_text());
        let session = self.session.clone()?;
        let session = session.read(cx);
        if let Some(diff) = &shown {
            let comparison = session.comparison();
            self.take_diff(&path, diff, comparison, &[]);
            let ui = self.files.entry(path.clone()).or_default();
            let count = diff.as_text().map_or(0, |text| text.lines.len());
            ui.segments = if count == 0 {
                Vec::new()
            } else {
                vec![Segment::Lines(0..count)]
            };
        }
        let composer = self.composer.as_ref().map(|composer| &composer.target);
        let on_disk = self.file_view_on_disk();
        let (inserts, heading) = match &shown {
            // The disk's lines aren't the review's: its threads don't stand at them.
            Some(diff) if !on_disk => self.file_view_inserts(&path, side, diff, session, composer),
            _ => (Vec::new(), Vec::new()),
        };
        let content = match (&shown, &source) {
            (Some(diff), _) => Content::Loaded(diff),
            (None, Source::Failed(_) | Source::Missing) => Content::Failed,
            (None, _) => Content::Loading,
        };
        let ui = self.files.entry(path.clone()).or_default();
        let rows = file_rows(&RowsSpec {
            mode: ViewMode::Unified,
            collapsed: false,
            content,
            segments: &ui.segments,
            inserts: &inserts,
            heading: &heading,
            footer: false,
            whole: true,
        });
        Some(Block { path, rows })
    }

    /// The live threads the file view draws: on its side, placed at lines of its file, a comment
    /// on the whole file, or outdated in it; on either side in a file the review doesn't change,
    /// whose two sides are the same text (PLAN §8); in the order they were added. In an interdiff
    /// an old side is a round's snapshot, which no thread shows on.
    pub(super) fn file_view_threads<'a>(&self, session: &'a ReviewSession) -> Vec<Card<'a>> {
        let Some(file) = self.shown_file() else {
            return Vec::new();
        };
        if file.side == AnchorSide::Old && session.comparison().is_interdiff() {
            return Vec::new();
        }
        session
            .cards()
            .filter(|&card| {
                let placement = session.card_placement(card);
                let inner = placement.placement();
                !placement.is_pending()
                    && session.shows(card, &placement)
                    && ((card.anchor().side == file.side
                        && inner.file() == Some(file.path.as_str()))
                        || inner.unlisted_path() == Some(file.path.as_str()))
            })
            .collect()
    }

    /// The file view's cards (module docs, "Threads"): inline under their last line, and those
    /// heading the file (comments on the whole file, the composer writing one, outdated ones).
    fn file_view_inserts(
        &self,
        path: &str,
        side: AnchorSide,
        diff: &FileDiff,
        session: &ReviewSession,
        composer: Option<&ComposerTarget>,
    ) -> (Vec<Insert>, Vec<InsertItem>) {
        let lines = diff.as_text().map(|text| text.lines.as_slice());
        let mut inserts = Vec::new();
        let mut files = Vec::new();
        let mut outdated = Vec::new();
        for card in self.file_view_threads(session) {
            let item = InsertItem::Thread(card.key());
            if card.anchor().is_file_level() {
                files.push(item);
                continue;
            }
            let placement = session.card_placement(card);
            let inner = placement.placement();
            let after = inner
                .lines(card.anchor())
                .map(|(_, end)| end)
                .or_else(|| inner.trail().and_then(|trail| trail.after_line()))
                .filter(|_| side == AnchorSide::New || inner.trail().is_none());
            match (after, lines) {
                (Some(end), Some(lines)) => match line_at(lines, side, end) {
                    Some(line) => inserts.push(Insert { line, side, item }),
                    None => outdated.push(item),
                },
                _ => outdated.push(item),
            }
        }
        if let (Some(target), Some(lines)) = (composer, lines)
            && target.reply_to.is_none()
            && !target.is_file()
            && target.path == path
            && let Some(line) = line_at(lines, side, target.end_line)
                .or_else(|| (!lines.is_empty()).then(|| lines.len() - 1))
        {
            inserts.push(Insert {
                line,
                side,
                item: InsertItem::Composer,
            });
        }
        if composer.is_some_and(|target| target.is_file() && target.path == path) {
            files.push(InsertItem::Composer);
        }
        files.extend(outdated);
        (inserts, files)
    }

    /// Puts the cursor on the line the file view was opened at (else its first line) once its
    /// text is there, and scrolls it into view: the middle of the view for a line asked for.
    pub(super) fn go_to_pending_line(&mut self, cx: &mut Context<Self>) {
        let Some(file) = self.shown_file() else {
            return;
        };
        let (path, side, go_to) = (file.path.clone(), file.side, file.go_to);
        let Some(lines) = self.lines(&path) else {
            return;
        };
        if lines.is_empty() {
            return;
        }
        if go_to.is_none() && self.cursor.is_some() {
            return;
        }
        let line = go_to
            .and_then(|number| {
                line_at(lines, side, number).or_else(|| {
                    // Past the end (the file got shorter): its last line.
                    lines
                        .iter()
                        .rposition(|line| side_number(line, side).is_some_and(|n| n < number))
                })
            })
            .unwrap_or(0);
        if let Some(file) = self.file_view.as_mut().and_then(|view| view.file.as_mut()) {
            file.go_to = None;
        }
        self.cursor = Some(Cursor {
            path: path.clone(),
            pos: LinePos { line, side },
        });
        if go_to.is_some()
            && let Some(item) = self.cursor_item()
        {
            self.centre_item(item);
        }
        self.viewport_changed(cx);
        cx.notify();
    }

    /// `Esc` with nothing else to take it, or the close button: back to the stream.
    pub(super) fn ask_close(&mut self, cx: &mut Context<Self>) {
        if self.is_file_view() {
            cx.emit(DiffPaneEvent::CloseFileView);
        }
    }

    /// The position the file view is at, for the history: its file, side and the cursor's line
    /// (else the first line in view).
    pub fn file_view_position(&self) -> Option<(String, AnchorSide, u32)> {
        let file = self.shown_file()?;
        let line = self
            .cursor()
            .filter(|cursor| cursor.path == file.path)
            .map(|cursor| cursor.line)
            .or_else(|| self.first_line_in_view())
            .or(file.go_to)
            .unwrap_or(1);
        Some((file.path.clone(), file.side, line))
    }

    /// The number of the first line in view below the header.
    fn first_line_in_view(&self) -> Option<u32> {
        let cursor = self.first_visible_cell()?;
        let line = self.line(&cursor.path, cursor.pos.line)?;
        side_number(line, cursor.pos.side)
    }

    /// What `state.file_view` reports, `None` in the stream's pane or with no file shown.
    pub fn file_view_summary(&self, cx: &App) -> Option<FileViewSummary> {
        let file = self.shown_file()?;
        let lines = self.lines(&file.path);
        let notice = self
            .blocks
            .first()
            .and_then(|block| {
                block.rows.iter().find_map(|row| match row {
                    super::rows::Row::Notice(notice) => Some(notice.key()),
                    _ => None,
                })
            })
            .filter(|notice| *notice != "loading");
        let changed = self
            .session
            .as_ref()
            .is_some_and(|session| session.read(cx).review_file(&file.path).is_some());
        let changed_lines = lines
            .map(|lines| {
                lines
                    .iter()
                    .filter(|line| line.kind != LineKind::Context)
                    .filter_map(|line| side_number(line, file.side))
                    .take(200)
                    .collect()
            })
            .unwrap_or_default();
        Some(FileViewSummary {
            path: file.path.clone(),
            side: file.side,
            top_line: self.first_line_in_view(),
            line_count: lines.map_or(0, <[DiffLine]>::len),
            changed,
            changed_lines,
            loading: file.cut.is_none() && notice.is_none(),
            notice,
            worktree: file.from == ViewFrom::Worktree,
            outside: file.from == ViewFrom::Outside,
        })
    }
}

/// The index of the line numbered `number` on `side` among `lines`.
fn line_at(lines: &[DiffLine], side: AnchorSide, number: u32) -> Option<usize> {
    lines
        .iter()
        .position(|line| side_number(line, side) == Some(number))
}
