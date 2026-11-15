//! GitHub's threads in a review (docs/PLAN.md §9.6): where each of the mirror's threads
//! stands in the diff a review shows, the code GitHub shows for one, and Resolve / Unresolve.
//!
//! GitHub's threads are the mirror's [`GithubThread`]s, never the review's own `Thread`s: they
//! are shown, not stored. To show one, dv gives it an [`Anchor`] of the same shape a review's
//! own thread has ([`anchor_threads`]) and places that anchor with the one relocation and
//! placement the review's threads go through (`crate::review::relocate`, [`place`]), so the
//! app and the CLI agree about GitHub's threads as they do about the review's.
//!
//! - **A thread on the whole file** has a file-level anchor: on the new side, or the old side
//!   of a file the pull request removes.
//! - **A line thread GitHub doesn't call outdated** has its line in its comments' commit, the
//!   pull request's head as the mirror was read. Its anchor is that commit's text at those
//!   lines with full context, so it stands exactly there when the review shows that head and
//!   is relocated when it shows another (a draft whose pins a refresh didn't move, a submitted
//!   review): nothing takes the mirror's head for the head a review shows (PLAN §9.5).
//! - **A line thread GitHub calls outdated** has no line in the head any more. When the commit
//!   it was made on is the head the review shows, its anchor is that commit's text at its
//!   original lines; otherwise it has no place in the diff, and shows in the "Not in the diff"
//!   section as "Outdated on GitHub" with its diff hunk ([`Standing::OutdatedOnGithub`]).
//! - **The old side** (GitHub's `LEFT`) of a commit is the old side of the pull request's diff
//!   at that commit: the review's own old side when the commit is the head it shows, the
//!   mirror's merge base when it is the mirror's head. A renamed file is read there under the
//!   name GitHub's file list says it had.
//! - **GitHub's own hunk is the check** ([`agreement`]). Which commit a thread's line counts
//!   in is GitHub's to say and dv's to assume, so the text read is held against the
//!   comment's diff hunk, which ends at the thread's last line: the thread's last line must
//!   be the hunk's last row on the thread's side. Every place the line may count in is read
//!   ([`candidates`], in a fixed order: the mirror's head after the comment's commit; for
//!   the old side the mirror's merge base, then the review's old side), and of those that
//!   pass, the one whose lines above the thread's last agree with the most of the hunk's
//!   rows above its last is the anchor, the first of equals: a last line that is `}` or
//!   blank in two commits doesn't put the thread in the wrong one. A hunk without a row on
//!   that side checks nothing.
//! - **What can't be placed** (the commit isn't in the clone, the file isn't text, the line is
//!   past its end, an old side dv can't tell, no candidate agrees with the hunk) leaves the
//!   thread without a place in the diff, with its diff hunk ([`Standing::Unplaced`]): never
//!   a card on code the comment isn't about.
//! - **A range whose first line is on the other side** than its last is anchored at its last
//!   line alone: an anchor has one side.
//!
//! **The work** is one read of a file's text per commit and path a line thread may count in
//! (`crate::diff::side_file`: two to four git calls), never one per thread, and none for a
//! thread on the whole file. [`ThreadTexts`] keeps what was read, so anchoring again for the
//! same head and sides (after a Resolve, which changes no code) reads nothing; a read that
//! couldn't be made is not kept as a miss.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value};

use super::error::{GithubError, unexpected};
use super::gh::{Gh, Mutation};
use super::mirror::store::{MirrorRead, MirrorStore};
use super::mirror::{FileStatus, GithubThread, Mirror, MirrorKey, ThreadLines, ThreadSubject};
use super::refresh::{Refreshed, refresh};
use crate::backend::{Backend, BackendError};
use crate::diff::{DiffError, DiffLimits, FileDiff, FileEntry, LineKind, side_file};
use crate::git::{GitError, GitRepo, Oid};
use crate::review::{
    ANCHOR_CONTEXT, Anchor, AnchorPlace, AnchorSide, History, Placement, Review, SideLines,
    SideText, anchor_selection, entry_for, place_anchor, side_lines,
};
use crate::target::{DiffSpec, Side};

/// The round a GitHub thread's anchor is in: a PR review has one dv round (PLAN §9.5).
const ROUND: u32 = 1;

/// One of GitHub's threads as dv places it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubAnchor {
    /// Where GitHub has the thread: its file, side and lines, with the code there and its
    /// context when [`Standing::Anchored`]; else its original lines, with the commented lines
    /// of its diff hunk as the text and no context.
    pub anchor: Anchor,
    pub standing: Standing,
}

/// Whether a GitHub thread has an anchor to place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Standing {
    /// Its anchor is the code it is on, read from a commit: placement finds it in the diff
    /// shown.
    Anchored,
    /// GitHub calls it outdated, and it was made on another commit than the head the review
    /// shows: it has no place in the diff.
    OutdatedOnGithub,
    /// dv can't tell which code it is on (module docs, "What can't be placed"): it has no
    /// place in the diff.
    Unplaced,
}

/// The anchor of each of `mirror`'s threads, in their order, for a review showing `shown`:
/// its pins' sides (`crate::review::pins::resolve`), never another comparison of the review
/// (an interdiff's old side isn't the pull request's, [`old_side`]). Blocking: what
/// `texts` doesn't hold yet (module docs, "The work"), through the repository's backend.
pub fn anchor_threads(
    git: &GitRepo,
    mirror: &Mirror,
    shown: &DiffSpec,
    texts: &ThreadTexts,
) -> Vec<GithubAnchor> {
    mirror
        .threads
        .iter()
        .map(|thread| anchor_thread(git, mirror, shown, texts, thread))
        .collect()
}

/// The text of the files GitHub's threads are on, kept between anchorings (module docs, "The
/// work"): a commit's file never changes, so it is read once for as long as the pull
/// request's head and merge base and the sides shown stay the same; what was read for
/// others is dropped, a fetch having come between.
///
/// A miss is kept only when it is a fact about the commit: no file there, a file that isn't
/// text, or git saying that the commit isn't in the clone ([`is_missing_object`]). A read
/// that couldn't be made (the connection lost, a timeout, git not started, git killed or
/// failing for any other reason) says nothing about the commit and is asked again by the
/// next anchoring.
#[derive(Debug, Default)]
pub struct ThreadTexts {
    kept: Mutex<Kept>,
}

/// What a [`ThreadTexts`] holds texts for: the mirror's head and merge base, and the sides
/// shown.
type Basis = (Oid, Option<Oid>, DiffSpec);

#[derive(Debug, Default)]
struct Kept {
    basis: Option<Basis>,
    /// A tree's file by side and path: its lines, or `None` for one that isn't there, isn't
    /// text, or is in a commit the clone doesn't hold.
    read: HashMap<(Side, String), Option<Arc<Vec<String>>>>,
}

impl ThreadTexts {
    /// Keeps what was read only when it was read for `basis`.
    fn keep_for(&self, basis: &Basis) {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        if kept.basis.as_ref() != Some(basis) {
            kept.read.clear();
            kept.basis = Some(basis.clone());
        }
    }

    /// The lines of `path` at `side`, as the diff pipeline reads them, for an anchoring on
    /// `basis`; `None` for a file that isn't there, isn't text, or can't be read. What is
    /// read is kept only while `basis` is still what the texts are kept for: an anchoring
    /// that another, for a newer mirror or other sides, has overtaken leaves nothing behind.
    fn lines(
        &self,
        git: &GitRepo,
        basis: &Basis,
        side: &Side,
        path: &str,
    ) -> Option<Arc<Vec<String>>> {
        let key = (side.clone(), path.to_string());
        let kept = self
            .kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .read
            .get(&key)
            .cloned();
        if let Some(lines) = kept {
            return lines;
        }
        let (lines, is_fact) = match side_file(git, side, path, &DiffLimits::UNLIMITED) {
            Ok(read) => (
                read.as_ref().and_then(FileDiff::as_text).map(|text| {
                    Arc::new(
                        side_lines(text, AnchorSide::New)
                            .into_iter()
                            .map(str::to_string)
                            .collect(),
                    )
                }),
                true,
            ),
            Err(error) => {
                log::debug!("{path} at {side:?}: {error}");
                (None, is_missing_object(&error))
            }
        };
        // Only a tree's file stays as it was read.
        if is_fact && side.is_tree() {
            let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
            if kept.basis.as_ref() == Some(basis) {
                kept.read.insert(key, lines.clone());
            }
        }
        lines
    }
}

/// What git says, under the runner's `LC_ALL=C`, of an object or a revision the repository
/// doesn't hold, when it exits 128.
const MISSING_OBJECT: [&str; 4] = [
    "bad object",
    "bad revision",
    "unknown revision",
    "Not a valid object name",
];

/// Whether `error` is git saying that an object it was asked about isn't in the repository
/// (exit 128 and one of [`MISSING_OBJECT`]): a fact about the clone, as far as an anchoring
/// goes. Any other failure of git (killed by a signal, out of memory, an I/O error, a pack
/// that went under a gc) may not happen again.
fn is_missing_object(error: &DiffError) -> bool {
    match error {
        DiffError::Git(GitError::UnknownRevision { .. }) => true,
        DiffError::Git(GitError::Backend(BackendError::Git {
            code: Some(128),
            stderr,
            ..
        })) => MISSING_OBJECT.iter().any(|said| stderr.contains(said)),
        _ => false,
    }
}

/// The anchor of `thread`, one of `mirror`'s, for a review showing `shown`
/// ([`anchor_threads`], for one thread: only its own file is read).
pub fn anchor_thread(
    git: &GitRepo,
    mirror: &Mirror,
    shown: &DiffSpec,
    texts: &ThreadTexts,
    thread: &GithubThread,
) -> GithubAnchor {
    let basis = (
        mirror.pr.head_oid.clone(),
        mirror.merge_base_oid.clone(),
        shown.clone(),
    );
    texts.keep_for(&basis);
    let file = mirror.files.iter().find(|file| file.path == thread.path);
    let previous = file.and_then(|file| file.previous_path.clone());
    let lines = match &thread.subject {
        ThreadSubject::File => {
            let side = match file.map(|file| file.status) {
                Some(FileStatus::Removed) => AnchorSide::Old,
                _ => AnchorSide::New,
            };
            return GithubAnchor {
                anchor: Anchor::file_level(ROUND, thread.path.clone(), previous, side),
                standing: Standing::Anchored,
            };
        }
        ThreadSubject::Lines(lines) => lines,
    };
    let place = AnchorPlace {
        round: ROUND,
        path: match (lines.side, &previous) {
            (AnchorSide::Old, Some(previous)) => previous.clone(),
            _ => thread.path.clone(),
        },
        old_path: previous,
        side: lines.side,
    };
    let hunk = thread
        .comments
        .first()
        .map_or("", |comment| comment.diff_hunk.as_str());
    let (sides, range, standing) = candidates(thread, lines, mirror, shown);
    // The candidate that agrees with the hunk over the most rows, the first of equals.
    let mut best: Option<(usize, Anchor)> = None;
    if let Some((start, end)) = range {
        for side in &sides {
            let Some(text) = texts.lines(git, &basis, side, &place.path) else {
                continue;
            };
            let Ok(anchor) = anchor_selection(place.clone(), &text, start, end) else {
                continue;
            };
            let agreed = text
                .get(..end as usize)
                .and_then(|through| agreement(through, hunk, lines.side));
            if let Some(rows) = agreed
                && best.as_ref().is_none_or(|(most, _)| rows > *most)
            {
                best = Some((rows, anchor));
            }
        }
    }
    match best {
        Some((_, anchor)) => GithubAnchor {
            anchor,
            standing: Standing::Anchored,
        },
        None => GithubAnchor {
            anchor: unplaced(place, lines, thread),
            standing,
        },
    }
}

/// Where a line thread's code may be read from: the sides its lines may count in, the
/// likeliest first (module docs), those lines, and what the thread is when none of them
/// holds its code.
fn candidates(
    thread: &GithubThread,
    lines: &ThreadLines,
    mirror: &Mirror,
    shown: &DiffSpec,
) -> (Vec<Side>, Option<(u32, u32)>, Standing) {
    let first = thread.comments.first();
    if thread.outdated {
        // No line in the head any more: only the commit it was made on has it, and only
        // when the review shows that commit does it have a place.
        let original = first
            .and_then(|comment| comment.original_commit.clone())
            .map(Side::Commit)
            .filter(|original| *original == shown.new);
        let sides = match (original, lines.side) {
            (Some(original), AnchorSide::New) => vec![original],
            (Some(_), AnchorSide::Old) => vec![shown.old.clone()],
            (None, _) => Vec::new(),
        };
        return (
            sides,
            range(lines, |start| start.original_line, lines.original_line),
            Standing::OutdatedOnGithub,
        );
    }
    let commits = [
        first.and_then(|comment| comment.commit.clone()),
        Some(mirror.pr.head_oid.clone()),
    ];
    let commits = commits.into_iter().flatten();
    let mut sides: Vec<Side> = Vec::new();
    let all: Vec<Side> = match lines.side {
        AnchorSide::New => commits.map(Side::Commit).collect(),
        AnchorSide::Old => commits
            .filter_map(|commit| old_side(&commit, shown, mirror))
            .chain(mirror.merge_base_oid.clone().map(Side::Commit))
            .chain([shown.old.clone()])
            .collect(),
    };
    for side in all {
        if !sides.contains(&side) {
            sides.push(side);
        }
    }
    (
        sides,
        range(lines, |start| start.line, lines.line),
        Standing::Unplaced,
    )
}

/// How far `through`, a file's lines of `side` up to a thread's last one, agrees with the
/// comment's `hunk` (module docs, "GitHub's own hunk is the check"): how many of the hunk's
/// rows of that side, counted back from its last, are those lines counted back from the
/// thread's last. `None` when the last ones differ: the thread's line isn't there. A hunk
/// without a row on that side (an empty one, a header alone) has nothing to hold the lines
/// against: 0.
fn agreement(through: &[String], hunk: &str, side: AnchorSide) -> Option<usize> {
    let mut rows = rows(hunk)
        .rev()
        .filter(|(kind, _)| on_side(*kind, side))
        .map(|(_, row)| row)
        .peekable();
    if rows.peek().is_none() {
        return Some(0);
    }
    let agreed = rows
        .zip(through.iter().rev())
        .take_while(|(row, line)| same_line(line, row))
        .count();
    (agreed > 0).then_some(agreed)
}

/// Whether `ours`, a line as dv reads it, is `theirs`, a row of GitHub's hunk: the same
/// text, a line ending's `\r` apart. Bytes that aren't UTF-8 read as U+FFFD here and
/// GitHub shows them its own way, so a line that holds one is compared by what stands
/// between them: those pieces are in GitHub's row, in order, the first at its start and the
/// last at its end, with at least one character of the row for each replacement character,
/// so a line that is nothing but replacement characters isn't any row.
pub(super) fn same_line(ours: &str, theirs: &str) -> bool {
    let (ours, theirs) = (ours.trim_end_matches('\r'), theirs.trim_end_matches('\r'));
    if !ours.contains(char::REPLACEMENT_CHARACTER) {
        return ours == theirs;
    }
    let mut pieces = ours.split(char::REPLACEMENT_CHARACTER);
    let Some(rest) = pieces.next().and_then(|first| theirs.strip_prefix(first)) else {
        return false;
    };
    let Some(rest) = pieces.next_back().and_then(|last| rest.strip_suffix(last)) else {
        return false;
    };
    pieces
        .try_fold(rest, |rest, piece| {
            // What a replacement character stands for: one character of the row at least.
            let mut characters = rest.chars();
            characters.next()?;
            let rest = characters.as_str();
            rest.find(piece).map(|at| &rest[at + piece.len()..])
        })
        .is_some_and(|rest| !rest.is_empty())
}

/// A line thread's range, `(start, end)`: its last line `end`, and its first when it has one
/// on the same side (`start` picks the current or the original one).
fn range(
    lines: &ThreadLines,
    start: impl Fn(&super::mirror::ThreadStart) -> Option<u32>,
    end: Option<u32>,
) -> Option<(u32, u32)> {
    let end = end.filter(|&end| end > 0)?;
    let first = lines
        .start
        .as_ref()
        .filter(|first| first.side == lines.side)
        .and_then(start)
        .filter(|&first| first > 0 && first <= end);
    Some((first.unwrap_or(end), end))
}

/// The old side of the pull request's diff at `commit` (module docs). `shown` must be the
/// review's pins' sides for this to hold: the old side of any other comparison the review
/// shows (the changes since a last review) isn't the pull request's.
fn old_side(commit: &Oid, shown: &DiffSpec, mirror: &Mirror) -> Option<Side> {
    if shown.new == Side::Commit(commit.clone()) {
        return Some(shown.old.clone());
    }
    (*commit == mirror.pr.head_oid)
        .then(|| mirror.merge_base_oid.clone().map(Side::Commit))
        .flatten()
}

/// The anchor of a line thread without a place in the diff: where GitHub had it (its original
/// lines, else its current ones), with the commented lines of its diff hunk as its text.
fn unplaced(place: AnchorPlace, lines: &ThreadLines, thread: &GithubThread) -> Anchor {
    let (start, end) = range(lines, |start| start.original_line, lines.original_line)
        .or_else(|| range(lines, |start| start.line, lines.line))
        .unwrap_or((1, 1));
    let hunk = thread
        .comments
        .first()
        .map_or("", |comment| comment.diff_hunk.as_str());
    Anchor {
        round: place.round,
        path: place.path,
        old_path: place.old_path,
        side: place.side,
        start_line: start,
        end_line: end,
        text: hunk_rows(hunk, lines.side, end - start + 1)
            .into_iter()
            .filter(|row| row.commented)
            .map(|row| row.text)
            .collect(),
        context_before: Vec::new(),
        context_after: Vec::new(),
    }
}

/// One row of the code GitHub shows above a comment ([`hunk_rows`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkRow {
    /// Whether the row is on both sides, the new one alone or the old one alone.
    pub kind: LineKind,
    /// The line, without its marker.
    pub text: String,
    /// One of the lines the thread is on.
    pub commented: bool,
}

/// The end of a comment's diff hunk as GitHub shows it above the comment: the last `count`
/// lines of `side` are the commented ones (GitHub cuts the hunk at the thread's last line),
/// with up to [`ANCHOR_CONTEXT`] rows before the first of them. The hunk's header and its "no
/// newline" notes are left out.
pub fn hunk_rows(diff_hunk: &str, side: AnchorSide, count: u32) -> Vec<HunkRow> {
    let mut rows: Vec<HunkRow> = rows(diff_hunk)
        .map(|(kind, text)| HunkRow {
            kind,
            text: text.to_string(),
            commented: false,
        })
        .collect();
    let mut left = count;
    let mut first = rows.len();
    for (index, row) in rows.iter_mut().enumerate().rev() {
        if left == 0 {
            break;
        }
        if on_side(row.kind, side) {
            row.commented = true;
            first = index;
            left -= 1;
        }
    }
    rows.split_off(first.saturating_sub(ANCHOR_CONTEXT))
}

/// The rows of code in a diff hunk as GitHub gives one (a unified diff's hunk): each line
/// without its marker and its line ending, the header and the "no newline" notes left out.
fn rows(diff_hunk: &str) -> impl DoubleEndedIterator<Item = (LineKind, &str)> {
    diff_hunk
        .lines()
        .filter(|line| !line.starts_with("@@") && !line.starts_with('\\'))
        .map(|line| match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            _ => (LineKind::Context, line),
        })
}

/// Whether a hunk's row of `kind` is a line of `side`.
fn on_side(kind: LineKind, side: AnchorSide) -> bool {
    match side {
        AnchorSide::New => kind != LineKind::Removed,
        AnchorSide::Old => kind != LineKind::Added,
    }
}

/// The entry of `entries` a GitHub thread's file is: the review's own rule for a thread's
/// file (`crate::review::entry_for`), and, since a GitHub thread is also placed in an older
/// head than the one it was read at, the entry still under the name the file is renamed from
/// there (`old_path`).
pub fn thread_entry<'e>(
    path: &str,
    old_path: Option<&str>,
    entries: &'e [FileEntry],
) -> Option<&'e FileEntry> {
    entry_for(path, old_path, entries)
        .or_else(|| old_path.and_then(|old| entries.iter().find(|entry| entry.path == old)))
}

/// Where a GitHub thread anchored as `anchored` shows among the files of the diff a review
/// shows (PLAN §9.6), `find` looking one of them up by [`thread_entry`]'s rule and `text`
/// giving a file's side as for `crate::review::place_thread`. A thread without a place is
/// outdated without a file: the "Not in the diff" section's. One whose file the diff doesn't
/// list comes back the same way (`crate::review::is_gone`), for the caller to read that file
/// at the review's new side (`crate::review::place_unlisted_anchor`), as for a review's own
/// thread, when it is [`Standing::Anchored`].
pub fn place<'e, L: SideLines>(
    anchored: &GithubAnchor,
    find: impl Fn(&str, Option<&str>) -> Option<&'e FileEntry>,
    text: impl FnOnce(&FileEntry, AnchorSide) -> SideText<L>,
) -> Placement {
    match anchored.standing {
        Standing::Anchored => place_anchor(&anchored.anchor, find, History::None, text),
        Standing::OutdatedOnGithub | Standing::Unplaced => Placement::Outdated {
            file: None,
            trail: None,
        },
    }
}

/// What a Resolve or an Unresolve did ([`set_resolved`]).
#[derive(Debug)]
pub struct Resolved {
    /// GitHub's answer: whether it calls the thread resolved now.
    pub resolved: bool,
    /// The refresh that followed. Without a mirror read after the change
    /// ([`Refreshed::fresh`]), `resolved` is all that is known of the thread's state.
    pub refreshed: Refreshed,
}

/// Resolves (`resolved`) or unresolves GitHub's thread `thread` (its node id) of `review`'s
/// pull request, whose mirror is `key`'s, then refreshes the mirror (PLAN §9.6): GitHub's
/// state, not the review's, so nothing is written to the log and any review of the pull
/// request may ask, a submitted one included.
///
/// An error is the mutation's: nothing changed. Once GitHub took it the result is its
/// answer and the refresh's outcome. Only when that refresh read no mirror from GitHub is
/// the answer written into the stored one's thread (PLAN §9.4): the mirror the refresh
/// came back with, when it is `key`'s and still the one stored (another refresh may have
/// written since); else, the refresh having come back with no mirror of `key`'s, `key`'s as
/// it is stored, which then comes back in the refresh's place when it had none. Its
/// `read_at` is kept, since nothing else of it was read again. A refresh that read the
/// mirror and failed after (a draft's pins) has GitHub's own state already.
pub fn set_resolved(
    backend: &Arc<dyn Backend>,
    gh: &Gh,
    review: &Review,
    key: &MirrorKey,
    thread: &str,
    resolved: bool,
) -> Result<Resolved, GithubError> {
    let name = if resolved {
        "resolveReviewThread"
    } else {
        "unresolveReviewThread"
    };
    let query = format!(
        "mutation($thread: ID!) {{ {name}(input: {{threadId: $thread}}) {{ thread {{ id \
         isResolved }} }} }}"
    );
    let mut variables = Map::new();
    variables.insert("thread".to_string(), Value::from(thread));
    let data = gh.mutate(&key.host, &query, &variables, Mutation::Change)?;
    let now = data
        .get(name)
        .and_then(|answer| answer.get("thread"))
        .and_then(|thread| thread.get("isResolved"))
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            unexpected(
                "api graphql",
                &format!("{name} answered without the thread's isResolved"),
                &data.to_string(),
            )
        })?;
    let mut refreshed = refresh(backend, gh, review);
    if !refreshed.fresh {
        let mirrors = MirrorStore::new(backend.clone());
        // GitHub's answer, written into `mirror`'s thread; `false` when it doesn't hold it.
        let answer = |mirror: &mut Mirror| {
            let found = mirror.threads.iter_mut().find(|found| found.id == thread);
            found.map(|found| {
                found.resolved = now;
                found.viewer_can_resolve = !now;
                found.viewer_can_unresolve = now;
            })
        };
        let stored = match mirrors.read(key) {
            Ok(MirrorRead::Found(stored)) => Some(stored),
            Ok(_) => None,
            Err(error) => {
                log::warn!("{thread}: {error}");
                None
            }
        };
        let write = |mirror: &Mirror| {
            if let Err(error) = mirrors.write(mirror) {
                log::warn!("{thread}: {error}");
            }
        };
        match refreshed
            .mirror
            .as_deref_mut()
            .filter(|mirror| mirror.key == *key)
        {
            // The mirror the refresh came back with, when it is still the one stored.
            Some(mirror) => {
                if answer(mirror).is_some()
                    && stored.is_some_and(|stored| stored.read_at == mirror.read_at)
                {
                    write(mirror);
                }
            }
            // The refresh came back with none of this login's (it failed before it read
            // the store): the stored one takes the answer, and stands in when there is
            // nothing else to show.
            None => {
                if let Some(mut stored) = stored
                    && answer(&mut stored).is_some()
                {
                    write(&stored);
                    refreshed.mirror.get_or_insert(stored);
                }
            }
        }
    }
    Ok(Resolved {
        resolved: now,
        refreshed,
    })
}
