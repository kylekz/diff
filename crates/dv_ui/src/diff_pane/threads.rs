//! Threads in the stream (DESIGN §4.1, §5): where their cards and the composer go, the keys
//! that move between threads (`]`/`[`), reply (`R`), resolve (`X`) and comment (`C`), and the
//! writes behind them, each a `dv_core::review::ops` call through the session.
//!
//! A thread shows inline after the last line of its range on its side where the session places
//! it (`ReviewSession::placement`, PLAN §8): at its recorded lines, or where relocation found
//! them since ("· moved"). One relocation can't find is outdated (DESIGN §4.1): when its
//! round's snapshot tells where its code went (a trail, PLAN §8), its card shows there, under
//! the last line that replaced it (or the line before where it was removed), with its lines as
//! they were; else it heads its file. One whose file left the review heads the stream, in the
//! "Not in the diff" section (`section`). A comment on the whole file heads its file too, ahead
//! of the outdated cards, with its composer (`file_comment`). In an interdiff, old-side threads and
//! those whose file isn't in it (but is in the review) aren't drawn, and `C` comments on the
//! new side only. Resolved threads collapse to one line until opened. The composer sits under
//! the selection's last line (a new thread) or inside the card it answers (a reply, which goes
//! wherever that card is drawn), which stays open meanwhile, and takes the keyboard while it is
//! open; posting or cancelling hands it back to the stream.
//!
//! The composer's row carries its field's focus handle into the list, so the field stays
//! rendered, and the keys work, while the stream scrolls it out of view; typing brings it back
//! into view. A composer whose row is gone (its file collapsed or failed to load) keeps its
//! text, but never the keyboard, which goes back to the stream; `C` opens its file again and
//! takes it up. One whose file left the diff, or whose thread was deleted, is dropped. A
//! composer that goes for good, its draft with it, says so ([`DiffPaneEvent::ComposerClosed`]):
//! a pick waiting for the draft runs then (`crate::shell::pending`).

use dv_core::diff::{DiffLine, FileDiff};
use dv_core::review::{AnchorSide, ThreadId, ThreadState};
use gpui::{App, AppContext as _, Context, FocusHandle, Focusable as _, Window};
use gpui_component::input::{InputEvent, TextareaState};

use super::rows::{Dir, Insert, InsertItem, Row, side_number};
use super::{DiffPane, DiffPaneEvent, rows};
use crate::failure::Failure;
use crate::session::{Comparison, ReviewSession};
use crate::thread::{COMPOSER_ROWS, Composer, ComposerTarget, REPLY_ROWS};

/// Whether `row` of `path` holds `composer`: its own row in its file, or the card of the thread
/// it answers, wherever that is drawn (a thread's card shows once in the stream).
fn holds_composer(composer: &Composer, path: &str, row: &Row) -> bool {
    match &composer.target.reply_to {
        Some(thread) => row.thread() == Some(thread),
        None => {
            composer.target.path == path
                && matches!(
                    row,
                    Row::Insert(Insert {
                        item: InsertItem::Composer,
                        ..
                    }) | Row::Heading(InsertItem::Composer)
                )
        }
    }
}

/// The thread cards and the composer of one file's rows: the inline inserts, and the outdated
/// threads whose cards head the file. Comments on the whole file, and the composer writing one,
/// head it too, ahead of those (`file_comment::heading_items`).
pub(super) fn thread_rows(
    path: &str,
    diff: &FileDiff,
    session: &ReviewSession,
    composer: Option<&ComposerTarget>,
) -> (Vec<Insert>, Vec<ThreadId>) {
    let mut inserts = Vec::new();
    let mut outdated = Vec::new();
    // A file whose content didn't change shows a notice rather than its lines: its threads'
    // cards head it, with the lines they stand at (DESIGN §4.1).
    let lines = diff
        .as_text()
        .filter(|text| text.has_changes())
        .map(|text| text.lines.as_slice());
    for thread in session
        .threads_in(path)
        .filter(|thread| !thread.anchor.is_file_level())
    {
        let side = thread.anchor.side;
        // Placed against the diff these rows show, which may be the one a reload replaces.
        let placement = session.placement(thread);
        if placement.is_pending() {
            // Its side isn't there to place it against yet: it is drawn once it is.
            continue;
        }
        // Found, it follows the last line of its range; outdated with a trail, the line its
        // code went to (a trail is on the new side); else it heads the file.
        let after = match placement.lines(&thread.anchor) {
            Some((_, end)) => Some((side, end)),
            None => placement
                .trail()
                .and_then(|trail| trail.after_line())
                .map(|line| (AnchorSide::New, line)),
        };
        match (after, lines) {
            (Some((side, end)), Some(lines)) => match line_index(lines, side, end) {
                Some(line) => inserts.push(Insert {
                    line,
                    side,
                    item: InsertItem::Thread(thread.id.clone()),
                }),
                None => outdated.push(thread.id.clone()),
            },
            _ => outdated.push(thread.id.clone()),
        }
    }
    // An interdiff's old column is round n's snapshot: a draft on the old side waits for all
    // changes, where its lines are (`reload`), until this file's own diff of them lands (a
    // switch to all changes shows the interdiff's diff meanwhile), as its threads wait.
    let other_text = composer.is_some_and(|target| target.side == AnchorSide::Old)
        && (session.comparison().is_interdiff()
            || session
                .shown_comparison(path)
                .is_some_and(Comparison::is_interdiff));
    if let (Some(target), Some(lines)) = (composer, lines)
        && target.reply_to.is_none()
        && !target.is_file()
        && target.path == path
        && !other_text
        && let Some(line) = nearest_line(lines, target.side, target.end_line)
    {
        inserts.push(Insert {
            line,
            side: target.side,
            item: InsertItem::Composer,
        });
    }
    (inserts, outdated)
}

/// The lines (indexes into `diff`'s lines) of the threads that show inline in `path`: each
/// one's whole range on its side, or for an outdated one with a trail the lines its code went
/// to (and the line after removed code), which the stream keeps shown (`reload`).
pub(super) fn thread_lines(path: &str, diff: &FileDiff, session: &ReviewSession) -> Vec<usize> {
    let Some(text) = diff.as_text() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for thread in session.threads_in(path) {
        let placement = session.placement(thread);
        let shown = match placement.lines(&thread.anchor) {
            Some(lines) => Some((thread.anchor.side, lines)),
            None => placement.trail().map(|trail| {
                let (start, end) = trail.lines();
                // Removed code stood between the line it follows and the next one.
                let end = match trail.went {
                    dv_core::review::Went::Removed { before_line } => before_line.max(end),
                    dv_core::review::Went::Replaced { .. } => end,
                };
                (AnchorSide::New, (start, end))
            }),
        };
        if let Some((side, (start, end))) = shown {
            out.extend(text.lines.iter().enumerate().filter_map(|(index, line)| {
                side_number(line, side)
                    .filter(|number| (start..=end).contains(number))
                    .map(|_| index)
            }));
        }
    }
    out
}

/// The index of the line numbered `number` on `side`.
fn line_index(lines: &[DiffLine], side: AnchorSide, number: u32) -> Option<usize> {
    lines
        .iter()
        .position(|line| side_number(line, side) == Some(number))
}

/// The line numbered `number` on `side`, else the last one before it, else the first line on
/// that side: where the composer stays when the file changed under it.
fn nearest_line(lines: &[DiffLine], side: AnchorSide, number: u32) -> Option<usize> {
    line_index(lines, side, number)
        .or_else(|| {
            lines
                .iter()
                .enumerate()
                .filter(|(_, line)| side_number(line, side).is_some_and(|n| n < number))
                .map(|(index, _)| index)
                .next_back()
        })
        .or_else(|| lines.iter().position(|line| rows::on_side(line, side)))
}

impl DiffPane {
    /// The open composer.
    pub fn composer(&self) -> Option<&crate::thread::Composer> {
        self.composer.as_ref()
    }

    /// The composer's text as typed.
    pub fn composer_text(&self, cx: &gpui::App) -> Option<String> {
        self.composer
            .as_ref()
            .map(|composer| composer.input.read(cx).value().to_string())
    }

    /// Whether a composer holds a draft: text other than whitespace, which nothing but the
    /// reviewer replaces ([`DiffPane::keep_composer`], `crate::shell::pending`).
    pub fn has_draft(&self, cx: &gpui::App) -> bool {
        self.composer
            .as_ref()
            .is_some_and(|composer| !composer.input.read(cx).value().trim().is_empty())
    }

    /// Whether the keyboard is on this pane or one of its fields (the composer, the find
    /// field).
    pub fn owns_focus(&self, window: &Window, cx: &gpui::App) -> bool {
        self.focus_handle.is_focused(window)
            || self
                .composer
                .as_ref()
                .is_some_and(|composer| composer.input.focus_handle(cx).is_focused(window))
            || self.is_find_focused(window, cx)
    }

    /// The keyboard's thread.
    pub fn focused_thread(&self) -> Option<&ThreadId> {
        self.focused_thread.as_ref()
    }

    /// Whether `thread`'s card shows whole: every thread but a resolved one the reviewer hasn't
    /// opened and isn't replying to.
    pub fn is_thread_open(&self, thread: &ThreadId, state: ThreadState) -> bool {
        state != ThreadState::Resolved
            || self.opened_threads.contains(thread)
            || self
                .composer
                .as_ref()
                .is_some_and(|composer| composer.target.reply_to.as_ref() == Some(thread))
    }

    /// Why the last change to `thread`'s state failed.
    pub fn thread_error(&self, thread: &ThreadId) -> Option<&Failure> {
        self.thread_error
            .as_ref()
            .filter(|(id, _)| id == thread)
            .map(|(_, failure)| failure)
    }

    pub(super) fn read_only(&self, cx: &gpui::App) -> bool {
        // The working tree or a file outside the repository read in a file view isn't the
        // review's text: no comment anchors on it (`file_view`).
        self.file_view_on_disk()
            || self
                .session
                .as_ref()
                .is_none_or(|session| session.read(cx).is_read_only())
    }

    /// What the stream shows: an interdiff or all changes.
    pub(super) fn comparison(&self, cx: &gpui::App) -> Comparison {
        self.session
            .as_ref()
            .map_or(Comparison::All, |session| session.read(cx).comparison())
    }

    /// `C`: opens the composer on the selected lines, else on the cursor's line; without a
    /// cursor, it is put on the first line in view first, as `↓` does. A composer that already
    /// holds text keeps it and takes the keyboard instead, wherever the cursor is (its file
    /// opens again if it collapsed).
    pub(super) fn comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only(cx) || self.keep_composer(window, cx) {
            return;
        }
        if self.selection.is_none() && self.cursor.is_none() {
            self.cursor = self.first_visible_cell();
        }
        let target = match (self.selection(), self.cursor()) {
            (Some(selection), _) => ComposerTarget {
                path: selection.path,
                side: selection.side,
                start_line: selection.start_line,
                end_line: selection.end_line,
                reply_to: None,
            },
            (None, Some(cursor)) => ComposerTarget {
                path: cursor.path,
                side: cursor.side,
                start_line: cursor.line,
                end_line: cursor.line,
                reply_to: None,
            },
            (None, None) => return,
        };
        // An interdiff's old column is a round's snapshot, which no anchor names (DESIGN §5).
        if target.side == AnchorSide::Old && self.comparison(cx).is_interdiff() {
            return;
        }
        // A collapsed (viewed) file opens to show the composer.
        if self.is_collapsed(&target.path) {
            self.set_open(&target.path.clone(), true, cx);
        }
        self.open_composer(target, window, cx);
    }

    /// `R`: replies to the focused thread.
    pub(super) fn reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.focused_thread.clone() {
            self.start_reply(&thread, window, cx);
        }
    }

    /// Opens the reply composer in `thread`'s card (a click on its reply field, or `R`).
    pub fn start_reply(&mut self, thread: &ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only(cx) {
            return;
        }
        let Some(session) = self.session.clone() else {
            return;
        };
        let target = {
            let session = session.read(cx);
            let Some(found) = session.review().thread(thread) else {
                return;
            };
            let placement = session.placement(found);
            // Its file's path where the stream draws it there; in the section, the path it
            // was written on.
            let path = match placement.file().and_then(|path| session.file(path)) {
                _ if self.is_file_view() => match self.file_view_path() {
                    Some(path) => path.to_string(),
                    None => return,
                },
                Some(entry) => entry.path.clone(),
                None if placement.is_not_in_diff() => found.anchor.path.clone(),
                None => return,
            };
            let (start_line, end_line) = placement
                .lines(&found.anchor)
                .unwrap_or((found.anchor.start_line, found.anchor.end_line));
            ComposerTarget {
                path,
                side: found.anchor.side,
                start_line,
                end_line,
                reply_to: Some(thread.clone()),
            }
        };
        // Only into a card the stream has a block for: a composer in a card that isn't drawn
        // would hold the keyboard where nothing shows it.
        if !self
            .thread_block_path(thread, cx)
            .is_some_and(|path| self.index.contains_key(&path))
        {
            return;
        }
        self.focused_thread = Some(thread.clone());
        if self
            .composer
            .as_ref()
            .is_some_and(|composer| composer.target == target)
        {
            self.focus_composer(window, cx);
            return;
        }
        if self.keep_composer(window, cx) {
            return;
        }
        self.opened_threads.insert(thread.clone());
        self.open_composer(target, window, cx);
    }

    /// Reply (the comments list's menu): shows `thread`'s card and opens the reply composer in
    /// it, at once when the card is there, else once it is built (its file loading or
    /// collapsed, all changes being listed).
    pub fn reply_to_thread(
        &mut self,
        thread: &ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        self.reveal_thread(thread, cx);
        if self.thread_item(thread).is_some() {
            self.reply_when_shown = None;
            self.start_reply(thread, window, cx);
        } else {
            self.reply_when_shown = Some(thread.clone());
        }
    }

    /// Opens the reply [`DiffPane::reply_to_thread`] asked for once its card is built, or
    /// forgets it once its card can't come (its block built without it). Runs as the stream
    /// is drawn.
    pub(super) fn open_waiting_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.reply_when_shown.clone() else {
            return;
        };
        if self.thread_item(&thread).is_some() {
            self.reply_when_shown = None;
            cx.defer_in(window, move |this, window, cx| {
                this.start_reply(&thread, window, cx);
            });
        } else if !self.card_may_come(&thread, cx) {
            self.reply_when_shown = None;
        }
    }

    /// Whether `thread`'s card, not built now, may still be: all changes are being listed, the
    /// thread waits for its file's text, or its file is loading. Its block collapsed again, or
    /// built without it, it won't.
    fn card_may_come(&self, thread: &ThreadId, cx: &App) -> bool {
        let Some(session) = self.session.as_ref().map(|session| session.read(cx)) else {
            return false;
        };
        if session.asked_comparison() != session.comparison() {
            return true;
        }
        let Some(found) = session.review().thread(thread) else {
            return false;
        };
        if session.placement(found).is_pending() {
            return true;
        }
        let Some(&index) = self
            .thread_block_path(thread, cx)
            .and_then(|path| self.index.get(&path))
        else {
            return false;
        };
        self.blocks[index]
            .rows
            .iter()
            .any(|row| matches!(row, Row::Notice(rows::Notice::Loading)))
    }

    /// The block (by its key: a file's path, or the section's) that draws `target`'s composer:
    /// a new thread's file, or the block of the card a reply answers.
    fn target_block_path(&self, target: &ComposerTarget, cx: &App) -> Option<String> {
        match &target.reply_to {
            Some(thread) => self.thread_block_path(thread, cx),
            None => Some(target.path.clone()),
        }
    }

    /// A composer that holds text stays: it takes the keyboard and comes into view. `true`
    /// when there was one, shown or not: a draft whose file can't show it yet (a failed load)
    /// is kept for when it can, never replaced by a new composer. One the interdiff shown
    /// can't hold (its file isn't in it, or it is on the old side) switches the stream to all
    /// changes, and takes the keyboard once its row is there.
    pub(super) fn keep_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.has_draft(cx) {
            return false;
        }
        if self.composer_needs_all_changes(cx)
            && let Some(session) = self.session.clone()
        {
            self.focus_composer_after_listing = true;
            session.update(cx, |session, cx| {
                session.set_comparison(Comparison::All, cx)
            });
            return true;
        }
        self.focus_composer(window, cx);
        true
    }

    /// Whether the composer's draft waits for all changes: the stream shows an interdiff, and
    /// the draft's file isn't in it or the draft is on the old side; for a reply, the interdiff
    /// doesn't draw its thread.
    fn composer_needs_all_changes(&self, cx: &App) -> bool {
        let Some(composer) = &self.composer else {
            return false;
        };
        let target = &composer.target;
        if !self.comparison(cx).is_interdiff() {
            return false;
        }
        match &target.reply_to {
            Some(thread) => self.session.as_ref().is_some_and(|session| {
                let session = session.read(cx);
                session
                    .review()
                    .thread(thread)
                    .is_some_and(|found| session.placement(found).is_not_in_comparison())
            }),
            None => !self.index.contains_key(&target.path) || target.side == AnchorSide::Old,
        }
    }

    /// Gives a draft asked for while all changes were being listed the keyboard, once its row
    /// is there ([`DiffPane::keep_composer`]). Runs as the stream is drawn.
    pub(super) fn focus_waiting_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_composer_after_listing {
            return;
        }
        if self.composer.is_none() {
            self.focus_composer_after_listing = false;
            return;
        }
        if self.composer_item().is_some() {
            self.focus_composer_after_listing = false;
            cx.defer_in(window, |this, window, cx| {
                this.focus_composer(window, cx);
            });
        }
    }

    /// Brings the whole composer into view once its row has been laid out at its new height
    /// ([`DiffPane::focus_composer`], typing): a reply opened in a card near the viewport's
    /// bottom grows the card past it, and `reveal_item` saw the card's height from before.
    /// Runs as the stream is drawn, a frame after the composer opened or grew.
    pub(super) fn reveal_laid_out_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.reveal_composer_in == 0 {
            return;
        }
        self.reveal_composer_in -= 1;
        if self.reveal_composer_in > 0 {
            // The next frame lays the row out.
            cx.defer_in(window, |_, _, cx| cx.notify());
            return;
        }
        if let Some(item) = self.composer_item() {
            self.reveal_item_bottom(item, 1);
            self.viewport_changed(cx);
        }
    }

    /// Gives the composer the keyboard and brings it into view, opening its file first when it
    /// is collapsed. A composer still without a row (its file failed to load) never takes the
    /// keyboard, which a field that isn't drawn would swallow: the stream keeps it, and `false`
    /// says so.
    fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(composer) = &self.composer else {
            return false;
        };
        let path = self.target_block_path(&composer.target, cx);
        if self.composer_item().is_none()
            && let Some(path) = path
            && self.is_collapsed(&path)
        {
            self.set_open(&path, true, cx);
        }
        let shown = match (self.composer_item(), &self.composer) {
            (Some(item), Some(composer)) => {
                composer
                    .input
                    .update(cx, |input, cx| input.focus(window, cx));
                self.reveal_item(item, 1);
                // A reply's card was measured without it: its full height is known once the
                // list has laid it out again, and its end comes into view then.
                self.reveal_composer_in = 2;
                true
            }
            _ => {
                self.focus_handle.focus(window, cx);
                false
            }
        };
        self.viewport_changed(cx);
        cx.notify();
        shown
    }

    /// Gives list item `item` `focus` to carry (gpui keeps rendering an item whose handle has
    /// the keyboard), keeping the reading position where it is.
    fn set_item_focus(&mut self, item: usize, focus: Option<FocusHandle>) {
        let top = self.list.logical_scroll_top();
        self.list.splice_focusable(item..item + 1, [focus]);
        if top.item_ix == item {
            self.list.scroll_to(top);
        }
    }

    /// The focus handle list item `row` of `path` carries: the composer field's, on the row
    /// that holds the composer ([`holds_composer`]).
    pub(super) fn row_focus(&self, path: &str, row: &Row, cx: &App) -> Option<FocusHandle> {
        self.composer
            .as_ref()
            .filter(|composer| holds_composer(composer, path, row))
            .map(|composer| composer.input.focus_handle(cx))
    }

    /// Hands the composer's row its field's focus handle: after the list was reset, or when no
    /// rebuild did (a reply's card, which was there already).
    pub(super) fn attach_composer_focus(&mut self, cx: &App) {
        let handle = self
            .composer
            .as_ref()
            .map(|composer| composer.input.focus_handle(cx));
        if let (Some(item), Some(handle)) = (self.composer_item(), handle) {
            self.set_item_focus(item, Some(handle));
        }
    }

    /// Closes the composer for good ([`DiffPane::release_composer`]), saying so
    /// ([`DiffPaneEvent::ComposerClosed`]). The keyboard is the caller's to place.
    fn close_composer(&mut self, cx: &mut Context<Self>) -> Option<Composer> {
        let closed = self.release_composer(cx);
        if closed.is_some() {
            cx.emit(DiffPaneEvent::ComposerClosed);
        }
        closed
    }

    /// Takes the composer away: its row goes (a reply's card drops its handle) and the rows of
    /// the block that drew it are rebuilt.
    fn release_composer(&mut self, cx: &mut Context<Self>) -> Option<Composer> {
        let item = self
            .composer
            .as_ref()
            .filter(|composer| composer.target.reply_to.is_some())
            .and_then(|_| self.composer_item());
        let path = self
            .composer
            .as_ref()
            .and_then(|composer| self.target_block_path(&composer.target, cx));
        let composer = self.composer.take()?;
        if composer.target.reply_to.is_some()
            && let Some(session) = self.session.clone()
        {
            session.update(cx, |session, _| session.set_replying_to(None));
        }
        if let Some(item) = item {
            self.set_item_focus(item, None);
        }
        if let Some(path) = path {
            self.refresh_path(&path, cx);
        }
        Some(composer)
    }

    /// Hands the keyboard back to the stream when the composer's field holds it without a row
    /// to be drawn in (its file collapsed, failed to load or left the diff, its thread was
    /// deleted): every key would go nowhere otherwise. Runs as the stream is drawn.
    pub(super) fn release_orphaned_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let orphaned = self
            .composer_focus
            .as_ref()
            .is_some_and(|focus| focus.is_focused(window))
            && self.composer_item().is_none();
        if !orphaned || self.refocus_pending {
            return;
        }
        self.refocus_pending = true;
        cx.defer_in(window, |this, window, cx| {
            this.refocus_pending = false;
            let still = this
                .composer_focus
                .as_ref()
                .is_some_and(|focus| focus.is_focused(window))
                && this.composer_item().is_none();
            if still {
                this.focus_handle.focus(window, cx);
                cx.notify();
            }
        });
    }

    pub(super) fn open_composer(
        &mut self,
        target: ComposerTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A reply waiting for its card is superseded by any composer.
        self.reply_when_shown = None;
        // A reply to an annotation asks its agent about it (PLAN §19.3).
        let asking = target.reply_to.as_ref().and_then(|thread| {
            let session = self.session.as_ref()?.read(cx);
            session
                .review()
                .thread(thread)
                .filter(|thread| thread.state == ThreadState::Annotation)
                .map(|thread| thread.author.name.clone())
        });
        let (rows, placeholder) = match (&target.reply_to, asking) {
            (Some(_), Some(agent)) => (REPLY_ROWS, format!("Ask {agent} about this…")),
            (Some(_), None) => (REPLY_ROWS, "Reply… (Markdown)".to_string()),
            (None, _) => (COMPOSER_ROWS, "Leave a comment… (Markdown)".to_string()),
        };
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(rows.0, rows.1)
                .placeholder(placeholder)
        });
        let subscription = cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                // The field grows with its text: its row is measured again. Typed into while
                // scrolled out of view, it comes back into view, as an editor's caret does.
                if let Some(item) = this.composer_item() {
                    this.remeasure(item);
                    if this.is_off_screen(item) {
                        this.reveal_item(item, 1);
                        this.viewport_changed(cx);
                    }
                    // Grown past the viewport's bottom: brought back once laid out again.
                    this.reveal_composer_in = 2;
                }
                cx.notify();
            }
        });
        // The composer this one replaces (one without text, [`DiffPane::keep_composer`]) lets go
        // of its row first.
        self.release_composer(cx);
        // Its card stays drawn while it answers it (`ReviewSession::shows_card`).
        if let (Some(thread), Some(session)) = (&target.reply_to, self.session.clone()) {
            session.update(cx, |session, _| {
                session.set_replying_to(Some(thread.clone()))
            });
        }
        self.composer_focus = Some(input.focus_handle(cx));
        self.composer = Some(Composer {
            target: target.clone(),
            input,
            posting: false,
            error: None,
            _subscription: subscription,
        });
        if let Some(path) = self.target_block_path(&target, cx) {
            self.refresh_path(&path, cx);
        }
        // A reply's card was there already, so no rebuild gave it the field's handle.
        if target.reply_to.is_some() {
            self.attach_composer_focus(cx);
        }
        self.focus_composer(window, cx);
    }

    /// Whether item `item` is wholly out of the viewport.
    pub(super) fn is_off_screen(&self, item: usize) -> bool {
        let height = self.viewport_height();
        let size = self.item_height(item);
        self.offset_of(item, height * 2.)
            .is_none_or(|y| y + size <= 0. || y >= height)
    }

    /// Posts the composer's text: a new thread at its lines (`ops::add_thread`, the anchor from
    /// the loaded lines) or on the whole file (a file-level anchor), or a reply (`ops::reply`). On success the composer closes, the thread
    /// is focused and the keyboard goes back to the stream; a failure stays in the composer.
    pub fn post_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        // In the file view, a new thread anchors on the text it shows (PLAN §18.1).
        let file_text = self.file_view_text();
        let Some(composer) = self.composer.as_mut() else {
            return;
        };
        if composer.posting {
            return;
        }
        let body = composer.input.read(cx).value().trim().to_string();
        if body.is_empty() {
            composer.error = Some(Failure::new("Write something first"));
            cx.notify();
            return;
        }
        let target = composer.target.clone();
        let posting: gpui::Task<Result<ThreadId, Failure>> = match &target.reply_to {
            Some(thread) => {
                let reply =
                    session.update(cx, |session, cx| session.reply(thread.clone(), body, cx));
                let thread = thread.clone();
                cx.background_spawn(async move { reply.await.map(|_| thread) })
            }
            None => {
                let anchor = if target.is_file() {
                    session.read(cx).file_anchor_for(&target.path, target.side)
                } else if let Some(text) = file_text.as_ref().and_then(|diff| diff.as_text()) {
                    session.read(cx).anchor_in_text(
                        &target.path,
                        target.side,
                        text,
                        target.start_line,
                        target.end_line,
                    )
                } else {
                    session.read(cx).anchor_for(
                        &target.path,
                        target.side,
                        target.start_line,
                        target.end_line,
                    )
                };
                match anchor {
                    Ok(anchor) => {
                        session.update(cx, |session, cx| session.add_thread(anchor, body, cx))
                    }
                    Err(failure) => {
                        composer.error = Some(failure);
                        cx.notify();
                        return;
                    }
                }
            }
        };
        composer.posting = true;
        composer.error = None;
        self.writes += 1;
        cx.spawn_in(window, async move |this, cx| {
            let result = posting.await;
            this.update_in(cx, |this, window, cx| {
                this.writes -= 1;
                this.posted(target, result, window, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(super) fn posted(
        &mut self,
        target: ComposerTarget,
        result: Result<ThreadId, Failure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .composer
            .as_mut()
            .filter(|composer| composer.target == target);
        match (result, current) {
            (Ok(thread), current) => {
                if current.is_some() {
                    self.close_composer(cx);
                    self.focus_handle.focus(window, cx);
                }
                if target.reply_to.is_none() {
                    self.selection = None;
                }
                self.focused_thread = Some(thread.clone());
                self.pending_thread = Some(thread);
                // The review's reload can draw the card before this answer comes (a loaded
                // machine), leaving no change to its rows to reveal it on: it is revealed now
                // if it is there, else once it is drawn.
                self.reveal_pending_thread();
                self.viewport_changed(cx);
                if let Some(path) = self.target_block_path(&target, cx) {
                    self.refresh_path(&path, cx);
                }
            }
            (Err(failure), Some(composer)) => {
                composer.posting = false;
                composer.error = Some(failure);
            }
            (Err(failure), None) => log::warn!("a comment wasn't posted: {failure}"),
        }
        cx.notify();
    }

    /// Discards the composer (`Esc` in it) and hands the keyboard back to the stream.
    pub fn cancel_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reply_when_shown = None;
        if self.close_composer(cx).is_none() {
            return;
        }
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Drops an empty composer without touching the keyboard (`Esc` in the stream). A composer
    /// holding a draft stays: only `Esc` in the composer itself discards one (DESIGN §5), so a
    /// press in the code to quote it, `Ctrl C` and `Esc` never lose it. `false` when nothing
    /// was dropped.
    pub(super) fn drop_composer(&mut self, cx: &mut Context<Self>) -> bool {
        if self.has_draft(cx) {
            return false;
        }
        self.close_composer(cx).is_some()
    }

    /// `]` / `[`: focuses the next (previous) thread that waits on someone (open or
    /// addressed: not resolved, nor an annotation, PLAN §19.3), scrolling to it, on into later
    /// (earlier) files.
    pub(super) fn step_thread(&mut self, dir: Dir, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let unresolved: Vec<(usize, ThreadId)> = {
            let review = session.read(cx).review();
            self.thread_items()
                .into_iter()
                .filter(|(_, id)| {
                    review
                        .thread(id)
                        .is_some_and(|thread| thread.state.is_waiting())
                })
                .collect()
        };
        let focused = self
            .focused_thread
            .as_ref()
            .and_then(|id| self.thread_item(id));
        let from = focused
            .or_else(|| self.cursor_item())
            .unwrap_or(self.list.logical_scroll_top().item_ix);
        let next = match dir {
            Dir::Down => unresolved.iter().find(|(item, _)| {
                if focused.is_some() {
                    *item > from
                } else {
                    *item >= from
                }
            }),
            Dir::Up => unresolved.iter().rev().find(|(item, _)| *item < from),
        };
        if let Some((item, id)) = next.cloned() {
            self.focus_item(item, id, cx);
            return;
        }
        // The next thread may live in a file (or the section) whose cards aren't built:
        // collapsed, or loading.
        let Some((from_block, _)) = self.locate(from) else {
            return;
        };
        let order: Vec<usize> = match dir {
            Dir::Down => (from_block + 1..self.blocks.len()).collect(),
            Dir::Up => (0..from_block).rev().collect(),
        };
        for index in order {
            let waiting: Vec<ThreadId> = self
                .block_threads(index, session.read(cx))
                .into_iter()
                .filter(|thread| thread.state.is_waiting())
                .map(|thread| thread.id.clone())
                .collect();
            if waiting.is_empty() {
                continue;
            }
            let pick = match dir {
                Dir::Down => waiting.first(),
                Dir::Up => waiting.last(),
            };
            if let Some(id) = pick.cloned() {
                self.reveal_thread(&id, cx);
            }
            return;
        }
    }

    /// Focuses `thread` and scrolls to its card (a click on its row in the comments list).
    /// A thread whose card isn't built yet — its file collapsed or still loading, or the
    /// comparison asked for still being listed — is shown once it is.
    pub fn reveal_thread(&mut self, thread: &ThreadId, cx: &mut Context<Self>) {
        // Showing another thread forgets a reply waiting for its card.
        if self.reply_when_shown.as_ref() != Some(thread) {
            self.reply_when_shown = None;
        }
        let Some(session) = self.session.clone() else {
            return;
        };
        self.revealing(cx);
        if let Some(item) = self.thread_item(thread) {
            self.focus_item(item, thread.clone(), cx);
            return;
        }
        let path = self.thread_block_path(thread, cx);
        let switching = {
            let session = session.read(cx);
            session.asked_comparison() != session.comparison()
        };
        let Some(path) = path else {
            // The comparison being listed shows it once it lands (or nothing draws it).
            self.focused_thread = Some(thread.clone());
            self.reveal_after_listing = switching.then(|| thread.clone());
            cx.notify();
            return;
        };
        self.focused_thread = Some(thread.clone());
        self.pending_thread = Some(thread.clone());
        if self.is_collapsed(&path) {
            self.set_open(&path, true, cx);
        }
        if self.thread_item(thread).is_none()
            && let Some(&index) = self.index.get(&path)
        {
            self.go_to_file(index, cx);
        }
        self.reveal_pending_thread();
        self.viewport_changed(cx);
        cx.notify();
    }

    /// A click on a card: it becomes the focused thread.
    pub fn focus_thread(&mut self, thread: &ThreadId, cx: &mut Context<Self>) {
        if self.focused_thread.as_ref() != Some(thread) {
            self.focused_thread = Some(thread.clone());
            cx.notify();
        }
    }

    fn focus_item(&mut self, item: usize, thread: ThreadId, cx: &mut Context<Self>) {
        self.focused_thread = Some(thread);
        self.pending_thread = None;
        self.menu = None;
        self.reveal_item(item, 2);
        self.viewport_changed(cx);
        cx.notify();
    }

    /// `X`: resolves the focused thread, or reopens it when it is resolved; nothing on an
    /// annotation.
    pub(super) fn resolve_focused(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.focused_thread.clone() else {
            return;
        };
        let state = self.session.as_ref().and_then(|session| {
            session
                .read(cx)
                .review()
                .thread(&thread)
                .map(|found| found.state)
        });
        match state {
            Some(ThreadState::Resolved) => self.set_thread_state(&thread, ThreadState::Open, cx),
            Some(ThreadState::Open | ThreadState::Addressed) => {
                self.set_thread_state(&thread, ThreadState::Resolved, cx)
            }
            // An annotation's state doesn't change (PLAN §19.1): `R` asks about it.
            Some(ThreadState::Annotation) | None => {}
        }
    }

    /// Resolves or reopens `thread` as the reviewer (`ops::set_thread_state`); nothing on a
    /// finished review. A resolved thread collapses.
    pub fn set_thread_state(
        &mut self,
        thread: &ThreadId,
        state: ThreadState,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }
        let Some(session) = self.session.clone() else {
            return;
        };
        if state == ThreadState::Resolved {
            self.opened_threads.remove(thread);
        }
        self.thread_error = None;
        let writing = session.update(cx, |session, cx| {
            session.set_thread_state(thread.clone(), state, None, cx)
        });
        self.writes += 1;
        let thread = thread.clone();
        cx.spawn(async move |this, cx| {
            let result = writing.await;
            this.update(cx, |this, cx| {
                this.writes -= 1;
                if let Err(failure) = result {
                    this.thread_error = Some((thread, failure));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Opens or collapses a resolved thread's card (its chevron).
    pub fn toggle_thread(&mut self, thread: &ThreadId, cx: &mut Context<Self>) {
        if !self.opened_threads.remove(thread) {
            self.opened_threads.insert(thread.clone());
        }
        if let Some(item) = self.thread_item(thread) {
            self.remeasure(item);
        }
        cx.notify();
    }

    /// Every thread card in the stream, as `(list item, thread)`, top to bottom.
    fn thread_items(&self) -> Vec<(usize, ThreadId)> {
        self.blocks
            .iter()
            .enumerate()
            .flat_map(|(block, rows)| {
                let start = self.starts[block];
                rows.rows.iter().enumerate().filter_map(move |(row, item)| {
                    item.thread().map(|id| (start + row, id.clone()))
                })
            })
            .collect()
    }

    pub(super) fn thread_item(&self, thread: &ThreadId) -> Option<usize> {
        self.thread_items()
            .into_iter()
            .find(|(_, id)| id == thread)
            .map(|(item, _)| item)
    }

    /// The block (by its key) and the card (the thread it answers, for a reply) of the
    /// composer's row (tests).
    #[cfg(test)]
    pub(crate) fn composer_card(&self) -> Option<(&str, Option<&ThreadId>)> {
        let (block, row) = self.locate(self.composer_item()?)?;
        let block = &self.blocks[block];
        Some((block.path.as_str(), block.rows[row].thread()))
    }

    /// The composer's list item: its own row, or the card of the thread it answers.
    pub(super) fn composer_item(&self) -> Option<usize> {
        let composer = self.composer.as_ref()?;
        if let Some(thread) = &composer.target.reply_to {
            return self.thread_item(thread);
        }
        // Only its own file's rows can hold a new thread's.
        let block = *self.index.get(&composer.target.path)?;
        self.blocks[block]
            .rows
            .iter()
            .position(|row| holds_composer(composer, &composer.target.path, row))
            .map(|row| self.starts[block] + row)
    }

    /// Scrolls to the thread `]`/`[` or the comments list went to, once its card is built.
    pub(super) fn reveal_pending_thread(&mut self) {
        let Some(thread) = self.pending_thread.clone() else {
            return;
        };
        if let Some(item) = self.thread_item(&thread) {
            self.pending_thread = None;
            self.reveal_item(item, 2);
        }
    }

    /// Whether `path`'s rows are its header alone.
    pub(super) fn is_collapsed(&self, path: &str) -> bool {
        self.index.get(path).is_some_and(|&block| {
            matches!(
                self.blocks[block].rows.first(),
                Some(Row::Header {
                    collapsed: true,
                    ..
                })
            )
        })
    }

    /// Rebuilds `path`'s rows (the composer moved, or a thread's card changed size).
    fn refresh_path(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(&index) = self.index.get(path) else {
            return;
        };
        let changed = self.refresh_block(index, cx);
        self.remeasure_cards(index);
        if changed {
            self.after_rows_changed(cx);
        } else {
            cx.notify();
        }
    }

    /// Measures block `index`'s cards and composer again: their content can change without
    /// their rows changing.
    pub(super) fn remeasure_cards(&mut self, index: usize) {
        let start = self.starts[index];
        let rows: Vec<usize> = self.blocks[index]
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| !row.has_fixed_height())
            .map(|(row, _)| start + row)
            .collect();
        for item in rows {
            self.remeasure(item);
        }
    }

    /// Forgets the focused, pending and opened threads that no longer exist (deleted), and
    /// drops a reply to one.
    pub(super) fn forget_gone_threads(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let review = session.read(cx).review();
        let exists = |id: &ThreadId| review.thread(id).is_some_and(|thread| !thread.is_deleted());
        if self.focused_thread.as_ref().is_some_and(|id| !exists(id)) {
            self.focused_thread = None;
        }
        if self.pending_thread.as_ref().is_some_and(|id| !exists(id)) {
            self.pending_thread = None;
        }
        if self
            .reveal_after_listing
            .as_ref()
            .is_some_and(|id| !exists(id))
        {
            self.reveal_after_listing = None;
        }
        if self.reply_when_shown.as_ref().is_some_and(|id| !exists(id)) {
            self.reply_when_shown = None;
        }
        self.opened_threads.retain(exists);
        let reply_gone = self
            .composer
            .as_ref()
            .and_then(|composer| composer.target.reply_to.as_ref())
            .is_some_and(|thread| !exists(thread));
        if reply_gone {
            self.close_composer(cx);
        }
    }
}
