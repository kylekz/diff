//! Relocation and placement (docs/PLAN.md §8): where a thread shows once the file it is
//! anchored in has moved on.
//!
//! - [`relocate`] finds an anchor's text in its side's current full text: still at its
//!   recorded lines, moved (an exact match nearby, the only exact match in the file, or a fuzzy
//!   match), or outdated.
//! - [`place_thread`] decides the listed file a thread belongs to now ([`thread_file`]: its path,
//!   the file renamed from it, or what its round's history says the file is called now) and
//!   runs [`relocate`] on that file's side; a new-side thread relocation can't find is traced
//!   through its round's snapshot to where its code went (`super::history`).
//! - [`place_unlisted`] places a thread whose file left the list against that file's text at
//!   the review's new side: found there, it stands in a file the review doesn't change.
//! - [`relocated_anchor`] is the anchor the next event on a moved thread records (PLAN §8:
//!   relocation is display-only until then).
//!
//! The GUI and the CLI place threads only through these functions, so they never disagree
//! about where a thread is.

use std::cell::OnceCell;
use std::cmp::Reverse;

use super::anchor::{ANCHOR_CONTEXT, AnchorPlace, anchor_selection};
use super::history::{History, PathHistory, Trail, trace};
use super::model::{Anchor, AnchorSide, Thread};
use crate::diff::{FileEntry, FileStatus};

/// How far from its recorded start an exact match of an anchor's text counts as nearby, in
/// lines (PLAN §8 step 2).
pub const NEARBY_LINES: usize = 200;

/// The least score a fuzzy match needs (PLAN §8 step 5).
pub const FUZZY_THRESHOLD: f64 = 0.80;

/// How far below the best fuzzy score a nearer window may score and still win (PLAN §8 step
/// 4): in repetitive text (rows of data, flag lists) an untouched window far away can score a
/// little better than the anchored lines edited where they stand.
pub const FUZZY_MARGIN: f64 = 0.05;

/// A side with more lines than this skips the fuzzy step (PLAN §8 step 5): every candidate
/// window is scored, so its cost grows with the file.
pub const FUZZY_MAX_LINES: usize = 20_000;

/// The most anchored lines the fuzzy step compares (PLAN §8 step 5): a longer anchor is
/// scored on this many of its lines, its first and last among them and the rest spread evenly
/// between, so a window costs the same whatever the range's length.
pub const FUZZY_MAX_ANCHORED: usize = 16;

/// How many more lines than it anchored the lines standing where an anchor's text was may
/// have and still count as its text rewritten there (PLAN §8 step 4, [`relocate`]).
pub const REWRITE_SLACK: usize = 10;

/// An anchor with fewer non-whitespace characters than this, in all its lines, is slight
/// ([`is_slight`], PLAN §8 step 4).
pub const SLIGHT_CHARS: usize = 32;

/// Scores within this of each other are equal: a weighted mean that is exactly 0.8 on paper
/// may come out a hair below it in floating point.
const EPSILON: f64 = 1e-9;

/// Where an anchor's text stands in its side's current text ([`relocate`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Relocation {
    /// At its recorded lines.
    Unchanged,
    /// At `start_line..=end_line` (1-based) instead.
    Moved {
        start_line: u32,
        end_line: u32,
        how: MoveKind,
    },
    /// Nowhere dv can tell.
    Outdated,
}

/// How a moved anchor was found (PLAN §8 steps 2–5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MoveKind {
    /// The anchored lines, exactly, within [`NEARBY_LINES`] of the recorded start.
    Nearby,
    /// The anchored lines, exactly, once in the whole file.
    Unique,
    /// A window of the anchor's shape whose lines are similar enough; `score` is its weighted
    /// mean similarity, at least [`FUZZY_THRESHOLD`].
    Fuzzy { score: f64 },
}

impl MoveKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nearby => "nearby",
            Self::Unique => "unique",
            Self::Fuzzy { .. } => "fuzzy",
        }
    }
}

/// A side's lines as the fuzzy step compares them (PLAN §8 step 5), built from the side's
/// text the first time a thread on it needs that step, then kept for every other thread on
/// the same text: building it is a pass over the whole side. A caller placing several threads
/// on one side keeps one per side ([`relocate_with`], [`SideLines::fuzzy`]).
#[derive(Debug, Default)]
pub struct FuzzyLines(OnceCell<Vec<Shingled>>);

impl FuzzyLines {
    fn get<S: AsRef<str>>(&self, lines: &[S]) -> &[Shingled] {
        self.0.get_or_init(|| {
            lines
                .iter()
                .map(|line| Shingled::new(line.as_ref()))
                .collect()
        })
    }
}

/// Where `anchor`'s text stands in `lines`, its side's current full text (`lines[0]` is line
/// 1, [`super::side_lines`]), by PLAN §8's steps:
///
/// 1. the anchored text still stands at its recorded lines: [`Relocation::Unchanged`];
/// 2. exact matches of the anchored lines within [`NEARBY_LINES`] of the recorded start: the
///    one whose context lines also match best wins (the count of `context_before` and
///    `context_after` lines that match where they belong), then the nearest, then the one
///    above: [`MoveKind::Nearby`];
/// 3. else an exact match anywhere in the file, if it is the only one: [`MoveKind::Unique`];
/// 4. else (and for a slight anchor whose exact matches are all bare, below), when the
///    anchor's whole context still stands together where its lines were, with the anchored
///    lines gone from between ([`closed_context`]): deleted (nothing left between)
///    is [`Relocation::Outdated`]; rewritten (up to the anchored count plus
///    [`REWRITE_SLACK`] lines between) is found there, as [`MoveKind::Fuzzy`] over those
///    lines, when they score at least [`FUZZY_THRESHOLD`] as a window, and is outdated
///    otherwise — never moved to a like line elsewhere (a sibling match arm, the next row);
/// 5. else fuzzy, unless the side has more than [`FUZZY_MAX_LINES`] lines: every window of the
///    anchor's shape (its context before, as many lines as it anchors, its context after,
///    clipped at the file's edges, where a missing line scores 0) scores the weighted mean of
///    its lines' similarity to the anchor's, anchored lines weighing 2 and context lines 1
///    ([`line_similarity`]; a long anchor on [`FUZZY_MAX_ANCHORED`] of its lines); of the
///    windows scoring at least [`FUZZY_THRESHOLD`], the nearest (then the one above) within
///    [`FUZZY_MARGIN`] of the best score wins: [`MoveKind::Fuzzy`];
/// 6. else [`Relocation::Outdated`].
///
/// An exact match found by step 2 or 3 wins at once when its context stands around it too. One
/// standing among other lines (a bare match) may be only the anchored text repeated somewhere
/// (`return Ok(());`, `}`, a second debug print):
///
/// - for a slight anchor ([`is_slight`]: under [`SLIGHT_CHARS`] non-whitespace characters in
///   all), whose text says too little to tell a move from a copy that was always
///   there, step 4 runs first, so a line deleted or rewritten where it stood is outdated or
///   found there, never moved onto its twin (a match standing between the closed-up context,
///   lines inserted beside it, is the anchored text itself, and still wins);
/// - a fuller anchor found elsewhere was moved there (a block cut and pasted leaves its context
///   closed up behind it), so step 4 is only for one whose text is found nowhere.
///
/// Then step 5 runs as well, and a window that scores better than the bare match's own window
/// as a whole wins over it, so an edit to the anchored lines themselves is found where it was
/// made rather than at a bare copy of the old text.
///
/// An anchor of blank lines only would match any blank run by text alone, so in steps 2 and 3
/// its matches count only where at least one context line matches too. A file-level anchor
/// ([`Anchor::is_file_level`]) has no lines to find: it is `Outdated` here, and
/// [`place_thread`] never asks.
pub fn relocate<S: AsRef<str>>(anchor: &Anchor, lines: &[S]) -> Relocation {
    relocate_with(anchor, lines, &FuzzyLines::default())
}

/// [`relocate`], reading the fuzzy step's form of `lines` from `fuzzy`, which holds (or is
/// about to hold) exactly that text: the caller keeps it for every thread on the same side.
pub fn relocate_with<S: AsRef<str>>(
    anchor: &Anchor,
    lines: &[S],
    fuzzy: &FuzzyLines,
) -> Relocation {
    // A file-level anchor has no lines to look for: placement never relocates one (PLAN §8).
    if anchor.is_file_level() {
        return Relocation::Outdated;
    }
    if anchor.matches(lines) {
        return Relocation::Unchanged;
    }
    let count = anchor.text.len();
    if count == 0 || count > lines.len() {
        return Relocation::Outdated;
    }
    let recorded = (anchor.start_line as usize).saturating_sub(1);
    let blank_only = anchor.text.iter().all(|line| line.trim().is_empty());
    // Every exact match, with how many of its context lines match too.
    let exact: Vec<(usize, usize)> = (0..=lines.len() - count)
        .filter(|&start| {
            lines[start..start + count]
                .iter()
                .zip(&anchor.text)
                .all(|(line, text)| line.as_ref() == text)
        })
        .map(|start| (start, context_matches(anchor, lines, start)))
        .filter(|&(_, context)| !blank_only || context > 0)
        .collect();
    let moved = |start: usize, how: MoveKind| Relocation::Moved {
        start_line: line_number(start),
        end_line: line_number(start + count - 1),
        how,
    };
    let nearby = exact
        .iter()
        .filter(|(start, _)| start.abs_diff(recorded) <= NEARBY_LINES)
        .min_by_key(|&&(start, context)| (Reverse(context), start.abs_diff(recorded), start))
        .map(|&(start, context)| (start, context, MoveKind::Nearby));
    // None of the exact matches is nearby, so the only one is far.
    let unique = match exact[..] {
        [(start, context)] => Some((start, context, MoveKind::Unique)),
        _ => None,
    };
    let found = nearby.or(unique);
    let whole_context = anchor.context_before.len() + anchor.context_after.len();
    if let Some((start, context, how)) = found
        && context == whole_context
    {
        return moved(start, how);
    }
    // A match left here is bare: a slight anchor's yields to its context closed up where it
    // stood, unless an exact match stands inside that context, which is the anchored text
    // itself with lines inserted beside it. At a file's edge the context runs to the edge, so
    // lines added there count as inserted beside it: code added above a file's last line keeps
    // its thread on that line, at the price of a last line deleted while code ending in a copy
    // of it is appended, which lands on the copy (PLAN §8).
    if (found.is_none() || is_slight(anchor))
        && let Some((start, between)) = closed_context(anchor, lines, recorded)
    {
        let inside = exact
            .iter()
            .map(|&(at, _)| at)
            .find(|&at| start <= at && at + count <= start + between);
        return match inside {
            Some(at) if found.is_some() => {
                let how = if at.abs_diff(recorded) <= NEARBY_LINES {
                    MoveKind::Nearby
                } else {
                    MoveKind::Unique
                };
                moved(at, how)
            }
            _ => in_place(anchor, lines, start, between),
        };
    }
    if lines.len() <= FUZZY_MAX_LINES {
        let scorer = Scorer::new(anchor);
        let file = fuzzy.get(lines);
        // A bare exact match is beaten only by a window that scores better than its own.
        let floor = found.map_or(FUZZY_THRESHOLD, |(start, ..)| {
            FUZZY_THRESHOLD.max(scorer.score(file, start, 0.0).unwrap_or(0.0) + EPSILON)
        });
        if let Some((start, score)) = scorer.best(file, recorded, floor) {
            return moved(start, MoveKind::Fuzzy { score });
        }
    }
    match found {
        Some((start, _, how)) => moved(start, how),
        None => Relocation::Outdated,
    }
}

/// Whether `anchor`'s text is too slight to be told from a copy of it elsewhere (PLAN §8,
/// [`relocate`]): fewer than [`SLIGHT_CHARS`] non-whitespace characters in all its lines (`}`
/// and a blank line, `return Ok(());`). A line that says more (`let page = paginate(items,
/// req.page);`) is followed where it moves, alone or not.
pub fn is_slight(anchor: &Anchor) -> bool {
    let chars: usize = anchor
        .text
        .iter()
        .map(|line| line.chars().filter(|c| !c.is_whitespace()).count())
        .sum();
    chars < SLIGHT_CHARS
}

/// A 0-based line index as a 1-based line number.
fn line_number(index: usize) -> u32 {
    u32::try_from(index + 1).unwrap_or(u32::MAX)
}

/// Where `anchor`'s whole context stands together with the anchored lines gone from between
/// it (PLAN §8 step 4): the index (0-based) of the first line after `context_before`, and how
/// many lines stand between it and `context_after` — none when the anchored lines were deleted,
/// at most the anchored count plus [`REWRITE_SLACK`] when they were rewritten. The nearest to
/// `recorded` wins, then the one above. A context cut short by the file's edge (an anchor
/// within [`ANCHOR_CONTEXT`] lines of it) must still meet that edge. `None` for an anchor
/// without context, which says nothing about where its lines were.
fn closed_context<S: AsRef<str>>(
    anchor: &Anchor,
    lines: &[S],
    recorded: usize,
) -> Option<(usize, usize)> {
    let (before, after) = (&anchor.context_before, &anchor.context_after);
    if before.is_empty() && after.is_empty() {
        return None;
    }
    let stands = |at: usize, text: &[String]| {
        lines.get(at..at + text.len()).is_some_and(|found| {
            found
                .iter()
                .zip(text)
                .all(|(line, text)| line.as_ref() == text)
        })
    };
    let most_between = anchor.text.len() + REWRITE_SLACK;
    let mut best: Option<(usize, usize)> = None;
    for start in before.len()..=lines.len() {
        // A short context before stands at the top of the file or nowhere.
        if before.len() < ANCHOR_CONTEXT && start != before.len() {
            break;
        }
        if !stands(start - before.len(), before) {
            continue;
        }
        let between = (0..=most_between).find(|&between| {
            let past = start + between;
            stands(past, after)
                && (after.len() == ANCHOR_CONTEXT || past + after.len() == lines.len())
        });
        if let Some(between) = between {
            let nearer = best.is_none_or(|(kept, _)| {
                (start.abs_diff(recorded), start) < (kept.abs_diff(recorded), kept)
            });
            if nearer {
                best = Some((start, between));
            }
        }
    }
    best
}

/// PLAN §8 step 4: `anchor`'s lines gone from between its context, which stands around the
/// `between` lines from `start` (0-based). Nothing between: they were deleted, outdated. Else
/// they were rewritten there: found at those lines when they score at least
/// [`FUZZY_THRESHOLD`] as step 5 scores a window (the context lines all matching; as many lines
/// as were anchored compared line by line, otherwise as one text each), else outdated.
fn in_place<S: AsRef<str>>(
    anchor: &Anchor,
    lines: &[S],
    start: usize,
    between: usize,
) -> Relocation {
    if between == 0 {
        return Relocation::Outdated;
    }
    let count = anchor.text.len();
    let now = &lines[start..start + between];
    let picked: Vec<usize> = scored_lines(count).collect();
    let similar = if between == count {
        picked
            .iter()
            .map(|&j| line_similarity(&anchor.text[j], now[j].as_ref()))
            .sum::<f64>()
            / picked.len() as f64
    } else {
        let now: Vec<&str> = now.iter().map(AsRef::as_ref).collect();
        line_similarity(&anchor.text.join(" "), &now.join(" "))
    };
    let context = (anchor.context_before.len() + anchor.context_after.len()) as f64;
    let anchored = 2.0 * picked.len() as f64;
    let score = (context + anchored * similar) / (context + anchored);
    if score + EPSILON >= FUZZY_THRESHOLD {
        Relocation::Moved {
            start_line: line_number(start),
            end_line: line_number(start + between - 1),
            how: MoveKind::Fuzzy { score },
        }
    } else {
        Relocation::Outdated
    }
}

/// How many of `anchor`'s context lines match where they would stand around its anchored
/// lines placed at `start` (0-based) in `lines`.
fn context_matches<S: AsRef<str>>(anchor: &Anchor, lines: &[S], start: usize) -> usize {
    let line = |at: Option<usize>| at.and_then(|at| lines.get(at)).map(AsRef::as_ref);
    let before = anchor
        .context_before
        .iter()
        .rev()
        .enumerate()
        .filter(|(i, text)| line(start.checked_sub(i + 1)) == Some(text.as_str()))
        .count();
    let past = start + anchor.text.len();
    let after = anchor
        .context_after
        .iter()
        .enumerate()
        .filter(|(i, text)| line(Some(past + i)) == Some(text.as_str()))
        .count();
    before + after
}

/// A line prepared for [`line_similarity`], once per line: the character bigrams of its
/// whitespace-normalised form (trimmed, every inner run of whitespace one space), sorted.
#[derive(Debug)]
struct Shingled {
    bigrams: Vec<u64>,
    /// A normalised form shorter than two characters, which has no bigrams and is compared by
    /// equality: `Some(None)` when empty, `Some(Some(c))` for one character.
    short: Option<Option<char>>,
}

impl Shingled {
    fn new(line: &str) -> Self {
        let mut bigrams = Vec::with_capacity(line.len());
        let (mut first, mut previous): (Option<char>, Option<char>) = (None, None);
        let mut push = |c: char| {
            if let Some(p) = previous {
                bigrams.push((u64::from(p) << 32) | u64::from(c));
            }
            first.get_or_insert(c);
            previous = Some(c);
        };
        for (n, word) in line.split_whitespace().enumerate() {
            if n > 0 {
                push(' ');
            }
            word.chars().for_each(&mut push);
        }
        bigrams.sort_unstable();
        let short = bigrams.is_empty().then_some(first);
        Self { bigrams, short }
    }
}

/// How similar two lines are, from 0 to 1, after normalising their whitespace (trimmed, inner
/// runs of whitespace collapsed to one space): equal lines (two empty ones included) are 1; a
/// line shorter than two characters is compared by equality; otherwise the Dice coefficient of
/// the two lines' character bigrams (twice the bigrams they share over the bigrams both have),
/// which is 1 for equal lines too.
pub fn line_similarity(a: &str, b: &str) -> f64 {
    similarity(&Shingled::new(a), &Shingled::new(b))
}

fn similarity(a: &Shingled, b: &Shingled) -> f64 {
    match (a.short, b.short) {
        (Some(a), Some(b)) => return if a == b { 1.0 } else { 0.0 },
        (Some(_), None) | (None, Some(_)) => return 0.0,
        (None, None) => {}
    }
    // Both lists are sorted: count the bigrams they share, repeats included.
    let (mut i, mut j, mut shared) = (0, 0, 0usize);
    while i < a.bigrams.len() && j < b.bigrams.len() {
        match a.bigrams[i].cmp(&b.bigrams[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
        }
    }
    2.0 * shared as f64 / (a.bigrams.len() + b.bigrams.len()) as f64
}

/// The windows of PLAN §8 step 5 for one anchor: the lines a window's score compares, each
/// with its weight and its offset from the window's first anchored line.
struct Scorer {
    /// (weight, the anchor's line, its offset): the anchored lines first, the heaviest, so a
    /// window that can't make the score it needs is dropped soonest.
    parts: Vec<(f64, Shingled, isize)>,
    total: f64,
    /// How many lines the anchor covers, which a window does too.
    count: usize,
}

impl Scorer {
    fn new(anchor: &Anchor) -> Self {
        let count = anchor.text.len();
        let before = anchor.context_before.len();
        let parts: Vec<(f64, Shingled, isize)> = scored_lines(count)
            .map(|j| (2.0, Shingled::new(&anchor.text[j]), offset(j, 0)))
            .chain(
                anchor
                    .context_before
                    .iter()
                    .enumerate()
                    .map(|(i, text)| (1.0, Shingled::new(text), offset(i, before))),
            )
            .chain(
                anchor
                    .context_after
                    .iter()
                    .enumerate()
                    .map(|(i, text)| (1.0, Shingled::new(text), offset(count + i, 0))),
            )
            .collect();
        let total = parts.iter().map(|(weight, ..)| weight).sum();
        Self {
            parts,
            total,
            count,
        }
    }

    /// The score of the window whose anchored lines start at `start` (0-based) in `file`, or
    /// `None` as soon as the lines left can't lift it to `needed`.
    fn score(&self, file: &[Shingled], start: usize, needed: f64) -> Option<f64> {
        let (mut scored, mut left) = (0.0, self.total);
        for (weight, text, offset) in &self.parts {
            left -= weight;
            let similar = start
                .checked_add_signed(*offset)
                .and_then(|at| file.get(at))
                .map_or(0.0, |line| similarity(text, line));
            scored += weight * similar;
            if (scored + left) / self.total < needed {
                return None;
            }
        }
        Some(scored / self.total)
    }

    /// PLAN §8 step 5 ([`relocate`]): of the windows scoring at least `floor`, the nearest to
    /// `recorded` (then the one above) within [`FUZZY_MARGIN`] of the best; its 0-based start
    /// and its score.
    ///
    /// Windows are scored nearest first, each needing the floor or the best score so far less
    /// the margin, whichever is higher. Every window within the margin of the final best
    /// meets what it needed when it was scored, so the first one kept that is within the
    /// margin of the best is the nearest.
    fn best(&self, file: &[Shingled], recorded: usize, floor: f64) -> Option<(usize, f64)> {
        let mut starts: Vec<usize> = (0..=file.len().checked_sub(self.count)?).collect();
        starts.sort_by_key(|&start| (start.abs_diff(recorded), start));
        let floor = floor - EPSILON;
        let mut best: Option<f64> = None;
        let mut kept: Vec<(usize, f64)> = Vec::new();
        for start in starts {
            let needed = best.map_or(floor, |best| floor.max(best - FUZZY_MARGIN));
            if let Some(score) = self.score(file, start, needed) {
                best = Some(best.map_or(score, |best| best.max(score)));
                kept.push((start, score));
            }
        }
        let best = best?;
        kept.into_iter()
            .find(|&(_, score)| score >= best - FUZZY_MARGIN - EPSILON)
    }
}

/// The anchored lines (0-based, of `count`) the fuzzy step compares: all of them, or
/// [`FUZZY_MAX_ANCHORED`] spread evenly from the first to the last.
fn scored_lines(count: usize) -> impl Iterator<Item = usize> {
    let picked = count.min(FUZZY_MAX_ANCHORED);
    (0..picked).map(move |i| match picked {
        1 => 0,
        _ if picked == count => i,
        _ => (i * (count - 1) + (picked - 1) / 2) / (picked - 1),
    })
}

/// `index - back` as a signed offset.
fn offset(index: usize, back: usize) -> isize {
    isize::try_from(index).unwrap_or(isize::MAX) - isize::try_from(back).unwrap_or(isize::MAX)
}

/// A side's text as [`place_thread`]'s caller has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideText<L> {
    /// The side's full text, line 1 first ([`super::side_lines`]), as a [`SideLines`].
    Lines(L),
    /// The file's diff isn't loaded yet.
    Pending,
    /// The file has no lines on that side to look in: binary, a submodule, unmerged.
    Unavailable,
}

/// A side's full text as [`place_thread`] reads it: its lines and, when the caller keeps one
/// for the side, the fuzzy step's form of them, shared by every thread placed on it
/// ([`FuzzyLines`]).
pub trait SideLines {
    type Line: AsRef<str>;

    /// The side's lines, line 1 first.
    fn lines(&self) -> &[Self::Line];

    /// The fuzzy step's form of [`SideLines::lines`] the caller keeps, if any.
    fn fuzzy(&self) -> Option<&FuzzyLines> {
        None
    }
}

impl<S: AsRef<str>> SideLines for Vec<S> {
    type Line = S;

    fn lines(&self) -> &[S] {
        self
    }
}

impl<S: AsRef<str>> SideLines for &[S] {
    type Line = S;

    fn lines(&self) -> &[S] {
        self
    }
}

/// Where a thread shows now (PLAN §8), as [`place_thread`] decides it. `file` is the listed
/// file's [`FileEntry::path`], which keys it in the file list and the stream.
#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    /// In `file`, its anchored text at its recorded lines.
    Unchanged { file: String },
    /// In `file`, its anchored text moved to `start_line..=end_line` of its side.
    Moved {
        file: String,
        start_line: u32,
        end_line: u32,
        how: MoveKind,
    },
    /// Not placed yet: `file`'s text isn't loaded, or its round's history isn't read (`file`
    /// then the listed file at its path, if any: a deleted file).
    Pending { file: Option<String> },
    /// At no line: its file left the list (`file: None`), or the listed `file` no longer has
    /// its side (a new-side thread on a deleted file) or its text isn't found there. `trail`
    /// says where its code went since its round, when its round's snapshot tells
    /// (`super::history::trace`): still outdated, its anchor no line of the file now.
    Outdated {
        file: Option<String>,
        trail: Option<Trail>,
    },
    /// A comment on the whole of `file` (a file-level anchor, [`Anchor::is_file_level`]),
    /// which has its side: at no line, and not outdated.
    File { file: String },
    /// Not among the review's changes: its file left the list (it never changed, or stopped
    /// changing) but stands at the review's new side, and the anchored text was found there
    /// ([`place_unlisted`]), at its recorded lines (`how` `None`) or moved to
    /// `start_line..=end_line`. `path` is that file's. Not outdated, and in no listed file.
    Unlisted {
        path: String,
        start_line: u32,
        end_line: u32,
        how: Option<MoveKind>,
    },
    /// A comment on the whole of a file the review doesn't change, which stands at its new
    /// side at `path` ([`place_unlisted`]): not outdated, in no listed file.
    UnlistedFile { path: String },
}

impl Placement {
    /// The listed file the thread belongs to; `None` when its file left the list (or isn't
    /// known yet).
    pub fn file(&self) -> Option<&str> {
        match self {
            Self::Unchanged { file } | Self::Moved { file, .. } | Self::File { file } => Some(file),
            Self::Pending { file } | Self::Outdated { file, .. } => file.as_deref(),
            Self::Unlisted { .. } | Self::UnlistedFile { .. } => None,
        }
    }

    /// The path of the file the review doesn't list that the thread stands in
    /// ([`Placement::Unlisted`]).
    pub fn unlisted_path(&self) -> Option<&str> {
        match self {
            Self::Unlisted { path, .. } | Self::UnlistedFile { path } => Some(path),
            _ => None,
        }
    }

    /// Whether the thread's file is among the review's changes: placed, pending or outdated in
    /// a listed file. A thread whose file left the list, found in it or not, isn't.
    pub fn in_diff(&self) -> bool {
        self.file().is_some()
    }

    /// Where an outdated thread's code went since its round, when its snapshot tells.
    pub fn trail(&self) -> Option<&Trail> {
        match self {
            Self::Outdated { trail, .. } => trail.as_ref(),
            _ => None,
        }
    }

    /// The lines on its side the thread shows at, `(start, end)`, 1-based and inclusive:
    /// `anchor`'s own while unchanged, where it moved to, else `None` (a comment on the whole
    /// file included).
    pub fn lines(&self, anchor: &Anchor) -> Option<(u32, u32)> {
        match self {
            Self::Unchanged { .. } => Some((anchor.start_line, anchor.end_line)),
            Self::Moved {
                start_line,
                end_line,
                ..
            }
            | Self::Unlisted {
                start_line,
                end_line,
                ..
            } => Some((*start_line, *end_line)),
            Self::Pending { .. }
            | Self::Outdated { .. }
            | Self::File { .. }
            | Self::UnlistedFile { .. } => None,
        }
    }

    pub fn is_outdated(&self) -> bool {
        matches!(self, Self::Outdated { .. })
    }

    /// A comment on the whole of its file, standing in it.
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File { .. } | Self::UnlistedFile { .. })
    }

    /// `"unchanged"`, `"moved"`, `"pending"`, `"outdated"` or `"file"` (a thread on a file the
    /// review doesn't list: `"unchanged"` or `"moved"`, [`Placement::in_diff`] false).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unchanged { .. } | Self::Unlisted { how: None, .. } => "unchanged",
            Self::Moved { .. } | Self::Unlisted { how: Some(_), .. } => "moved",
            Self::Pending { .. } => "pending",
            Self::Outdated { .. } => "outdated",
            Self::File { .. } | Self::UnlistedFile { .. } => "file",
        }
    }
}

/// Whether `thread`, placed at `placement`, shows as moved: the lines it shows at now start
/// elsewhere than the ones it was written on ([`Thread::first_anchor`]), whether relocation
/// finds its code elsewhere or it stands where an earlier event recorded it moved to (PLAN
/// §8). Its code edited where it stands (a fuzzy match starting at its own first line, as many
/// lines as it has now: a line rewritten into a block), or its file renamed, moves nothing, so
/// recording either on the next event changes nothing anyone sees. The GUI's "· moved" and the
/// CLI's `placement: "moved"` both mean this.
pub fn shows_moved(thread: &Thread, placement: &Placement) -> bool {
    moved_since(thread.first_anchor(), &thread.anchor, placement)
}

/// [`shows_moved`] for a thread anchored at `anchor` now and written on `first`: one of
/// GitHub's (PLAN §9.6), whose two are the same anchor, is moved wherever relocation finds it
/// at other lines than GitHub has it at.
pub fn moved_since(first: &Anchor, anchor: &Anchor, placement: &Placement) -> bool {
    placement
        .lines(anchor)
        .is_some_and(|(start, _)| start != first.start_line)
}

/// Whether `thread`, placed at `placement`, shows its code edited where it stands (PLAN §8):
/// not moved ([`shows_moved`]), but found there by a fuzzy match, or standing at lines an
/// event recorded whose text isn't the text it was written on. The CLI's `placement:
/// "edited"`; the GUI marks nothing for it, since nothing moved.
pub fn shows_edited(thread: &Thread, placement: &Placement) -> bool {
    edited_since(thread.first_anchor(), &thread.anchor, placement)
}

/// [`shows_edited`] for a thread anchored at `anchor` now and written on `first`
/// ([`moved_since`]).
pub fn edited_since(first: &Anchor, anchor: &Anchor, placement: &Placement) -> bool {
    if moved_since(first, anchor, placement) {
        return false;
    }
    match placement {
        Placement::Moved {
            how: MoveKind::Fuzzy { .. },
            ..
        }
        | Placement::Unlisted {
            how: Some(MoveKind::Fuzzy { .. }),
            ..
        } => true,
        Placement::Unchanged { .. } | Placement::Moved { .. } | Placement::Unlisted { .. } => {
            anchor.text != first.text
        }
        Placement::Pending { .. }
        | Placement::Outdated { .. }
        | Placement::File { .. }
        | Placement::UnlistedFile { .. } => false,
    }
}

/// The listed file `anchor` belongs to (PLAN §8: renames carry anchors): the entry at its
/// path, else the entry renamed from its path, else the entry renamed from the path its file
/// had been renamed from when it was written (`anchor.old_path`: a file renamed again since,
/// which git reports as one rename from its first name). An old-side anchor on a renamed file
/// records the rename's source, which finds the same entry.
pub fn anchored_entry<'e>(anchor: &Anchor, entries: &'e [FileEntry]) -> Option<&'e FileEntry> {
    entry_for(&anchor.path, anchor.old_path.as_deref(), entries)
}

/// [`anchored_entry`]'s rule for a file at `path`, renamed from `old_path` when it was
/// written.
pub fn entry_for<'e>(
    path: &str,
    old_path: Option<&str>,
    entries: &'e [FileEntry],
) -> Option<&'e FileEntry> {
    let renamed_from = |path: &str| {
        entries
            .iter()
            .find(|entry| entry.old_path.as_deref() == Some(path))
    };
    entries
        .iter()
        .find(|entry| entry.path == path)
        .or_else(|| renamed_from(path))
        .or_else(|| old_path.and_then(renamed_from))
}

/// The listed file a thread's anchor belongs to now ([`thread_file`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadFile<'e> {
    /// That file (which may lack the anchor's side: a new-side thread on a deleted file).
    Found(&'e FileEntry),
    /// Its round's history, which would say what the file is called now, isn't read yet; the
    /// listed file at its path, when there is one (deleted).
    Pending(Option<&'e FileEntry>),
    /// Its file left the list.
    Gone,
}

/// The file rule of PLAN §8: [`anchored_entry`]'s entry (`find` looks a path up by that rule,
/// [`entry_for`] over the caller's list) when it has the anchor's side; else, for a new-side
/// anchor, the entry at the path its round's history says the file has now (`super::history`:
/// a rename git reports since the round, or a file git can't pair, untracked now, paired by
/// content), through the same rule; else the anchored entry, if any (a deleted file, where the
/// thread is outdated). `history` is the anchor's round's ([`History::None`] while that round
/// is in progress).
pub fn thread_file<'e>(
    anchor: &Anchor,
    history: History<'_>,
    find: impl Fn(&str, Option<&str>) -> Option<&'e FileEntry>,
) -> ThreadFile<'e> {
    let anchored = find(&anchor.path, anchor.old_path.as_deref());
    if let Some(entry) = anchored
        && has_side(entry, anchor.side)
    {
        return ThreadFile::Found(entry);
    }
    if anchor.side == AnchorSide::New {
        match history.of(&anchor.path) {
            PathHistory::Pending => return ThreadFile::Pending(anchored),
            PathHistory::Read(file) => {
                if let Some(entry) = file
                    .now
                    .as_deref()
                    .and_then(|now| find(now, None))
                    .filter(|entry| has_side(entry, AnchorSide::New))
                {
                    return ThreadFile::Found(entry);
                }
            }
            PathHistory::None => {}
        }
    }
    anchored.map_or(ThreadFile::Gone, ThreadFile::Found)
}

/// Whether `entry`'s file exists on `side`: an added or untracked file has no old side, a
/// deleted one no new side.
pub fn has_side(entry: &FileEntry, side: AnchorSide) -> bool {
    match side {
        AnchorSide::New => entry.status != FileStatus::Deleted,
        AnchorSide::Old => !matches!(entry.status, FileStatus::Added | FileStatus::Untracked),
    }
}

/// Where `thread` shows among `entries`, the current file list (PLAN §8): its file is
/// [`thread_file`]'s; `text` gives that file's side (the anchor's) as the caller has it, and
/// [`relocate`] finds the anchor in it. A thread whose file left the list, or whose side the
/// file no longer has, is outdated. A new-side thread relocation can't find is traced through
/// its round's snapshot (`super::history::trace`): outdated still, with the [`Trail`] to where
/// its code went when the snapshot tells. `history` is the anchor's round's history as the
/// caller has it ([`History::None`] for a round in progress); a thread that needs it before
/// it is read is `Pending`, so it never shows outdated first and then jumps. A comment on the
/// whole file (a file-level anchor) is never relocated and `text` is never asked for it: it is
/// [`Placement::File`] in its file by the same file rule, renames and round history included,
/// or outdated there when the file no longer has its side (a new-side comment on a file
/// deleted since), or outdated without a file when its file left the list.
pub fn place_thread<L: SideLines>(
    thread: &Thread,
    entries: &[FileEntry],
    history: History<'_>,
    text: impl FnOnce(&FileEntry, AnchorSide) -> SideText<L>,
) -> Placement {
    place_anchor(
        &thread.anchor,
        |path, old| entry_for(path, old, entries),
        history,
        text,
    )
}

/// [`place_thread`] for an anchor alone, its file looked up through `find` ([`thread_file`]):
/// the review's own threads by [`entry_for`] over the list, GitHub's (PLAN §9.6,
/// `crate::github::threads`) by a rule that also follows a rename back.
pub fn place_anchor<'e, L: SideLines>(
    anchor: &Anchor,
    find: impl Fn(&str, Option<&str>) -> Option<&'e FileEntry>,
    history: History<'_>,
    text: impl FnOnce(&FileEntry, AnchorSide) -> SideText<L>,
) -> Placement {
    let entry = match thread_file(anchor, history, find) {
        ThreadFile::Found(entry) => entry,
        ThreadFile::Pending(entry) => {
            return Placement::Pending {
                file: entry.map(|entry| entry.path.clone()),
            };
        }
        ThreadFile::Gone => {
            return Placement::Outdated {
                file: None,
                trail: None,
            };
        }
    };
    let file = entry.path.clone();
    let outdated = |file: String| Placement::Outdated {
        file: Some(file),
        trail: None,
    };
    if !has_side(entry, anchor.side) {
        return outdated(file);
    }
    if anchor.is_file_level() {
        return Placement::File { file };
    }
    let side = match text(entry, anchor.side) {
        SideText::Pending => return Placement::Pending { file: Some(file) },
        SideText::Unavailable => return outdated(file),
        SideText::Lines(side) => side,
    };
    let relocation = match side.fuzzy() {
        Some(fuzzy) => relocate_with(anchor, side.lines(), fuzzy),
        None => relocate(anchor, side.lines()),
    };
    match relocation {
        Relocation::Unchanged => Placement::Unchanged { file },
        Relocation::Moved {
            start_line,
            end_line,
            how,
        } => Placement::Moved {
            file,
            start_line,
            end_line,
            how,
        },
        Relocation::Outdated if anchor.side == AnchorSide::Old => outdated(file),
        Relocation::Outdated => match history.of(&anchor.path) {
            PathHistory::None => outdated(file),
            PathHistory::Pending => Placement::Pending { file: Some(file) },
            PathHistory::Read(past) => Placement::Outdated {
                file: Some(file),
                trail: past
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| trace(anchor, snapshot, side.lines()))
                    .map(|went| Trail {
                        round: anchor.round,
                        went,
                    }),
            },
        },
    }
}

/// The path a thread whose file left the list is read at (PLAN §8 "Threads on files the
/// review doesn't change"): the one its round's history says its file has now, when it says,
/// else the anchor's own.
pub fn unlisted_path(anchor: &Anchor, history: History<'_>) -> String {
    if anchor.side == AnchorSide::New
        && let PathHistory::Read(file) = history.of(&anchor.path)
        && let Some(now) = &file.now
    {
        return now.clone();
    }
    anchor.path.clone()
}

/// Where `thread` stands when [`place_thread`] found its file gone from the list
/// (`Placement::Outdated` without a file; PLAN §8 "Threads on files the review doesn't
/// change"), against the file at `path` ([`unlisted_path`]) as the review's new side holds it
/// (`crate::diff::side_file`): `present` whether that side holds a file there, `text` its lines
/// (unavailable for a file without text: binary, a submodule). The review doesn't change the
/// file, so its two sides are the same text, which anchors on either side are relocated in:
/// found there, the thread is [`Placement::Unlisted`]; a comment on the whole file of a file
/// that is there is [`Placement::UnlistedFile`]; else, the file or its text gone, outdated
/// without a file as before.
pub fn place_unlisted<L: SideLines>(
    thread: &Thread,
    path: &str,
    present: bool,
    text: SideText<L>,
) -> Placement {
    place_unlisted_anchor(&thread.anchor, path, present, text)
}

/// [`place_unlisted`] for an anchor alone.
pub fn place_unlisted_anchor<L: SideLines>(
    anchor: &Anchor,
    path: &str,
    present: bool,
    text: SideText<L>,
) -> Placement {
    let gone = Placement::Outdated {
        file: None,
        trail: None,
    };
    if matches!(text, SideText::Pending) {
        return Placement::Pending { file: None };
    }
    if !present {
        return gone;
    }
    if anchor.is_file_level() {
        return Placement::UnlistedFile {
            path: path.to_string(),
        };
    }
    let side = match text {
        SideText::Pending | SideText::Unavailable => return gone,
        SideText::Lines(side) => side,
    };
    let relocation = match side.fuzzy() {
        Some(fuzzy) => relocate_with(anchor, side.lines(), fuzzy),
        None => relocate(anchor, side.lines()),
    };
    let unlisted = |start_line, end_line, how| Placement::Unlisted {
        path: path.to_string(),
        start_line,
        end_line,
        how,
    };
    match relocation {
        Relocation::Unchanged => unlisted(anchor.start_line, anchor.end_line, None),
        Relocation::Moved {
            start_line,
            end_line,
            how,
        } => unlisted(start_line, end_line, Some(how)),
        Relocation::Outdated => gone,
    }
}

/// Whether `placement` is [`place_thread`]'s for a thread whose file left the list: the case
/// [`place_unlisted`] reads further.
pub fn is_gone(placement: &Placement) -> bool {
    matches!(placement, Placement::Outdated { file: None, .. })
}

/// The anchor the next event on a thread records (PLAN §8): `anchor` re-taken where
/// `placement` found it on its side of `entry` (the placement's file), whose current full text
/// is `lines`, in `round` (the round in progress), with fresh context. A thread that moved is
/// re-taken at the lines it moved to; one standing at its lines in a file renamed since it was
/// written is re-taken at the same lines under the file's name now, so it keeps its file once
/// the rename becomes part of the old side (a commit, a rebase). A comment on the whole file
/// placed in a file renamed since is re-taken as a file-level anchor under the name now (`lines`
/// unread). `None` otherwise; callers pass it to `ops::reply` and `ops::set_thread_state`.
pub fn relocated_anchor<S: AsRef<str>>(
    anchor: &Anchor,
    placement: &Placement,
    entry: &FileEntry,
    lines: &[S],
    round: u32,
) -> Option<Anchor> {
    let place = AnchorPlace::for_entry(entry, anchor.side, round);
    if anchor.is_file_level() {
        return (placement.is_file() && place.path != anchor.path).then(|| place.file_level());
    }
    let (start, end) = match placement {
        Placement::Moved {
            start_line,
            end_line,
            ..
        } => (*start_line, *end_line),
        Placement::Unchanged { .. } if place.path != anchor.path => {
            (anchor.start_line, anchor.end_line)
        }
        _ => return None,
    };
    anchor_selection(place, lines, start, end).ok()
}
