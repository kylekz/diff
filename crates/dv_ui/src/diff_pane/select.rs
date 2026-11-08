//! Text in code (PLAN §18.1 "Text", DESIGN §4.1): the mouse selects characters of the lines
//! the stream shows, a click puts the caret, and `Ctrl C` copies.
//!
//! **Selecting.** A press in a line's code puts the keyboard cursor on its line (as a click on
//! its number does) and the caret at the character; dragging with the button down selects from
//! there to the pointer, character by character. A double-click selects the word under the
//! pointer ([`word_at`]) and a drag after it extends by whole words; a triple-click
//! selects the whole line and a drag after it extends by whole lines. A shift-click extends from
//! the selection's anchor (or the caret). A selection lives in one file and, in split view, on
//! one side (the column the press was in); in unified view it takes the file's rows in order,
//! removed and added lines alike. Dragging past the file's first or last row clamps there, and
//! dragging near the stream's top or bottom edge (or past it) scrolls ([`AUTOSCROLL_EDGE`]) while
//! the stream can still move. A press anywhere else in the stream, `Esc`, switching split /
//! unified or the comparison, and the file's diff changing (the agent's edit) clear it; so does
//! the window going inactive for a drag in progress.
//!
//! Its ends are [`TextPoint`]s: a file, a side, a line's number on that side and a byte offset
//! into its text, never a row, so a selection survives scrolling and rows being rebuilt (a gap
//! opening, a card measured). The pointer maps to a byte offset through the line as it was
//! shaped and painted ([`LineGeometry`], kept by `render::code` for every line it draws), so a
//! wide character, an emoji or a combining mark takes the room its glyph takes; a line not drawn
//! this frame falls back to the code font's advance.
//!
//! **The caret** ([`Caret`]) marks where a click in code was, painted as a thin accent bar while
//! the stream has the keyboard; the keyboard's line moves (`↑`/`↓`, `J`/`K`, `N`/`P`, pages,
//! `S`, `I`) clear it. Language features hang on it (PLAN §18.1).
//!
//! **Copy** (`Ctrl C`) writes the selected text through [`crate::system::write_clipboard`]: the
//! source text (real tabs), lines joined with `\n`, the lines a gap hides inside the selection
//! included, as an editor copies folded text. With no text selected, a line selection
//! (`Shift ↑`/`↓`) copies its lines; with neither, nothing happens.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use dv_core::diff::DiffLine;
use dv_core::review::AnchorSide;
use gpui::{Context, MouseDownEvent, Pixels, Point, ShapedLine};

use super::render::code::{MARKER_WIDTH, NUMBER_WIDTH, TEXT_INSET};
use super::rows::{LinePos, Row, heights, natural_side, on_side, side_number};
use super::text::{Visible, caret_at, char_column, column_at, unit_at, word_at};
use super::{Cursor, DiffPane};
use crate::services::ViewMode;

/// How close to the stream's top (below the pinned header's bar) or bottom edge the pointer
/// scrolls a selection being dragged, in logical pixels.
pub const AUTOSCROLL_EDGE: f32 = 24.;

/// How far one autoscroll tick moves at most, and how often one comes.
const AUTOSCROLL_MAX_STEP: f32 = 40.;
const AUTOSCROLL_TICK: std::time::Duration = std::time::Duration::from_millis(16);

/// A place in a line of code: a file, a side, the line's number on that side and a byte offset
/// into its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextPoint {
    pub path: String,
    pub side: AnchorSide,
    pub line: u32,
    pub offset: usize,
}

/// The caret: where a click in code put it.
pub type Caret = TextPoint;

/// Text selected with the mouse, from `anchor` (where it started) to `head` (where the pointer
/// went), either order. `side` is the split view's column; in unified view (`None`) each end is
/// on its line's own side and every row between is taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TextSelection {
    pub anchor: TextPoint,
    pub head: TextPoint,
    pub side: Option<AnchorSide>,
}

/// What a drag extends by: characters after a press, words after a double-click (the word it
/// took, a line index and a byte range), lines after a triple-click (the line, an index).
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Granularity {
    Character,
    Word { line: usize, word: Range<usize> },
    Line { line: usize },
}

/// The mouse button held after a press in code: the selection's head follows the pointer.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct TextDrag {
    pub path: String,
    pub side: Option<AnchorSide>,
    /// The pointer's last position, in window coordinates, which autoscroll maps again as the
    /// rows move under it.
    pub position: Point<Pixels>,
    pub granularity: Granularity,
}

/// Where a line drawn this frame is: the window x its painted slice starts at, the slice shaped
/// as it paints, and how the slice's bytes map to the source line's.
#[derive(Clone)]
pub(super) struct LineGeometry {
    /// The text cell's bounds: the pointer is on this line's code within them.
    pub bounds: gpui::Bounds<Pixels>,
    pub origin_x: Pixels,
    pub shaped: ShapedLine,
    pub shown: Rc<Visible>,
}

impl LineGeometry {
    /// The window x where byte `source` of the source line is drawn (the slice's start for a
    /// byte scrolled off before it, its end for one past it).
    pub fn x_of(&self, source: usize) -> Pixels {
        self.origin_x + self.shaped.x_for_index(self.shown.painted_of(source))
    }
}

/// A drawn line's key: its file, side and line (an index into the file's lines).
pub(super) type GeometryKey = (String, AnchorSide, usize);

/// The geometry of every line drawn this frame, which the pointer maps through.
pub(super) type Geometry = Rc<RefCell<HashMap<GeometryKey, LineGeometry>>>;

/// A selection resolved against its file's loaded lines: `(line, offset)` pairs, the line an
/// index into the lines, `start` before `end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Span {
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub side: Option<AnchorSide>,
}

impl Span {
    /// The byte range of line `line`'s text this span selects on `side`, and whether the span
    /// goes on past the line; `None` when it doesn't touch that line there.
    pub fn on_line(
        &self,
        lines: &[DiffLine],
        line: usize,
        side: AnchorSide,
    ) -> Option<(usize, usize, bool)> {
        if line < self.start.0 || line > self.end.0 {
            return None;
        }
        let diff_line = lines.get(line)?;
        match self.side {
            Some(selected) if selected != side || !on_side(diff_line, side) => return None,
            _ => {}
        }
        let from = if line == self.start.0 {
            self.start.1
        } else {
            0
        };
        let to = if line == self.end.0 {
            self.end.1
        } else {
            diff_line.text.len()
        };
        Some((from.min(to), to, line != self.end.0))
    }

    /// The selected text: each line's selected part, joined with `\n`.
    pub fn text(&self, lines: &[DiffLine]) -> String {
        // In unified view (`side` `None`) every line counts, whichever side is asked about.
        let side = self.side.unwrap_or(AnchorSide::New);
        let parts: Vec<&str> = (self.start.0..=self.end.0)
            .filter_map(|line| {
                let (from, to, _) = self.on_line(lines, line, side)?;
                lines[line].text.get(from..to)
            })
            .collect();
        parts.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// The index of the line numbered `number` on `side` among `lines`.
pub(super) fn line_index(lines: &[DiffLine], side: AnchorSide, number: u32) -> Option<usize> {
    lines
        .iter()
        .position(|line| side_number(line, side) == Some(number))
}

/// `offset` within `text`, moved back to a character boundary.
fn on_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The selected text as `state.stream.text_selection` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextSelectionRef {
    pub path: String,
    /// The split view's column, or `None` in unified view.
    pub side: Option<AnchorSide>,
    /// The first selected character and the place after the last: each its side, its line's
    /// number there and a 1-based character column.
    pub from: PlaceRef,
    pub to: PlaceRef,
    pub text: String,
}

/// A place in a line as automation names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaceRef {
    pub side: AnchorSide,
    pub line: u32,
    pub column: usize,
}

impl DiffPane {
    /// The selected text, if any (`state.stream.text_selection`).
    pub fn text_selection(&self) -> Option<TextSelectionRef> {
        let selection = self.text_selection.as_ref()?;
        let lines = self.lines(&selection.anchor.path)?;
        let span = self.span(selection, lines)?;
        if span.is_empty() {
            return None;
        }
        let place = |(line, offset): (usize, usize)| {
            let diff_line = &lines[line];
            let side = selection.side.unwrap_or_else(|| natural_side(diff_line));
            Some(PlaceRef {
                side,
                line: side_number(diff_line, side)?,
                column: char_column(&diff_line.text, offset),
            })
        };
        Some(TextSelectionRef {
            path: selection.anchor.path.clone(),
            side: selection.side,
            from: place(span.start)?,
            to: place(span.end)?,
            text: span.text(lines),
        })
    }

    /// The caret, if any, as `state.stream.caret` reports it: its file, side, line number and
    /// 1-based character column.
    pub fn caret(&self) -> Option<(String, PlaceRef)> {
        let caret = self.caret.as_ref()?;
        let lines = self.lines(&caret.path)?;
        let line = line_index(lines, caret.side, caret.line)?;
        let text = &lines[line].text;
        Some((
            caret.path.clone(),
            PlaceRef {
                side: caret.side,
                line: caret.line,
                column: char_column(text, on_boundary(text, caret.offset)),
            },
        ))
    }

    /// `selection` resolved against `lines` (its file's), its ends in order and on character
    /// boundaries.
    pub(super) fn span(&self, selection: &TextSelection, lines: &[DiffLine]) -> Option<Span> {
        let resolve = |point: &TextPoint| {
            let line = line_index(lines, point.side, point.line)?;
            Some((line, on_boundary(&lines[line].text, point.offset)))
        };
        let (a, b) = (resolve(&selection.anchor)?, resolve(&selection.head)?);
        Some(Span {
            start: a.min(b),
            end: a.max(b),
            side: selection.side,
        })
    }

    /// The selection as this frame paints it: its file and span, when it selects anything.
    pub(super) fn painted_span(&self) -> Option<(String, Span)> {
        let selection = self.text_selection.as_ref()?;
        let lines = self.lines(&selection.anchor.path)?;
        let span = self.span(selection, lines)?;
        (!span.is_empty()).then(|| (selection.anchor.path.clone(), span))
    }

    /// The text `Ctrl F` puts in the find bar: the selected text when it is on one line.
    pub(super) fn one_line_selection(&self) -> Option<String> {
        let selection = self.text_selection()?;
        (!selection.text.is_empty() && !selection.text.contains('\n')).then_some(selection.text)
    }

    /// Where the pointer at window x `x` is in a line's text on `side`, in the code font's
    /// columns (fractional) from the line's start, for a line not drawn this frame: the code
    /// cell's left edge worked out from the stream's layout (DESIGN §4.1: a 44-wide number and a
    /// 16-wide marker per side, both numbers then the marker in unified view), less the
    /// sideways scroll.
    pub(super) fn pointer_column(&self, x: Pixels, side: AnchorSide) -> f32 {
        let bounds = self.list.viewport_bounds();
        let width = f32::from(bounds.size.width);
        let cell = match self.mode {
            ViewMode::Unified => self.number_columns() * NUMBER_WIDTH + MARKER_WIDTH,
            ViewMode::Split => {
                let half = if side == AnchorSide::New {
                    width / 2.
                } else {
                    0.
                };
                half + NUMBER_WIDTH + MARKER_WIDTH
            }
        };
        let left = f32::from(bounds.origin.x) + cell + TEXT_INSET;
        (f32::from(x) - left + self.h_offset) / self.char_width.unwrap_or(7.2).max(1.)
    }

    /// Where a pointer at window x `x` is in `path`'s line `line` drawn on `side`: the caret's
    /// byte offset and the unit ([`super::text::units`]) under it, through the line's geometry
    /// this frame ([`LineGeometry`]), else the code font's advance.
    fn at_x(
        &self,
        path: &str,
        line: usize,
        side: AnchorSide,
        x: Pixels,
    ) -> (usize, Option<Range<usize>>) {
        let Some(text) = self.line(path, line).map(|line| line.text.as_str()) else {
            return (0, None);
        };
        let geometry = self.geometry.borrow();
        match geometry.get(&(path.to_string(), side, line)) {
            Some(drawn) => {
                let x_of = |at| f32::from(drawn.x_of(at));
                let x = f32::from(x);
                (caret_at(text, x, x_of), unit_at(text, x, x_of))
            }
            None => {
                let column = self.pointer_column(x, side);
                let x_of = |at| column_at(text, at) as f32;
                (caret_at(text, column, x_of), unit_at(text, column, x_of))
            }
        }
    }

    /// A left press in the code of `path`'s line `line` (an index into its lines), drawn on
    /// `side` (split view's column, or the line's own side in unified view): see the module docs.
    pub(super) fn press_text(
        &mut self,
        path: &str,
        line: usize,
        side: AnchorSide,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(diff_line) = self.line(path, line).cloned() else {
            return;
        };
        let Some(number) = side_number(&diff_line, side) else {
            return;
        };
        let text = &diff_line.text;
        // `Ctrl`+click: go to definition, the press selecting nothing (PLAN §18.1).
        if event.modifiers.secondary(cx)
            && event.click_count == 1
            && self.definition_click(event, cx)
        {
            return;
        }
        self.close_hover(cx);
        let (caret, unit) = self.at_x(path, line, side, event.position.x);
        let point = |offset: usize| TextPoint {
            path: path.to_string(),
            side,
            line: number,
            offset,
        };
        let selection_side = (self.mode == ViewMode::Split).then_some(side);
        // Where a shift-click extends from: the selection's anchor, else the caret, in the
        // same file (and column).
        let from = self
            .text_selection
            .as_ref()
            .map(|selection| selection.anchor.clone())
            .or_else(|| self.caret.clone())
            .filter(|from| from.path == path && (selection_side.is_none() || from.side == side));
        let (anchor, head, granularity) = match event.click_count {
            2 => {
                let word = unit.map_or(caret..caret, |unit| word_at(text, unit));
                (
                    point(word.start),
                    point(word.end),
                    Granularity::Word { line, word },
                )
            }
            count if count >= 3 => (point(0), point(text.len()), Granularity::Line { line }),
            _ => {
                let here = point(caret);
                match from.filter(|_| event.modifiers.shift) {
                    Some(from) => (from, here, Granularity::Character),
                    None => (here.clone(), here, Granularity::Character),
                }
            }
        };
        self.click_line(path, LinePos { line, side }, false, cx);
        self.caret = Some(head.clone());
        self.text_selection = Some(TextSelection {
            anchor,
            head,
            side: selection_side,
        });
        self.text_drag = Some(TextDrag {
            path: path.to_string(),
            side: selection_side,
            position: event.position,
            granularity,
        });
        cx.notify();
    }

    /// The pointer moved to `position` with the button still down after a press in code: the
    /// selection's head, the caret and the cursor follow it (clamped to the press's file and
    /// column), and near an edge the stream scrolls.
    pub(super) fn drag_text_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = self.text_drag.as_mut() else {
            return;
        };
        drag.position = position;
        let drag = drag.clone();
        self.move_head(&drag, cx);
        if self.autoscroll_delta(position) != 0. && self.autoscroll.is_none() {
            self.autoscroll = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(AUTOSCROLL_TICK).await;
                    let more = this
                        .update(cx, |this, cx| this.autoscroll_tick(cx))
                        .unwrap_or(false);
                    if !more {
                        break;
                    }
                }
            }));
        }
    }

    /// The place in the drag's file at `line` (an index) and `offset`, on the drag's column or
    /// the line's own side.
    fn drag_point(&self, drag: &TextDrag, line: usize, offset: usize) -> Option<TextPoint> {
        let diff_line = self.line(&drag.path, line)?;
        let side = drag.side.unwrap_or_else(|| natural_side(diff_line));
        Some(TextPoint {
            path: drag.path.clone(),
            side,
            line: side_number(diff_line, side)?,
            offset,
        })
    }

    /// Puts the selection's head where `drag`'s pointer is, a whole word or line at a time
    /// after a double- or triple-click, the anchor then the far end of the word or line it
    /// started on.
    fn move_head(&mut self, drag: &TextDrag, cx: &mut Context<Self>) {
        let Some((line, offset, unit)) = self.text_point_at(drag) else {
            return;
        };
        let Some(text) = self.line(&drag.path, line).map(|line| line.text.clone()) else {
            return;
        };
        let line_end = |line: usize| self.line(&drag.path, line).map_or(0, |l| l.text.len());
        let ((anchor_line, anchor), (head_line, head)) = match &drag.granularity {
            Granularity::Character => {
                let Some(selection) = self.text_selection.as_ref() else {
                    return;
                };
                let anchor = selection.anchor.clone();
                let Some(lines) = self.lines(&drag.path) else {
                    return;
                };
                let Some(anchor_line) = line_index(lines, anchor.side, anchor.line) else {
                    return;
                };
                ((anchor_line, anchor.offset), (line, offset))
            }
            Granularity::Word {
                line: start,
                word: first,
            } => {
                let word = unit.map_or(offset..offset, |unit| word_at(&text, unit));
                if (line, word.start) < (*start, first.start) {
                    ((*start, first.end), (line, word.start))
                } else {
                    // Within the word it started on, the whole of it stays.
                    let end = if line == *start {
                        word.end.max(first.end)
                    } else {
                        word.end
                    };
                    ((*start, first.start), (line, end))
                }
            }
            Granularity::Line { line: start } => {
                if line < *start {
                    ((*start, line_end(*start)), (line, 0))
                } else {
                    ((*start, 0), (line, line_end(line)))
                }
            }
        };
        let (Some(anchor), Some(head)) = (
            self.drag_point(drag, anchor_line, anchor),
            self.drag_point(drag, head_line, head),
        ) else {
            return;
        };
        let Some(selection) = self
            .text_selection
            .as_mut()
            .filter(|selection| selection.anchor.path == drag.path)
        else {
            return;
        };
        if selection.head == head && selection.anchor == anchor {
            return;
        }
        selection.anchor = anchor;
        selection.head = head.clone();
        let side = head.side;
        self.caret = Some(head);
        self.cursor = Some(Cursor {
            path: drag.path.clone(),
            pos: LinePos {
                line: head_line,
                side,
            },
        });
        cx.notify();
    }

    /// The button came up after a press in code (wherever it came up, however fast): a press
    /// that selected nothing leaves the caret alone.
    pub(super) fn end_text_drag(&mut self, cx: &mut Context<Self>) {
        if self.text_drag.take().is_none() {
            return;
        }
        self.autoscroll = None;
        let empty = self.text_selection.as_ref().is_none_or(|selection| {
            self.lines(&selection.anchor.path)
                .and_then(|lines| self.span(selection, lines))
                .is_none_or(|span| span.is_empty())
        });
        if empty {
            self.text_selection = None;
        }
        self.viewport_changed(cx);
        cx.notify();
    }

    /// The line (an index into the drag's file's lines) and byte offset under `drag`'s pointer,
    /// with the unit under it when the pointer is on that line's code, clamped to the file:
    /// above its first row (or its header) the start of its first line, below its last the end
    /// of its last, and on a row without a line on the drag's side (a card, a gap, the other
    /// column's empty cell) the end of the line before it.
    fn text_point_at(&self, drag: &TextDrag) -> Option<(usize, usize, Option<Range<usize>>)> {
        let block = *self.index.get(&drag.path)?;
        let lines = self.lines(&drag.path)?;
        let rows = &self.blocks[block].rows;
        let bounds = self.list.viewport_bounds();
        let height = self.viewport_height();
        let y = (f32::from(drag.position.y) - f32::from(bounds.origin.y)).clamp(0., height - 1.);
        let top = self.list.logical_scroll_top();
        let mut at = -f32::from(top.offset_in_item);
        let mut item = top.item_ix;
        while item + 1 < self.item_count() {
            let size = self.item_height(item);
            if at + size > y {
                break;
            }
            at += size;
            item += 1;
        }
        let cell = |row: &Row| match (self.mode, row, drag.side) {
            (ViewMode::Unified, Row::Line(line), _) => Some(*line),
            (ViewMode::Split, _, Some(side)) => row.cell(lines, side),
            _ => None,
        };
        let end_of = |line: usize| (line, lines[line].text.len(), None);
        let first = || rows.iter().find_map(cell).map(|line| (line, 0, None));
        let last = || rows.iter().rev().find_map(cell).map(end_of);
        let (at_block, row) = match self.locate(item) {
            Some(found) => found,
            None => return last(),
        };
        if at_block < block {
            return first();
        }
        if at_block > block {
            return last();
        }
        match rows.get(row).and_then(cell) {
            Some(line) => {
                let side = drag.side.unwrap_or_else(|| natural_side(&lines[line]));
                let (offset, unit) = self.at_x(&drag.path, line, side, drag.position.x);
                Some((line, offset, unit))
            }
            None => rows[..row.min(rows.len())]
                .iter()
                .rev()
                .find_map(cell)
                .map(end_of)
                .or_else(first),
        }
    }

    /// How far, and which way, a pointer at `position` scrolls a drag: up (negative) within
    /// [`AUTOSCROLL_EDGE`] of the stream's top below the pinned header's bar or above it, down
    /// near or past its bottom; 0 elsewhere.
    fn autoscroll_delta(&self, position: Point<Pixels>) -> f32 {
        let bounds = self.list.viewport_bounds();
        let y = f32::from(position.y) - f32::from(bounds.origin.y);
        let height = self.viewport_height();
        let top = heights::HEADER_BAR + AUTOSCROLL_EDGE;
        let bottom = height - AUTOSCROLL_EDGE;
        let step = |into: f32| (into / 2.).clamp(2., AUTOSCROLL_MAX_STEP);
        if y < top {
            -step(top - y)
        } else if y > bottom {
            step(y - bottom)
        } else {
            0.
        }
    }

    /// One autoscroll tick: the stream scrolls towards the pointer and the head follows;
    /// `false` once the drag ended, the pointer left the edges, or the stream can't move that
    /// way any more.
    fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.text_drag.clone() else {
            self.autoscroll = None;
            return false;
        };
        let delta = self.autoscroll_delta(drag.position);
        if delta == 0. {
            self.autoscroll = None;
            return false;
        }
        let top = self.list.logical_scroll_top();
        let target = self.offset_by(top, delta);
        let max = self.max_scroll_top();
        let past_end = (target.item_ix, f32::from(target.offset_in_item))
            > (max.item_ix, f32::from(max.offset_in_item));
        let to = if past_end { max } else { target };
        if (to.item_ix, to.offset_in_item) == (top.item_ix, top.offset_in_item) {
            self.autoscroll = None;
            return false;
        }
        self.list.scroll_to(to);
        self.move_head(&drag, cx);
        self.viewport_changed(cx);
        cx.notify();
        true
    }

    /// A press in the stream outside code (the root's capture, before the press reaches what
    /// is under it): the selection and the caret go, unless Shift holds them for a shift-click.
    pub(super) fn press_outside_text(&mut self, shift: bool, cx: &mut Context<Self>) {
        // A press on the hover card (selecting its text, its buttons) leaves the stream's be.
        if shift || self.hover_ui.is_over_card() {
            return;
        }
        if self.text_selection.take().is_some() | self.caret.take().is_some() {
            cx.notify();
        }
    }

    /// Forgets the selection and the caret in `path`, whose diff was replaced: their line
    /// numbers and offsets named the text it had.
    pub(super) fn forget_text_in(&mut self, path: &str) {
        // A hover card on the file named its text too (`intel`).
        self.hover_ui.file_replaced(path);
        if self
            .text_selection
            .as_ref()
            .is_some_and(|selection| selection.anchor.path == path)
        {
            self.clear_text_selection();
        }
        if self.caret.as_ref().is_some_and(|caret| caret.path == path) {
            self.caret = None;
        }
    }
    /// Clears the selected text; `false` when there was none.
    pub(super) fn clear_text_selection(&mut self) -> bool {
        self.text_drag = None;
        self.autoscroll = None;
        self.text_selection.take().is_some()
    }

    /// `Ctrl C`: the selected text, else the selected lines' text, through the system seam.
    pub(crate) fn copy(&mut self, cx: &mut Context<Self>) {
        let text = self
            .text_selection()
            .map(|selection| selection.text)
            .or_else(|| self.selected_lines_text());
        if let Some(text) = text {
            crate::system::write_clipboard(text, cx);
        }
    }

    /// The text of the lines a line selection (`Shift ↑`/`↓`) picked, joined with `\n`.
    fn selected_lines_text(&self) -> Option<String> {
        let selection = self.selection()?;
        let lines = self.lines(&selection.path)?;
        let picked: Vec<&str> = lines
            .iter()
            .filter(|line| {
                side_number(line, selection.side)
                    .is_some_and(|n| (selection.start_line..=selection.end_line).contains(&n))
            })
            .map(|line| line.text.as_str())
            .collect();
        Some(picked.join("\n"))
    }
}
