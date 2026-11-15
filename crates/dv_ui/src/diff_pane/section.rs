//! The stream's "Not in the diff" section (DESIGN §4.1, PLAN §8): a thread whose file left the
//! review's list (`Placement::Outdated` without a file: the agent deleted the file, reverted it,
//! or renamed one git can't pair with its old name) has no file to head, so its card heads the
//! stream instead, before the first file, and can still be answered and resolved there.
//!
//! The section is a block of the stream like a file's, keyed [`SECTION`] (no path git lists is
//! empty): a header bar in the file header's style ("Not in the diff" and the count, a chevron
//! that collapses it), then one card per thread in the order they were added, each an outdated
//! card naming the path it was written on. It shows whichever comparison is shown, and is
//! absent while no thread needs it; it comes and goes with the threads (a refresh rebuilds the
//! stream when it does). With no file listed it keeps the stream from the main area's empty
//! state, whose words (and an empty interdiff's Show all changes) it ends with instead. It isn't a file: it is never the current file, and the keys and
//! buttons that step through files (`J`/`K`, the headers' arrows, the footer) pass over it,
//! while the thread keys (`]`/`[`, `R`, `X`) and the comments list reach its cards as any other.
//!
//! On a PR review it also holds GitHub's threads that have no place in the diff (DESIGN §4.5):
//! those GitHub calls outdated, made on another commit than the one the review shows, and
//! those whose code can't be read here, after the review's own.

use dv_core::github::Standing;
use dv_core::review::{ThreadId, ThreadKey};
use gpui::{App, Context, ListOffset, px};

use super::rows::{InsertItem, Row};
use super::{Block, DiffPane};
use crate::session::{Card, ReviewSession};
use crate::shell::EmptyState;

/// The section's key among the stream's blocks: no path git lists is empty.
pub const SECTION: &str = "";

/// What `state.stream` shows of the section: whether it is collapsed, its threads in the
/// order they were added (all of them, collapsed or not), GitHub's threads in it by node id,
/// which follow them, and the empty state it ends with while no file is listed
/// ([`EmptyState::key`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionSummary {
    pub collapsed: bool,
    pub threads: Vec<ThreadId>,
    pub github: Vec<String>,
    pub empty: Option<&'static str>,
}

/// What the section's bar says of its cards after their count (DESIGN §4.1, §4.5): "comments
/// on files the review doesn't change now" for `unchanged` threads whose file left the
/// review's list, "outdated on GitHub" for `outdated` threads GitHub calls outdated, made on
/// a commit the review doesn't show, and "on GitHub, can't be placed here" for `unplaced`
/// ones dv can't tell the code of; each with its own count when the section holds several
/// kinds.
pub(super) fn section_words(unchanged: usize, outdated: usize, unplaced: usize) -> String {
    let comments = |count: usize| if count == 1 { "comment" } else { "comments" };
    let parts: Vec<(usize, String)> = [
        (
            unchanged,
            format!(
                "{} on files the review doesn't change now",
                comments(unchanged)
            ),
        ),
        (outdated, "outdated on GitHub".to_string()),
        (unplaced, "on GitHub, can't be placed here".to_string()),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .collect();
    match parts.as_slice() {
        [(_, words)] => words.clone(),
        several => several
            .iter()
            .map(|(count, words)| format!("{count} {words}"))
            .collect::<Vec<_>>()
            .join(" · "),
    }
}

impl DiffPane {
    /// [`section_words`] for the cards the section holds now.
    pub(super) fn section_subtitle(&self, cx: &App) -> String {
        let (mut unchanged, mut outdated, mut unplaced) = (0, 0, 0);
        if let Some(session) = &self.session {
            for card in session.read(cx).cards_not_in_diff() {
                match card.github().map(|shown| shown.standing) {
                    Some(Standing::OutdatedOnGithub) => outdated += 1,
                    Some(Standing::Unplaced) => unplaced += 1,
                    Some(Standing::Anchored) | None => unchanged += 1,
                }
            }
        }
        section_words(unchanged, outdated, unplaced)
    }

    /// Whether block `index` is the section.
    pub(super) fn is_section(&self, index: usize) -> bool {
        self.blocks
            .get(index)
            .is_some_and(|block| block.path == SECTION)
    }

    /// The first file's block: after the section when it shows.
    pub(super) fn first_file_block(&self) -> usize {
        usize::from(self.is_section(0))
    }

    /// The section's block, `None` while no thread needs it.
    pub(super) fn section_block(&self, cx: &App) -> Option<Block> {
        if self.is_file_view() {
            return None;
        }
        let session = self.session.as_ref()?.read(cx);
        let threads: Vec<ThreadKey> = session.cards_not_in_diff().map(Card::key).collect();
        if threads.is_empty() {
            return None;
        }
        let collapsed = self.section_collapsed();
        let mut rows = vec![Row::Header {
            collapsed,
            labels: false,
        }];
        if !collapsed {
            rows.extend(
                threads
                    .into_iter()
                    .map(|thread| Row::Heading(InsertItem::Thread(thread))),
            );
        }
        if EmptyState::of_session(session).is_some() {
            rows.push(Row::Empty);
        }
        Some(Block {
            path: SECTION.to_string(),
            rows,
        })
    }

    /// Whether the reviewer collapsed the section (its chevron, as a file's: `set_open`).
    fn section_collapsed(&self) -> bool {
        self.files
            .get(SECTION)
            .and_then(|ui| ui.open)
            .is_some_and(|open| !open)
    }

    /// Whether the section should come or go: the threads that need it changed, and every
    /// block moves.
    pub(super) fn section_comes_or_goes(&self, cx: &App) -> bool {
        let wanted = !self.is_file_view()
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.read(cx).cards_not_in_diff().next().is_some());
        wanted != self.is_section(0)
    }

    /// Brings the section into line with the threads that need it: the stream is rebuilt when
    /// it comes or goes, else its rows are. `true` when the rows changed.
    pub(super) fn refresh_section(&mut self, cx: &mut Context<Self>) -> bool {
        if self.section_comes_or_goes(cx) {
            self.rebuild_all(cx);
            return true;
        }
        self.is_section(0) && self.refresh_block(0, cx)
    }

    /// The threads whose cards block `index` draws, built or not (its header alone while
    /// collapsed): the review's own in the order they were added, then GitHub's.
    pub(super) fn block_threads<'a>(
        &self,
        index: usize,
        session: &'a ReviewSession,
    ) -> Vec<Card<'a>> {
        match self.blocks.get(index) {
            Some(_) if self.is_file_view() => self.file_view_threads(session),
            Some(block) if block.path == SECTION => session.cards_not_in_diff().collect(),
            Some(block) => session
                .cards()
                .filter(|&card| {
                    let placement = session.card_placement(card);
                    placement.file() == Some(block.path.as_str()) && session.shows(card, &placement)
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// The block `thread`'s card is drawn in, built or not: the section's for a thread whose
    /// file left the diff, else the listed file it shows in; `None` when the stream doesn't
    /// draw it (an interdiff that doesn't show it, a deleted thread).
    pub(super) fn thread_block_path(&self, thread: &ThreadKey, cx: &App) -> Option<String> {
        let session = self.session.as_ref()?.read(cx);
        // A deleted thread of the review's own still answers here: its card goes with it.
        let found = match thread {
            ThreadKey::Local(id) => Card::Local(session.review().thread(id)?),
            ThreadKey::Github(_) => session.card(thread)?,
        };
        if self.is_file_view() {
            return self
                .file_view_threads(session)
                .iter()
                .any(|shown| shown.key() == *thread)
                .then(|| self.file_view_path())
                .flatten()
                .map(str::to_string);
        }
        let placement = session.card_placement(found);
        if !session.shows(found, &placement) {
            return None;
        }
        if placement.is_not_in_diff() {
            return Some(SECTION.to_string());
        }
        session.file_of(found).map(|entry| entry.path.clone())
    }

    /// What `state.stream` shows of the section, `None` while it doesn't show.
    pub fn section_summary(&self, cx: &App) -> Option<SectionSummary> {
        let block = self.blocks.first().filter(|block| block.path == SECTION)?;
        let session = self.session.as_ref()?.read(cx);
        Some(SectionSummary {
            collapsed: matches!(
                block.rows.first(),
                Some(Row::Header {
                    collapsed: true,
                    ..
                })
            ),
            threads: session
                .threads_not_in_diff()
                .map(|thread| thread.id.clone())
                .collect(),
            github: session
                .cards_not_in_diff()
                .filter_map(Card::github)
                .map(|shown| shown.thread.id.clone())
                .collect(),
            empty: block
                .rows
                .contains(&Row::Empty)
                .then(|| EmptyState::of_session(session).map(EmptyState::key))
                .flatten(),
        })
    }

    /// Scrolls the section's header to the top (a thread shown in it whose card isn't built),
    /// leaving the cursor where it is: the section has no lines.
    pub(super) fn go_to_section(&mut self, cx: &mut Context<Self>) {
        if !self.is_section(0) {
            return;
        }
        self.list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
        self.menu = None;
        self.viewport_changed(cx);
        cx.notify();
    }
}
