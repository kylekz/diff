//! Files at the review's sides, whole (PLAN §18.1 "The file view", §8): one file's text as the
//! review's new side (or, for a file it deletes, its old side) holds it
//! (`dv_core::diff::side_file`), read in the background for the file view and for the threads
//! on files the review doesn't list, and the paths the new side holds, for `Ctrl P`.
//!
//! **Reads.** A file is read when the file view shows it (highlighted then too, `crate::syntax`)
//! or when a live thread's file left the review's list ([`ReviewSession::ensure_side_reads`],
//! after a listing, a fold, a history read; at the path its round's history says the file has
//! now), and kept while the target resolves to the same sides and no refresh touches its path
//! (a watch batch naming it, a git change, a rescan). It is read whole, without the size caps,
//! as the CLI reads it, so a large file places its threads alike; the file view applies the caps
//! to what it shows ("Load anyway"). Read again, it keeps placing threads and showing in the
//! file view until the new read lands, so nothing flickers; a read that fails says why to the
//! file view and leaves its threads unplaced by it (outdated without a file, as before).
//!
//! **The working tree.** The file view also shows a file as the working tree holds it now
//! ([`ReviewSession::worktree_file`]), whatever the review's sides are: where go to definition
//! and references go in a review whose new side isn't the working tree, since a language server
//! reads the working tree (PLAN §18.2). Read the same way, kept apart from the sides' reads.
//!
//! **The disk's reads** (the working tree as it stands, a file outside the repository) are
//! never covered by the watch of a review whose new side isn't the working tree, so the file
//! view's is read again each time it opens and when the window comes back to the front
//! ([`ReviewSession::read_disk_view_again`]), a file outside the repository only once a look
//! (no bytes) finds its length changed; one the file view leaves is dropped, and a read
//! at a side loses its highlights once the file view leaves it (placement keeps its text). A
//! file over the size caps (`DiffLimits`), which the file view shows only after "Load anyway",
//! isn't highlighted.
//!
//! **Outside the repository.** Go to definition into a file outside the repository (a
//! library's declarations) shows it read-only ([`ReviewSession::outside_file`]): read through
//! `Backend::probe_path` (up to `PROBE_READ_MAX`; links on the way followed, as a language
//! server reports a file by its real path anyway), every line a context line, highlighted. A
//! larger file shows the too-large notice; one that isn't there, a read that failed.
//!
//! **Paths.** The side's paths are read once per review open and round, when `Ctrl P` first
//! asks ([`ReviewSession::ensure_side_paths`]), and kept; the finder adds the review's own
//! files to them, so a file the agent created since shows too.

use std::collections::HashMap;
use std::sync::Arc;

use dv_core::backend::{PROBE_READ_MAX, PathProbe};
use dv_core::diff::{
    DiffError, DiffLimits, DiffLine, FileDiff, LineKind, TextDiff, side_file, side_paths,
};
use dv_core::review::{AnchorSide, SideText, ThreadFile, unlisted_path};
use dv_core::{Backend, BackendError, Side};
use gpui::{AppContext as _, Context, Task};

use super::load::Batch;
use super::placement::KeptSide;
use super::{ReviewSession, SessionEvent, Shown};
use crate::failure::Failure;
use crate::syntax::{FileHighlights, highlight_diff, language_for_path};

/// Where a read reads a file: one of the review's sides, the working tree as it stands, or
/// (an absolute path) outside the repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReadAt {
    Side(AnchorSide),
    Worktree,
    Outside,
}

/// Where the file view reads its file (PLAN §18.1 "The file view", "Go to definition").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ViewFrom {
    /// At the review's side: the stream's diff of a listed file, else read at the side.
    #[default]
    Review,
    /// As the working tree holds it now, read-only (module docs, "The working tree").
    Worktree,
    /// A file outside the repository, by its absolute path, read-only (module docs).
    Outside,
}

/// One file read at one of the review's sides.
pub(super) struct SideRead {
    /// The side it is read at: a read at another one is dropped.
    side: Side,
    /// Bumped by every read that lands, so placement knows when to work a thread out again.
    pub version: u64,
    /// The last read that landed: `Some(None)` when the side holds no file at the path.
    read: Option<Result<Option<Arc<FileDiff>>, Failure>>,
    highlights: Option<Arc<FileHighlights>>,
    /// The file view shows it: it is highlighted.
    highlight: bool,
    /// A read under way, which a newer one replaces.
    loading: Option<(u64, Task<()>)>,
    highlighting: Option<Task<()>>,
    /// A file outside the repository: its length when it was read, which a look first
    /// compares before it is read again (module docs, "The disk's reads").
    len: Option<u64>,
    /// That look, under way.
    looking: Option<Task<()>>,
}

/// Where a file at the new side stands, as the file view shows it.
#[derive(Clone, Debug)]
pub enum SideFileState {
    /// Not read yet (or read again, with nothing read before).
    Loading,
    /// The side holds no file at that path.
    Missing,
    Loaded(Shown),
    Failed(Failure),
}

/// The side's paths, read once (module docs, "Paths").
pub(super) struct SidePaths {
    key: (Side, u32),
    paths: Option<Arc<Vec<String>>>,
    _task: Option<Task<()>>,
}

impl ReviewSession {
    /// The review's new side, once the target resolved.
    pub fn new_side(&self) -> Option<&Side> {
        self.resolved.as_ref().map(|spec| &spec.new)
    }

    /// The review's `which` side (its target resolved: all changes, an interdiff's too), once
    /// it resolved.
    fn review_side(&self, which: AnchorSide) -> Option<&Side> {
        self.resolved.as_ref().map(|spec| match which {
            AnchorSide::Old => &spec.old,
            AnchorSide::New => &spec.new,
        })
    }

    /// The side `at` reads: the review's `which`, or the working tree.
    fn read_side(&self, at: ReadAt) -> Option<Side> {
        match at {
            ReadAt::Side(which) => self.review_side(which).cloned(),
            // A file outside the repository doesn't depend on the review's sides.
            ReadAt::Worktree | ReadAt::Outside => Some(Side::Worktree),
        }
    }

    /// Where `path` stands at the review's `which` side, as [`ReviewSession::ensure_side_file`]
    /// reads it.
    pub fn side_file(&self, which: AnchorSide, path: &str) -> SideFileState {
        self.read_state(ReadAt::Side(which), path)
    }

    /// Where `path` stands in the working tree, as [`ReviewSession::ensure_worktree_file`]
    /// reads it (module docs, "The working tree").
    pub fn worktree_file(&self, path: &str) -> SideFileState {
        self.read_state(ReadAt::Worktree, path)
    }

    /// Where the file outside the repository at `path` (absolute) stands, as
    /// [`ReviewSession::ensure_outside_file`] reads it (module docs, "Outside the repository").
    pub fn outside_file(&self, path: &str) -> SideFileState {
        self.read_state(ReadAt::Outside, path)
    }

    /// Reads the file outside the repository at `path` (absolute) unless it is read, or being
    /// read, and highlights it.
    pub fn ensure_outside_file(&mut self, path: &str, cx: &mut Context<Self>) {
        self.ensure_read(ReadAt::Outside, path, true, cx);
    }

    /// Reads `path` where `at` (the working tree, outside) says, highlighted: at once when it
    /// wasn't read, again when it was (the read before shows until this one lands). A file
    /// outside the repository is only looked at first, and read again only when its length
    /// changed (it is read whole, up to 4 MiB, across the distro's connection).
    fn read_from_disk(&mut self, at: ReadAt, path: &str, cx: &mut Context<Self>) {
        let key = (at, path.to_string());
        let Some(side) = self.read_side(at) else {
            return;
        };
        let (busy, len) = match self.side_reads.get(&key) {
            Some(read) => (
                read.loading.is_some() || read.looking.is_some(),
                read.len.filter(|_| read.read.is_some()),
            ),
            None => (false, None),
        };
        if busy {
            return;
        }
        let Some(was) = len.filter(|_| at == ReadAt::Outside) else {
            self.start_side_read(key, side, true, cx);
            return;
        };
        let (backend, read_path) = (self.backend.clone(), key.1.clone());
        let job = cx.background_spawn(async move { outside_len(backend.as_ref(), &read_path) });
        let task = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let now = job.await;
                this.update(cx, |this, cx| {
                    let Some(read) = this.side_reads.get_mut(&key) else {
                        return;
                    };
                    read.looking = None;
                    if now != Some(was) {
                        this.start_side_read(key, side, true, cx);
                    }
                })
                .ok();
            }
        });
        if let Some(read) = self.side_reads.get_mut(&key) {
            read.looking = Some(task);
        }
    }

    /// The file view's file read from the disk is read again (module docs, "The disk's
    /// reads"): the window came back to the front.
    pub(super) fn read_disk_view_again(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.file_view.clone() else {
            return;
        };
        match self.file_view_from {
            ViewFrom::Worktree => self.read_from_disk(ReadAt::Worktree, &path, cx),
            ViewFrom::Outside => self.read_from_disk(ReadAt::Outside, &path, cx),
            ViewFrom::Review => {}
        }
    }

    /// The file view left `path` (read `from`): a read of the disk is dropped, a read at a
    /// side loses its highlights (module docs, "The disk's reads").
    pub(super) fn left_file_view(&mut self, path: &str, from: ViewFrom) {
        match from {
            ViewFrom::Worktree => {
                self.side_reads
                    .remove(&(ReadAt::Worktree, path.to_string()));
            }
            ViewFrom::Outside => {
                self.side_reads.remove(&(ReadAt::Outside, path.to_string()));
            }
            ViewFrom::Review => {
                for side in [AnchorSide::Old, AnchorSide::New] {
                    if let Some(read) = self
                        .side_reads
                        .get_mut(&(ReadAt::Side(side), path.to_string()))
                    {
                        read.highlight = false;
                        read.highlights = None;
                        read.highlighting = None;
                    }
                }
            }
        }
    }

    fn read_state(&self, at: ReadAt, path: &str) -> SideFileState {
        let Some(read) = self.side_reads.get(&(at, path.to_string())) else {
            return SideFileState::Loading;
        };
        match &read.read {
            None => SideFileState::Loading,
            Some(Ok(None)) => SideFileState::Missing,
            Some(Ok(Some(diff))) => SideFileState::Loaded(Shown {
                diff: diff.clone(),
                highlights: read.highlights.clone(),
            }),
            Some(Err(failure)) => SideFileState::Failed(failure.clone()),
        }
    }

    /// Reads `path` at the review's `which` side unless it is read, or being read; `highlight`
    /// (the file view shows it) highlights it too.
    pub fn ensure_side_file(
        &mut self,
        which: AnchorSide,
        path: &str,
        highlight: bool,
        cx: &mut Context<Self>,
    ) {
        self.ensure_read(ReadAt::Side(which), path, highlight, cx);
    }

    /// Reads `path` as the working tree holds it unless it is read, or being read, and
    /// highlights it (module docs, "The working tree").
    pub fn ensure_worktree_file(&mut self, path: &str, cx: &mut Context<Self>) {
        self.ensure_read(ReadAt::Worktree, path, true, cx);
    }

    fn ensure_read(&mut self, at: ReadAt, path: &str, highlight: bool, cx: &mut Context<Self>) {
        let Some(side) = self.read_side(at) else {
            return;
        };
        let key = (at, path.to_string());
        match self.side_reads.get_mut(&key) {
            Some(read) if read.side == side => {
                if highlight && !read.highlight {
                    read.highlight = true;
                    self.start_side_highlight(&key, cx);
                }
            }
            _ => self.start_side_read(key, side, highlight, cx),
        }
    }

    /// `path`'s text at the new side as placement reads it ([`dv_core::review::place_unlisted`]):
    /// its lines once read (the last read while another runs), pending before the first read
    /// lands, unavailable when the side holds no text there or the read failed.
    pub(super) fn side_text(&self, path: &str) -> SideText<KeptSide<'_>> {
        match self
            .side_reads
            .get(&(ReadAt::Side(AnchorSide::New), path.to_string()))
            .and_then(|read| read.read.as_ref())
        {
            None => SideText::Pending,
            Some(Ok(Some(diff))) => self.lines_of(path, AnchorSide::New, diff),
            Some(Ok(None) | Err(_)) => SideText::Unavailable,
        }
    }

    /// Whether the new side holds a file at `path`, as last read (`false` before a read lands,
    /// and when it failed).
    pub(super) fn side_present(&self, path: &str) -> bool {
        matches!(
            self.side_reads
                .get(&(ReadAt::Side(AnchorSide::New), path.to_string()))
                .and_then(|read| read.read.as_ref()),
            Some(Ok(Some(_)))
        )
    }

    /// Which read of `path` at the new side placement worked a thread out against.
    pub(super) fn side_version(&self, path: &str) -> Option<u64> {
        self.side_reads
            .get(&(ReadAt::Side(AnchorSide::New), path.to_string()))
            .map(|read| read.version)
    }

    /// Starts reading, at the new side, every file a live thread is anchored in whose file left
    /// the review's list, at the path its round's history says it has now (module docs,
    /// "Reads"). Blocking work, so in the background.
    pub(super) fn ensure_side_reads(&mut self, cx: &mut Context<Self>) {
        if !self.listed {
            return;
        }
        let wanted: Vec<String> = self
            .cards()
            .filter(|&card| {
                card.is_anchored()
                    && matches!(
                        self.card_file_with(card, self.card_history(card)),
                        ThreadFile::Gone
                    )
            })
            .map(|card| unlisted_path(card.anchor(), self.card_history(card)))
            .collect();
        for path in wanted {
            self.ensure_side_file(AnchorSide::New, &path, false, cx);
        }
    }

    /// After a listing (module docs, "Reads"): reads again every file `batch` touches, or all
    /// of them when the git state moved or the new side is another.
    pub(super) fn refresh_side_reads(&mut self, batch: &Batch, cx: &mut Context<Self>) {
        let Some(side) = self.new_side().cloned() else {
            return;
        };
        let stale: Vec<((ReadAt, String), Side, bool)> = self
            .side_reads
            .iter()
            .filter_map(|((which, path), read)| {
                let now = self.read_side(*which)?;
                (batch.rescan || batch.git || read.side != now || batch.paths.contains(path))
                    .then(|| ((*which, path.clone()), now, read.highlight))
            })
            .collect();
        for (key, now, highlight) in stale {
            self.start_side_read(key, now, highlight, cx);
        }
        let round = self.review.current_round();
        if self
            .side_paths
            .as_ref()
            .is_some_and(|paths| paths.key != (side.clone(), round))
        {
            self.side_paths = None;
        }
    }

    fn start_side_read(
        &mut self,
        key: (ReadAt, String),
        side: Side,
        highlight: bool,
        cx: &mut Context<Self>,
    ) {
        self.next_token += 1;
        let token = self.next_token;
        let (repo, read_side, read_path) = (self.repo.clone(), side.clone(), key.1.clone());
        let (outside, backend) = (key.0 == ReadAt::Outside, self.backend.clone());
        // Whole, as the CLI reads it: the file view applies the caps to what it shows.
        let job = cx.background_spawn(async move {
            if outside {
                match read_outside(backend.as_ref(), &read_path) {
                    Ok((diff, len)) => (Ok(diff), len),
                    Err(error) => (Err(DiffError::from(error)), None),
                }
            } else {
                (
                    side_file(&repo, &read_side, &read_path, &DiffLimits::UNLIMITED),
                    None,
                )
            }
        });
        let task = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let (read, len) = job.await;
                this.update(cx, |this, cx| {
                    if let Some(state) = this.side_reads.get_mut(&key)
                        && state.loading.as_ref().is_some_and(|(at, _)| *at == token)
                    {
                        state.len = len;
                    }
                    this.finish_side_read(&key, token, read, cx)
                })
                .ok();
            }
        });
        let read = self.side_reads.entry(key).or_insert_with(|| SideRead {
            side: side.clone(),
            version: 0,
            read: None,
            highlights: None,
            highlight,
            loading: None,
            highlighting: None,
            len: None,
            looking: None,
        });
        // A read at another side doesn't show meanwhile.
        if read.side != side {
            read.read = None;
            read.highlights = None;
        }
        read.side = side;
        read.highlight |= highlight;
        read.loading = Some((token, task));
    }

    fn finish_side_read(
        &mut self,
        key: &(ReadAt, String),
        token: u64,
        read: Result<Option<FileDiff>, DiffError>,
        cx: &mut Context<Self>,
    ) {
        let path = key.1.as_str();
        let Some(state) = self
            .side_reads
            .get_mut(key)
            .filter(|state| state.loading.as_ref().is_some_and(|(at, _)| *at == token))
        else {
            return;
        };
        state.loading = None;
        let read = match read {
            Ok(diff) => Ok(diff.map(Arc::new)),
            Err(error) => {
                let failure = Failure::from(&error);
                if self.lost_connection(&failure).is_some() {
                    // The reconnect's rescan reads it again; meanwhile it shows what it had.
                    cx.notify();
                    return;
                }
                log::warn!(
                    "review {}: couldn't read {path} at the review's side: {error}",
                    self.review.id
                );
                Err(failure)
            }
        };
        let Some(state) = self.side_reads.get_mut(key) else {
            return;
        };
        let same = match (&state.read, &read) {
            (Some(Ok(Some(old))), Ok(Some(new))) => old == new,
            _ => false,
        };
        if !same {
            state.read = Some(read);
            state.highlights = None;
            state.version += 1;
        }
        if state.highlight && state.highlights.is_none() {
            self.start_side_highlight(key, cx);
        }
        cx.emit(SessionEvent::FileLoaded(path.to_string()));
        cx.notify();
    }

    /// Highlights the read `key` names in the background, when its file has a grammar.
    fn start_side_highlight(&mut self, key: &(ReadAt, String), cx: &mut Context<Self>) {
        let path = key.1.as_str();
        let Some(diff) = self
            .side_reads
            .get(key)
            .and_then(|read| read.read.as_ref())
            .and_then(|read| read.as_ref().ok())
            .and_then(Option::as_ref)
            .cloned()
        else {
            return;
        };
        if language_for_path(path).is_none() || diff.as_text().is_none_or(over_caps) {
            return;
        }
        let job = cx.background_spawn({
            let (path, diff) = (path.to_string(), diff.clone());
            async move {
                diff.as_text()
                    .map(|text| highlight_diff(&path, None, text))
                    .unwrap_or_default()
            }
        });
        let task = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let highlights = job.await;
                this.update(cx, |this, cx| {
                    let path = key.1.clone();
                    let Some(read) = this.side_reads.get_mut(&key) else {
                        return;
                    };
                    read.highlighting = None;
                    let current =
                        matches!(&read.read, Some(Ok(Some(now))) if Arc::ptr_eq(now, &diff));
                    if current {
                        read.highlights = Some(Arc::new(highlights));
                        cx.emit(SessionEvent::FileLoaded(path.clone()));
                        cx.notify();
                    }
                })
                .ok();
            }
        });
        if let Some(read) = self.side_reads.get_mut(key) {
            read.highlighting = Some(task);
        }
    }

    /// Whether a read or a highlight of a file at the new side is under way (automation's
    /// `settled`).
    pub(super) fn side_reads_loading(&self) -> bool {
        self.side_reads.values().any(|read| {
            read.loading.is_some() || read.highlighting.is_some() || read.looking.is_some()
        }) || self
            .side_paths
            .as_ref()
            .is_some_and(|paths| paths.paths.is_none())
    }

    /// The paths the new side holds, once read ([`ReviewSession::ensure_side_paths`]).
    pub fn side_paths(&self) -> Option<&Arc<Vec<String>>> {
        self.side_paths.as_ref()?.paths.as_ref()
    }

    /// Reads the paths the new side holds unless they are read for this round, or being read
    /// (module docs, "Paths").
    pub fn ensure_side_paths(&mut self, cx: &mut Context<Self>) {
        let Some(side) = self.new_side().cloned() else {
            return;
        };
        let key = (side.clone(), self.review.current_round());
        if self
            .side_paths
            .as_ref()
            .is_some_and(|paths| paths.key == key)
        {
            return;
        }
        let repo = self.repo.clone();
        let job = cx.background_spawn(async move { side_paths(&repo, &side) });
        let task = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let read = job.await;
                this.update(cx, |this, cx| {
                    let Some(paths) = this.side_paths.as_mut().filter(|paths| paths.key == key)
                    else {
                        return;
                    };
                    paths._task = None;
                    paths.paths = Some(Arc::new(read.unwrap_or_else(|error| {
                        log::warn!("listing the files of the review's new side: {error}");
                        Vec::new()
                    })));
                    cx.notify();
                })
                .ok();
            }
        });
        self.side_paths = Some(SidePaths {
            key,
            paths: None,
            _task: Some(task),
        });
    }
}

/// The reads kept per side and path.
pub(super) type SideReads = HashMap<(ReadAt, String), SideRead>;

/// Whether `text` is over the size caps the file view shows a file under (`DiffLimits`): it
/// isn't highlighted (module docs, "The disk's reads").
fn over_caps(text: &TextDiff) -> bool {
    let limits = DiffLimits::default();
    let bytes: u64 = text
        .lines
        .iter()
        .map(|line| line.text.len() as u64 + 1)
        .sum();
    text.lines.len() > limits.max_lines || bytes > limits.max_bytes
}

/// The file outside the repository at `path` (absolute on the backend's machine), as the file
/// view shows it (module docs, "Outside the repository"), and its length: its text, every line
/// a context line; too large past what `probe_path` reads; binary with a NUL early on; `None`
/// when nothing is there (or a directory, or a link dv can't follow). The links on the way are
/// only looked at; the file itself is read once. Blocking.
pub fn read_outside(
    backend: &dyn Backend,
    path: &str,
) -> Result<(Option<FileDiff>, Option<u64>), BackendError> {
    let Some(real) = outside_real(backend, path)? else {
        return Ok((None, None));
    };
    Ok(match backend.probe_path(&real, PROBE_READ_MAX)? {
        PathProbe::File {
            len,
            contents: Some(bytes),
        } => (Some(text_file(&bytes)), Some(len)),
        PathProbe::File {
            len,
            contents: None,
        } => (
            Some(FileDiff::TooLarge {
                old_len: None,
                new_len: Some(len),
            }),
            Some(len),
        ),
        PathProbe::Missing | PathProbe::Dir | PathProbe::Link { .. } => (None, None),
    })
}

/// The length of the file outside the repository at `path` now, only looked at (the links on
/// the way followed); `None` when there is none. Blocking.
fn outside_len(backend: &dyn Backend, path: &str) -> Option<u64> {
    let real = outside_real(backend, path).ok()??;
    match backend.probe_path(&real, 0).ok()? {
        PathProbe::File { len, .. } => Some(len),
        _ => None,
    }
}

/// Where the file at `path` (absolute) really is, every link on the way followed, looked at
/// only; `None` when there is no file there.
fn outside_real(backend: &dyn Backend, path: &str) -> Result<Option<String>, BackendError> {
    use dv_lsp::resolve::host::real_file;
    use dv_lsp::resolve::node::{AbsPath, Reader};
    let style = dv_lsp::WorkspacePaths::new(&backend.repo().toplevel).style();
    let Some(asked) = AbsPath::parse(path, style) else {
        return Ok(None);
    };
    let reader = Reader::new(backend, style);
    Ok(real_file(&reader, &asked)?.map(|real| real.to_string()))
}

/// `bytes` as a file whose every line is unchanged (both sides the same text), as
/// `dv_core::diff::side_file` gives a side back: binary when a NUL shows in its first 8,000
/// bytes (git's own test), else its lines decoded leniently, `\r\n` endings noted.
fn text_file(bytes: &[u8]) -> FileDiff {
    let len = bytes.len() as u64;
    if bytes.iter().take(8000).any(|&byte| byte == 0) {
        return FileDiff::Binary {
            old_len: Some(len),
            new_len: Some(len),
        };
    }
    let text = String::from_utf8_lossy(bytes);
    let eof_newline = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(&text);
    let crlf = body.contains("\r\n") || text.ends_with("\r\n");
    let lines: Vec<DiffLine> = if text.is_empty() {
        Vec::new()
    } else {
        body.split('\n')
            .enumerate()
            .map(|(at, line)| DiffLine {
                kind: LineKind::Context,
                old_no: Some(at as u32 + 1),
                new_no: Some(at as u32 + 1),
                text: line.strip_suffix('\r').unwrap_or(line).to_string(),
                words: Vec::new(),
            })
            .collect()
    };
    FileDiff::Text(TextDiff {
        lines,
        old_eof_newline: eof_newline,
        new_eof_newline: eof_newline,
        crlf,
    })
}
