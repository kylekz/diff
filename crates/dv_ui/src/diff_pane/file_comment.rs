//! Comments on a whole file in the stream (PLAN §8, DESIGN §4.1, §5): the cards heading a file
//! and how such a comment is started.
//!
//! A file's rows are headed, right under its header and above its first row or its notice (its
//! skeleton rows too: a comment on the whole file reads none of its text, so its card shows
//! before the file's diff lands), by its comments on the whole file in the order they were
//! added, then the composer writing a new one, then its outdated threads (which show the lines
//! they were written on, so they stand nearest the code). One comes from the header's `⋯` menu
//! ("Comment on file") or `Shift C` on the current file: the composer opens there, on the
//! file's new side, or its old side for a deleted file; an interdiff's old column is a round's
//! snapshot, so there a deleted file takes none, as `C` takes no comment on that column.

use dv_core::review::ThreadKey;
use gpui::{App, Context, Window};

use super::DiffPane;
use super::rows::InsertItem;
use crate::session::ReviewSession;
use crate::thread::ComposerTarget;

/// The cards heading `path`'s rows, in order (module docs): its comments on the whole file
/// placed so far, the composer when it writes one on `path`, then `outdated`, the outdated
/// threads whose cards head it (`threads::thread_rows`).
pub(super) fn heading_items(
    path: &str,
    session: &ReviewSession,
    composer: Option<&ComposerTarget>,
    outdated: Vec<ThreadKey>,
) -> Vec<InsertItem> {
    let mut items: Vec<InsertItem> = session
        .cards()
        .filter(|card| card.anchor().is_file_level())
        .filter(|&card| {
            let placement = session.card_placement(card);
            placement.file() == Some(path)
                && !placement.is_pending()
                && session.shows(card, &placement)
        })
        .map(|card| InsertItem::Thread(card.key()))
        .collect();
    if composer.is_some_and(|target| target.is_file() && target.path == path) {
        items.push(InsertItem::Composer);
    }
    items.extend(outdated.into_iter().map(InsertItem::Thread));
    items
}

impl DiffPane {
    /// "Comment on file" (the header's menu): opens the composer heading `path`'s rows, for a
    /// comment on the whole file, opening the file first when it is collapsed. A composer that
    /// holds a draft takes the keyboard instead, as for `C`; nothing on a finished review or
    /// where the file takes no such comment ([`DiffPane::can_comment_on_file`]).
    pub fn comment_on_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_comment_on_file(path, cx) || self.keep_composer(window, cx) {
            return;
        }
        let Some(side) = self
            .session
            .as_ref()
            .and_then(|session| session.read(cx).file_comment_side(path))
        else {
            return;
        };
        if self.is_collapsed(path) {
            self.set_open(path, true, cx);
        }
        self.open_composer(ComposerTarget::file(path.to_string(), side), window, cx);
    }

    /// `Shift C`: [`DiffPane::comment_on_file`] on the current file.
    pub(super) fn comment_on_current_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.current_file.clone() {
            self.comment_on_file(&path, window, cx);
        }
    }

    /// Whether `path` takes a comment on the whole file now: the review isn't finished and the
    /// file has a side one goes on (`ReviewSession::file_comment_side`). The menu item is
    /// dimmed and inert otherwise.
    pub(super) fn can_comment_on_file(&self, path: &str, cx: &App) -> bool {
        !self.read_only(cx)
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.read(cx).file_comment_side(path).is_some())
    }
}
