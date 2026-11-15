//! The stream's rows (PLAN §6 steps 5 and 7, DESIGN §4.1): what one file contributes to the one
//! list over every file, as plain data the view renders and the keys move over.
//!
//! A file is its header, then — unless it is collapsed (a viewed file, DESIGN §4.1) — either a
//! notice (loading, failed, binary, too large, a submodule, unmerged, no content change) or its
//! lines: code rows for the shown runs, a gap row for each hidden run (which also heads the run
//! after it with its `@@ -a,b +c,d @@`), and the next-file footer. Unified view shows one line per
//! row; split view pairs the i-th removed line of a change with the i-th added one
//! (`dv_core::diff::split_rows`). Rows are built for the view mode in use only, from lines
//! already loaded, so expanding a gap or switching modes never goes back to git.
//!
//! Thread cards and the composer go between rows ([`Insert`]): after the row that shows their
//! line on their side, or after the gap that hides it. A comment on the whole file, the composer
//! writing one, and a thread whose anchored text no longer stands at its lines (outdated, PLAN
//! §8) have no line to follow: their cards head the file's rows, right under the header
//! ([`Row::Heading`]), in that order.

use std::ops::Range;

use dv_core::diff::{
    DiffLine, FileDiff, HunkRange, LineKind, Segment, TextDiff, hunk_range, split_rows,
};
use dv_core::git::Oid;
use dv_core::review::{AnchorSide, ThreadKey};

use crate::services::ViewMode;

/// Unchanged lines kept around each change before the rest collapses into a gap, as git's own
/// default context.
pub const CONTEXT_LINES: usize = 3;

/// Row heights in logical pixels (DESIGN §1.5: code rows 20). The view draws each row at its
/// height, and the stream's scrolling works out positions from them without waiting for a
/// layout.
pub mod heights {
    /// The file header's bar (the board's 44px `codeHead` bar).
    pub const HEADER_BAR: f32 = 44.;
    /// The side labels under it (`main` / `working tree`).
    pub const SIDE_LABELS: f32 = 24.;
    /// A file header: its bar and its side labels.
    pub const HEADER: f32 = HEADER_BAR + SIDE_LABELS;
    pub const CODE: f32 = 20.;
    pub const GAP: f32 = 28.;
    pub const NOTICE: f32 = 48.;
    pub const LOADING: f32 = 70.;
    /// A failed load's banner with its detail closed; open, it is measured.
    pub const FAILED: f32 = 64.;
    pub const FOOTER: f32 = 52.;
    /// A thread card or the composer before it is measured.
    pub const INSERT: f32 = 120.;
    /// No measured row is shorter (the smallest, a resolved card collapsed to its 32px header,
    /// takes 50 with its padding): the stream's spacer counts a row not measured yet at this,
    /// so it is never too short for the last file's header to reach the top.
    pub const MEASURED_MIN: f32 = 40.;
}

/// What a file shows in place of its lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// Its diff is loading (skeleton rows, DESIGN §4.4).
    Loading,
    /// Its diff didn't load; the session holds why.
    Failed,
    /// git treats it as binary; sizes in bytes.
    Binary {
        old_len: Option<u64>,
        new_len: Option<u64>,
    },
    /// A side is over the size caps ("Load anyway").
    TooLarge {
        old_len: Option<u64>,
        new_len: Option<u64>,
    },
    /// A submodule's commit changed.
    Submodule {
        old_oid: Option<Oid>,
        new_oid: Option<Oid>,
        dirty: bool,
    },
    /// Unresolved conflicts on an index side.
    Unmerged,
    /// The text didn't change: a mode change, a rename without edits, an empty file.
    NoContentChange,
}

impl Notice {
    /// `"loading"`, `"failed"`, `"binary"`, `"too_large"`, `"submodule"`, `"unmerged"` or
    /// `"no_content_change"`, as automation state spells it.
    pub fn key(&self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Failed => "failed",
            Self::Binary { .. } => "binary",
            Self::TooLarge { .. } => "too_large",
            Self::Submodule { .. } => "submodule",
            Self::Unmerged => "unmerged",
            Self::NoContentChange => "no_content_change",
        }
    }
}

/// A gap row: hidden unchanged lines, and the hunk that follows them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gap {
    /// The gap's index in the file's segments, which `expand_gap` takes.
    pub segment: usize,
    /// Which gap of the file it is, from the top (its automation target,
    /// `stream/gap/<path>/<ordinal>`).
    pub ordinal: usize,
    /// The hidden lines, as indexes into the diff's lines.
    pub lines: Range<usize>,
    /// The `@@ -a,b +c,d @@` of the shown run after the gap; `None` at the end of the file.
    pub hunk: Option<HunkRange>,
    /// Lines are shown above it, so it can reveal lines from its top.
    pub above: bool,
    /// Lines are shown below it, so it can reveal lines from its bottom.
    pub below: bool,
}

/// What goes between rows: a thread card (one of the review's own threads or one of GitHub's,
/// PLAN §9.6) or the composer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum InsertItem {
    Thread(ThreadKey),
    Composer,
}

/// An [`InsertItem`] and the line it follows: `line` (an index into the diff's lines) on `side`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Insert {
    pub line: usize,
    pub side: AnchorSide,
    pub item: InsertItem,
}

/// One row of the stream within a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// The file's header: status, path, totals and controls, then, with `labels`, the side
    /// labels (`main` / `working tree`) over its lines. A `collapsed` file is its header alone;
    /// a file that shows a notice instead of lines has no side labels.
    Header {
        collapsed: bool,
        labels: bool,
    },
    /// Unified view: one line, an index into the diff's lines.
    Line(usize),
    /// Split view: the old and the new column, each an index into the diff's lines or empty.
    Pair {
        old: Option<usize>,
        new: Option<usize>,
    },
    Gap(Gap),
    Notice(Notice),
    Insert(Insert),
    /// A card heading the file, under the header (see the module docs), or the "Not in the
    /// diff" section: a thread's, or the composer of a comment on the whole file.
    Heading(InsertItem),
    /// Under the "Not in the diff" section while no file is listed: the main area's empty
    /// state as a row (its words, and an empty interdiff's Show all changes, DESIGN §4.1).
    Empty,
    /// "Next: `path`" and "Mark viewed and go next".
    Footer,
}

impl Row {
    /// Whether it shows lines of code.
    pub fn is_code(&self) -> bool {
        matches!(self, Self::Line(_) | Self::Pair { .. })
    }

    /// The line this row shows on `side`: for a unified row, its line when that line exists
    /// on `side` (a context line exists on both); for a split row, that column's line.
    pub fn cell(&self, lines: &[DiffLine], side: AnchorSide) -> Option<usize> {
        match *self {
            Self::Line(line) => lines.get(line).filter(|l| on_side(l, side)).map(|_| line),
            Self::Pair { old, new } => match side {
                AnchorSide::Old => old,
                AnchorSide::New => new,
            },
            _ => None,
        }
    }

    /// Whether it shows a removed or an added line.
    pub fn has_change(&self, lines: &[DiffLine]) -> bool {
        let changed = |line: Option<usize>| {
            line.and_then(|line| lines.get(line))
                .is_some_and(|line| line.kind.is_change())
        };
        match *self {
            Self::Line(line) => changed(Some(line)),
            Self::Pair { old, new } => changed(old) || changed(new),
            _ => false,
        }
    }

    /// Its height in logical pixels ([`heights`]); an estimate for the rows that are measured
    /// (an opened failure banner, thread cards).
    pub fn height(&self) -> f32 {
        match self {
            Self::Header { labels: false, .. } => heights::HEADER_BAR,
            Self::Header { labels: true, .. } => heights::HEADER,
            Self::Line(_) | Self::Pair { .. } => heights::CODE,
            Self::Gap(_) => heights::GAP,
            Self::Notice(Notice::Loading) => heights::LOADING,
            Self::Notice(Notice::Failed) => heights::FAILED,
            Self::Notice(_) | Self::Empty => heights::NOTICE,
            Self::Insert(_) | Self::Heading(_) => heights::INSERT,
            Self::Footer => heights::FOOTER,
        }
    }

    /// Whether [`Row::height`] is exact.
    pub fn has_fixed_height(&self) -> bool {
        !matches!(
            self,
            Self::Notice(Notice::Failed) | Self::Insert(_) | Self::Heading(_)
        )
    }

    /// The thread whose card this row is, if it is one.
    pub fn thread(&self) -> Option<&ThreadKey> {
        match self {
            Self::Insert(Insert {
                item: InsertItem::Thread(id),
                ..
            })
            | Self::Heading(InsertItem::Thread(id)) => Some(id),
            _ => None,
        }
    }

    /// What the row stands for, independent of the view mode and of where it sits: the stream
    /// keeps the reading position on it when rows are rebuilt.
    pub fn key(&self) -> RowKey {
        match self {
            Self::Header { .. } => RowKey::Header,
            Self::Line(line) => RowKey::Line(*line),
            Self::Pair { old, new } => RowKey::Line(new.or(*old).unwrap_or_default()),
            Self::Gap(gap) => RowKey::Gap(gap.lines.start),
            Self::Notice(_) | Self::Empty => RowKey::Notice,
            Self::Insert(insert) => RowKey::Insert(insert.item.clone()),
            Self::Heading(item) => RowKey::Insert(item.clone()),
            Self::Footer => RowKey::Footer,
        }
    }
}

/// See [`Row::key`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    Header,
    /// A code row, by a line it shows (the new column's in split view).
    Line(usize),
    /// A gap, by its first hidden line.
    Gap(usize),
    Notice,
    Insert(InsertItem),
    Footer,
}

/// Where a file's diff stands, for its rows.
#[derive(Clone, Copy, Debug)]
pub enum Content<'a> {
    Loading,
    Failed,
    Loaded(&'a FileDiff),
}

/// What one file's rows are built from.
#[derive(Clone, Copy, Debug)]
pub struct RowsSpec<'a> {
    pub mode: ViewMode,
    /// Only the header shows (a viewed file, DESIGN §4.1).
    pub collapsed: bool,
    pub content: Content<'a>,
    /// The text's shown runs and gaps (`dv_core::diff::segments`, then any expansions).
    pub segments: &'a [Segment],
    pub inserts: &'a [Insert],
    /// The cards heading the file's rows, in order (module docs).
    pub heading: &'a [InsertItem],
    /// Whether the file ends with the next-file footer.
    pub footer: bool,
    /// The file view's one file whole (PLAN §18.1): its lines show whether or not they changed
    /// (an empty file says so), under a header without side labels.
    pub whole: bool,
}

/// One file's rows (see the module docs).
pub fn file_rows(spec: &RowsSpec) -> Vec<Row> {
    let labels = !spec.collapsed
        && !spec.whole
        && matches!(spec.content, Content::Loaded(FileDiff::Text(text)) if text.has_changes());
    let mut rows = vec![Row::Header {
        collapsed: spec.collapsed,
        labels,
    }];
    if spec.collapsed {
        return rows;
    }
    rows.extend(spec.heading.iter().cloned().map(Row::Heading));
    let diff = match spec.content {
        Content::Loading => {
            rows.push(Row::Notice(Notice::Loading));
            return rows;
        }
        Content::Failed => {
            rows.push(Row::Notice(Notice::Failed));
            return rows;
        }
        Content::Loaded(diff) => diff,
    };
    let notice = match diff {
        // Renamed or its mode changed, its content as it was: the diff holds the text as
        // context (for its threads, PLAN §8), the stream says what happened.
        FileDiff::Text(text) if spec.whole && text.lines.is_empty() => {
            Some(Notice::NoContentChange)
        }
        FileDiff::Text(_) if spec.whole => None,
        FileDiff::Text(text) if !text.has_changes() => Some(Notice::NoContentChange),
        FileDiff::Text(_) => None,
        FileDiff::Binary { old_len, new_len } => Some(Notice::Binary {
            old_len: *old_len,
            new_len: *new_len,
        }),
        FileDiff::TooLarge { old_len, new_len } => Some(Notice::TooLarge {
            old_len: *old_len,
            new_len: *new_len,
        }),
        FileDiff::Submodule {
            old_oid,
            new_oid,
            dirty,
        } => Some(Notice::Submodule {
            old_oid: old_oid.clone(),
            new_oid: new_oid.clone(),
            dirty: *dirty,
        }),
        FileDiff::Unmerged => Some(Notice::Unmerged),
    };
    match (notice, diff.as_text()) {
        (Some(notice), _) => rows.push(Row::Notice(notice)),
        (None, Some(text)) => text_rows(text, spec, &mut rows),
        (None, None) => {}
    }
    if spec.footer {
        rows.push(Row::Footer);
    }
    rows
}

/// The code and gap rows of `text`, with the inserts after the rows they follow.
fn text_rows(text: &TextDiff, spec: &RowsSpec, rows: &mut Vec<Row>) {
    let lines = &text.lines;
    let mut ordinal = 0;
    for (index, segment) in spec.segments.iter().enumerate() {
        match segment {
            Segment::Gap {
                lines: hidden,
                expandable_above,
                expandable_below,
            } => {
                let next = spec.segments.get(index + 1).filter(|next| !next.is_gap());
                rows.push(Row::Gap(Gap {
                    segment: index,
                    ordinal,
                    lines: hidden.clone(),
                    hunk: next.map(|next| hunk_range(lines, next.range())),
                    above: *expandable_above,
                    below: *expandable_below,
                }));
                ordinal += 1;
                // Inserts on hidden lines follow the gap that hides them.
                push_inserts(rows, spec.inserts, |insert| hidden.contains(&insert.line));
            }
            Segment::Lines(range) => match spec.mode {
                ViewMode::Unified => {
                    for line in range.clone() {
                        rows.push(Row::Line(line));
                        push_inserts(rows, spec.inserts, |insert| {
                            insert.line == line
                                && lines.get(line).is_some_and(|l| on_side(l, insert.side))
                        });
                    }
                }
                ViewMode::Split => {
                    for pair in split_rows(lines, range.clone()) {
                        rows.push(Row::Pair {
                            old: pair.old,
                            new: pair.new,
                        });
                        push_inserts(rows, spec.inserts, |insert| match insert.side {
                            AnchorSide::Old => pair.old == Some(insert.line),
                            AnchorSide::New => pair.new == Some(insert.line),
                        });
                    }
                }
            },
        }
    }
}

fn push_inserts(rows: &mut Vec<Row>, inserts: &[Insert], follows: impl Fn(&Insert) -> bool) {
    rows.extend(
        inserts
            .iter()
            .filter(|insert| follows(insert))
            .cloned()
            .map(Row::Insert),
    );
}

/// Whether `line` exists on `side` (has a number there).
pub fn on_side(line: &DiffLine, side: AnchorSide) -> bool {
    side_number(line, side).is_some()
}

/// `line`'s number on `side`.
pub fn side_number(line: &DiffLine, side: AnchorSide) -> Option<u32> {
    match side {
        AnchorSide::Old => line.old_no,
        AnchorSide::New => line.new_no,
    }
}

/// The side a unified row reads `line` on: removed lines on the old side, the rest on the new.
pub fn natural_side(line: &DiffLine) -> AnchorSide {
    if line.kind == LineKind::Removed {
        AnchorSide::Old
    } else {
        AnchorSide::New
    }
}

/// A line of a file's text diff and the side it is read on: where the cursor sits, and the ends
/// of a selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinePos {
    /// An index into the diff's lines.
    pub line: usize,
    pub side: AnchorSide,
}

/// Up or down the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
}

/// Where the cursor can sit on `row`: its line on `side` when it has one there (split view
/// keeps to its column); a unified row without a side constraint reads its line on its
/// natural side.
pub fn cell_pos(
    row: &Row,
    lines: &[DiffLine],
    mode: ViewMode,
    side: Option<AnchorSide>,
) -> Option<LinePos> {
    match (mode, row, side) {
        (ViewMode::Unified, Row::Line(line), None) => lines.get(*line).map(|l| LinePos {
            line: *line,
            side: natural_side(l),
        }),
        (_, _, Some(side)) => row.cell(lines, side).map(|line| LinePos { line, side }),
        _ => None,
    }
}

/// The row that shows `pos`: in unified view the row of its line, in split view the row whose
/// `pos.side` column holds it.
pub fn find_row(rows: &[Row], lines: &[DiffLine], mode: ViewMode, pos: LinePos) -> Option<usize> {
    rows.iter().position(|row| match (mode, row) {
        (ViewMode::Unified, Row::Line(line)) => *line == pos.line,
        (ViewMode::Split, Row::Pair { .. }) => row.cell(lines, pos.side) == Some(pos.line),
        _ => false,
    })
}

/// The next place for the cursor from row `from` (exclusive) in `dir` within one file: the
/// next code row (unified, `side` `None`), or the next row with a line on `side`.
pub fn step(
    rows: &[Row],
    lines: &[DiffLine],
    mode: ViewMode,
    from: usize,
    dir: Dir,
    side: Option<AnchorSide>,
) -> Option<(usize, LinePos)> {
    let found = |index: usize| cell_pos(&rows[index], lines, mode, side).map(|pos| (index, pos));
    match dir {
        Dir::Down => (from + 1..rows.len()).find_map(found),
        Dir::Up => (0..from.min(rows.len())).rev().find_map(found),
    }
}

/// The first (`Dir::Down`) or last (`Dir::Up`) place for the cursor in a file, on `side` when
/// given, else (split view) on whichever side has a line.
pub fn end_cell(
    rows: &[Row],
    lines: &[DiffLine],
    mode: ViewMode,
    dir: Dir,
    side: Option<AnchorSide>,
) -> Option<(usize, LinePos)> {
    let scan = |side: Option<AnchorSide>| {
        let found =
            |index: usize| cell_pos(&rows[index], lines, mode, side).map(|pos| (index, pos));
        match dir {
            Dir::Down => (0..rows.len()).find_map(found),
            Dir::Up => (0..rows.len()).rev().find_map(found),
        }
    };
    match (mode, side) {
        (ViewMode::Unified, _) => scan(side).or_else(|| scan(None)),
        (ViewMode::Split, Some(side)) => scan(Some(side)).or_else(|| scan(Some(other(side)))),
        (ViewMode::Split, None) => {
            scan(Some(AnchorSide::New)).or_else(|| scan(Some(AnchorSide::Old)))
        }
    }
}

/// The other side.
pub fn other(side: AnchorSide) -> AnchorSide {
    match side {
        AnchorSide::Old => AnchorSide::New,
        AnchorSide::New => AnchorSide::Old,
    }
}

/// The rows that start a block of changed lines, in order.
pub fn change_starts(rows: &[Row], lines: &[DiffLine]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut in_block = false;
    for (index, row) in rows.iter().enumerate() {
        if !row.is_code() {
            // Inserts sit inside a block without ending it; anything else ends one.
            if !matches!(row, Row::Insert(_)) {
                in_block = false;
            }
            continue;
        }
        let changed = row.has_change(lines);
        if changed && !in_block {
            starts.push(index);
        }
        in_block = changed;
    }
    starts
}

/// Where the cursor goes on a change block's first row: its new line when it has one (an
/// addition or a pair), else its removed line.
pub fn change_pos(row: &Row, lines: &[DiffLine], mode: ViewMode) -> Option<LinePos> {
    match mode {
        ViewMode::Unified => cell_pos(row, lines, mode, None),
        ViewMode::Split => cell_pos(row, lines, mode, Some(AnchorSide::New))
            .filter(|pos| lines[pos.line].kind.is_change())
            .or_else(|| cell_pos(row, lines, mode, Some(AnchorSide::Old))),
    }
}

/// The gap row nearest row `from` (ties go to the one below).
pub fn nearest_gap(rows: &[Row], from: usize) -> Option<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| matches!(row, Row::Gap(_)))
        .map(|(index, _)| index)
        .min_by_key(|&index| (index.abs_diff(from), index < from))
}

/// The first and last line numbers on `side` among the lines between `a` and `b` (indexes into
/// `lines`, either order, inclusive): what a selection anchors. `None` when none of them exists
/// on `side`.
pub fn selected_numbers(
    lines: &[DiffLine],
    side: AnchorSide,
    a: usize,
    b: usize,
) -> Option<(u32, u32)> {
    let (first, last) = (a.min(b), a.max(b));
    let mut numbers = lines
        .get(first..=last.min(lines.len().saturating_sub(1)))?
        .iter()
        .filter_map(|line| side_number(line, side));
    let start = numbers.next()?;
    Some((start, numbers.next_back().unwrap_or(start)))
}

#[cfg(test)]
mod tests;
