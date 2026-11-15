//! One open review (PLAN §11 `ReviewSession`): its repository and store, the folded review,
//! the target resolved to a [`DiffSpec`], the file list, and each file's diff as it loads.
//!
//! **Loading.** The file list comes first (one background job resolves the target and runs
//! `dv_core::diff::file_list`), then file diffs on demand: the stream asks for the files around
//! its viewport ([`ReviewSession::ensure_loaded`]), at most [`MAX_LOADS_IN_FLIGHT`] load at
//! once; while the stream's find bar searches, every listed file loads after those
//! ([`ReviewSession::load_for_search`], DESIGN §4.1); and "Load anyway" reloads one file without the size caps
//! ([`ReviewSession::load_anyway`]). A loaded text diff is then highlighted in the background
//! (`crate::syntax`, token kinds only); rows paint unhighlighted until it lands. Nothing here
//! blocks the UI thread: git, file reads and the store all run in `background_spawn` jobs.
//!
//! **Staleness.** The session's load generation is the stale-completion guard (PLAN §11): a
//! file-list result is applied only if no newer refresh started, and each file load carries a
//! token that a refresh or a "Load anyway" replaces, so a load that finishes after its file was
//! invalidated is dropped. The target never changes, so nothing else can go stale. A file that
//! loads again keeps showing the diff it had until the new one lands
//! ([`ReviewSession::shown`]), so the rows under the reader — a thread's card, the composer
//! being typed in — stay where they are.
//!
//! **Live updates.** A live target (`working`, `staged`, a branch) watches the repository
//! (PLAN §5.3, `watch`): worktree paths and git-state changes are coalesced for
//! [`REFRESH_DEBOUNCE`] and refresh the spec and the file list, keeping every loaded diff whose
//! entry didn't change (same oids, totals and modes, path not in the batch) and reloading the
//! rest; an append to this review's log reloads the fold. The watch starts with the session; a
//! change made before it was registered is never reported, so whatever was read before then is
//! read again once it is. A watch that can't cover the worktree leaves a notice
//! ([`ReviewSession::watch_notice`]) and the session refreshes when the window is activated
//! instead ([`ReviewSession::window_activated`]).
//!
//! **Writes** go through `dv_core::review::ops` on a background thread, never as hand-built
//! events, and the fold reloads afterwards (`writes`); the watch reports the same append, and
//! both paths converge on the log. A reply, Resolve or Reopen on a thread that moved, or whose
//! file was renamed, records its relocated anchor (PLAN §8, `placement`). A viewed toggle that fails says why
//! ([`ReviewSession::viewed_error`]), since its checkbox alone can't.
//!
//! **A remote repository** (a WSL distro's, PLAN §5.2, `connection`): the session follows its
//! backend's connection. A call that fails because the connection was lost raises nothing (the
//! status bar says "reconnecting…"); the reconnect's synthetic rescan reads everything again.
//!
//! **A PR review** (PLAN §9.4, §9.5, `pr`): its sides are its pins, which a refresh of its
//! pull request moves; it is refreshed as it opens and when the reviewer asks, and follows a
//! refresh made by another process. GitHub's threads on the pull request show beside the
//! review's own (PLAN §9.6, `github`): read from the mirror, placed as the review's threads
//! are, never written to its log.
//!
//! **Comparison** (PLAN §2.3, `comparison`): what the files and diffs are, the interdiff since
//! a closed round or all changes. Switching ([`ReviewSession::set_comparison`]) lists again
//! from scratch under a new generation, the diffs shown meanwhile staying on screen until
//! theirs land, so the stream keeps its place; a round snapshot that went missing falls back to
//! all changes and says so ([`ReviewSession::comparison_error`]). An interdiff's listing also
//! lists the target's own files ([`ReviewSession::target_files`]): a thread belongs to the
//! review's file whatever is shown, and a comment anchors on it. Threads are placed through
//! `dv_core::review::place_thread` with the interdiff's own rule (`placement`); placement runs
//! on the UI thread, kept until what it read changes. Every file a thread shows in loads
//! whatever the stream's viewport, and the review's own uncapped diff of a file is read in the
//! background for the threads the stream can't place against what it shows (a file too large to
//! show; in an interdiff, a file it doesn't list, or an old-side thread) (`load`). A closed
//! round's history is read, also in the background, for the threads anchored in it that the
//! current text alone can't place (`history`, PLAN §8).

mod comparison;
mod connection;
mod github;
mod history;
mod load;
mod placement;
mod pr;
mod side;
mod watch;
mod writes;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use dv_core::diff::{FileDiff, FileEntry, FileStatus};
use dv_core::review::{
    Anchor, AnchorError, AnchorPlace, AnchorSide, Author, LogWarning, Review, ReviewId,
    ReviewStore, ThreadId, ThreadKey, anchor_selection, has_side, side_lines,
};
use dv_core::{Backend, DiffSpec, GitRepo, Location};
use gpui::{Context, EventEmitter, Task};

use crate::failure::Failure;
use crate::services::reviews::OpenedReview;
use crate::syntax::FileHighlights;
pub use comparison::{Comparison, comparison_available, default_comparison};
use dv_core::label::{LabelContext, side_labels, target_label};
pub use github::{Card, GithubShown, ResolveAsked};
pub use load::Batch;
use load::ReviewText;
pub use placement::ThreadPlacement;
use placement::{FileIndex, FuzzyTables};
pub use side::{SideFileState, ViewFrom};
use watch::WatchState;
use writes::ViewedIntent;

/// How many file diffs load at once.
pub const MAX_LOADS_IN_FLIGHT: usize = 4;

/// How long worktree and git changes gather before the file list refreshes, on top of the
/// watch's own coalescing (PLAN §5.3).
pub const REFRESH_DEBOUNCE: Duration = Duration::from_millis(150);

/// Where one file's diff stands.
#[derive(Clone, Debug)]
pub enum FileState {
    /// Not asked for yet, or invalidated by a refresh.
    Unloaded,
    /// A load is running; `token` tells its result from a superseded one's.
    Loading {
        token: u64,
    },
    Loaded {
        diff: Arc<FileDiff>,
        /// `None` until highlighting lands (or for a file with no grammar, or not text).
        highlights: Option<Arc<FileHighlights>>,
    },
    Failed(Failure),
}

impl FileState {
    /// `"unloaded"`, `"loading"`, `"loaded"` or `"failed"`, as automation state spells it.
    pub fn key(&self) -> &'static str {
        match self {
            Self::Unloaded => "unloaded",
            Self::Loading { .. } => "loading",
            Self::Loaded { .. } => "loaded",
            Self::Failed(_) => "failed",
        }
    }

    /// The loaded diff, if any.
    pub fn diff(&self) -> Option<&Arc<FileDiff>> {
        match self {
            Self::Loaded { diff, .. } => Some(diff),
            _ => None,
        }
    }

    /// The loaded diff's highlights, once they landed.
    pub fn highlights(&self) -> Option<&Arc<FileHighlights>> {
        match self {
            Self::Loaded { highlights, .. } => highlights.as_ref(),
            _ => None,
        }
    }

    fn is_pending(&self) -> bool {
        matches!(self, Self::Unloaded | Self::Loading { .. })
    }
}

/// A loaded diff as the stream shows it: the diff and, once they landed, its highlights.
#[derive(Clone, Debug)]
pub struct Shown {
    pub diff: Arc<FileDiff>,
    pub highlights: Option<Arc<FileHighlights>>,
}

/// The diff a file showed before it was sent to load again, and the comparison it was listed
/// under (its old side is another text under the other one, `placement`).
#[derive(Clone, Debug)]
struct Previous {
    shown: Shown,
    comparison: Comparison,
}

/// `"text"`, `"binary"`, `"too_large"`, `"submodule"` or `"unmerged"`: what a loaded diff is,
/// as automation state spells it.
pub fn diff_kind(diff: &FileDiff) -> &'static str {
    match diff {
        FileDiff::Text(_) => "text",
        FileDiff::Binary { .. } => "binary",
        FileDiff::TooLarge { .. } => "too_large",
        FileDiff::Submodule { .. } => "submodule",
        FileDiff::Unmerged => "unmerged",
    }
}

/// What a session tells its subscribers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// The spec, the file list or the files' states were replaced.
    FilesChanged,
    /// A file's diff (or its highlights) landed, or it was sent to load again ("Load anyway",
    /// Retry).
    FileLoaded(String),
    /// The folded review changed.
    ReviewChanged,
    /// A round's history landed (`history`): threads waiting for it are placed, some in
    /// another file or where their code went.
    HistoryLoaded,
    /// Refreshing failed; [`ReviewSession::error`] says why.
    Error,
    /// The watch saw these worktree paths change (`rescan`: anything may have, a reconnect's
    /// rescan); the language servers hear of them (PLAN §18.5, `crate::intel`).
    WorktreeChanged { paths: Arc<[String]>, rescan: bool },
    /// The agent's annotations were shown or hidden (`A`, PLAN §19.3): which cards the stream
    /// draws changed, nothing else.
    AnnotationsShown,
    /// What is known of the review's pull request changed: a refresh landed, or another
    /// process's was read (`pr`); GitHub's threads on it, or a Resolve of one, with it
    /// (`github`).
    PrChanged,
}

/// One open review (see the module docs).
pub struct ReviewSession {
    /// The review's home worktree's backend, where its target resolves (PLAN §2.4,
    /// `Reviews::open`).
    backend: Arc<dyn Backend>,
    /// The repository the review was opened from (a switcher row), which may be another
    /// worktree of the same repository than the home.
    origin: Location,
    repo: GitRepo,
    store: ReviewStore,
    review: Review,
    warnings: Vec<LogWarning>,
    /// Who writes the reviewer's events.
    reviewer: Author,
    labels: LabelContext,
    spec: Option<DiffSpec>,
    /// The target resolved at the last refresh: [`ReviewSession::spec`] for all changes; an
    /// interdiff keeps its new side.
    resolved: Option<DiffSpec>,
    /// What `spec`, `files` and the diffs are.
    comparison: Comparison,
    /// The comparison asked for, which the next listing is made under.
    asked: Comparison,
    /// Why the comparison asked for couldn't be shown (a missing round snapshot).
    comparison_error: Option<Failure>,
    /// The files shown, by path and by the path a renamed file was renamed from.
    list: FileIndex,
    /// In an interdiff, the target's own files (all changes), which threads belong to
    /// (`placement`); `None` when `list` is all changes.
    target_list: Option<FileIndex>,
    states: HashMap<String, FileState>,
    /// The diff each file showed before a refresh (or Retry, "Load anyway") sent it to load
    /// again; it stays on screen until the new one lands.
    previous: HashMap<String, Previous>,
    /// Where each thread, the review's own or GitHub's, was last placed (`placement`).
    placements: RefCell<HashMap<ThreadKey, placement::Cached>>,
    /// The fuzzy step's form of each side placed against, shared by its threads.
    fuzzy: RefCell<FuzzyTables>,
    /// The review's own uncapped diffs of files whose threads the stream can't place against
    /// what it shows, read for those threads, by path (`load`).
    review_texts: HashMap<String, ReviewText>,
    /// The closed rounds' histories threads are placed with, by round (`history`).
    histories: HashMap<u32, history::RoundRead>,
    /// History reads wait while this is set (tests: what shows while a history is missing).
    histories_held: bool,
    /// Files read whole at the new side, for the file view and the threads on files the review
    /// doesn't list (`side`).
    side_reads: side::SideReads,
    /// The paths the new side holds, for `Ctrl P` (`side`).
    side_paths: Option<side::SidePaths>,
    /// The file the file view shows: it loads (a listed file's diff, else its read at the new
    /// side) and is highlighted whatever the stream's viewport (`side`).
    file_view: Option<String>,
    /// Where the file view reads its file: the review, the working tree as it stands, or a
    /// file outside the repository (`side`).
    file_view_from: side::ViewFrom,
    /// How many placements were worked out (tests: what keeping them saves).
    #[cfg(test)]
    worked_out: std::cell::Cell<usize>,
    /// Files queued to load wait while this is set (tests: what shows before a load lands).
    #[cfg(test)]
    loads_held: bool,
    /// Files the reviewer loaded anyway: they keep loading without caps after a refresh.
    unlimited: HashSet<String>,
    /// The files the stream asked for, which `settled` waits on.
    wanted: Vec<String>,
    /// The stream's find bar is searching: every listed file loads, after the wanted ones and
    /// those threads show in ([`ReviewSession::load_for_search`]).
    searching: bool,
    /// The files a references list has rows in: they load whatever the viewport, so the rows
    /// can say whether the review changed them ([`ReviewSession::load_for_references`]).
    extra: HashMap<load::Extra, Vec<String>>,
    queue: VecDeque<String>,
    loads_in_flight: usize,
    /// Highlighting jobs by path, with the diff each one highlights.
    highlighting: HashMap<String, (Arc<FileDiff>, Task<()>)>,
    next_token: u64,
    /// The load generation: bumped by every refresh, checked by its result.
    generation: u64,
    refreshing: Option<Task<()>>,
    /// A refresh asked for while one runs; it starts when that one lands.
    queued: Option<Batch>,
    /// Whether a file list has landed.
    listed: bool,
    /// Why the last refresh failed.
    error: Option<Failure>,
    fold_task: Option<Task<()>>,
    /// Another fold reload was asked for while one ran.
    fold_again: bool,
    /// How many fold reloads have started; each knows its number, so a write can tell the
    /// reloads that read it from those that started before it landed.
    folds_started: u64,
    /// Why the last fold reload failed.
    fold_error: Option<Failure>,
    /// Writes running.
    writes: usize,
    /// Viewed toggles whose outcome hasn't been recorded yet.
    viewed_pending: usize,
    /// The viewed state the reviewer last asked for, by path, until the fold reads it back
    /// (`writes`): a toggle made meanwhile flips that, not the fold's stale state.
    viewed_intents: HashMap<String, ViewedIntent>,
    /// Why the last viewed toggle failed.
    viewed_error: Option<Failure>,
    watch: WatchState,
    /// The connection to the repository's `dv backend --stdio`, when it is a remote one.
    link: connection::Link,
    /// Whether the stream draws the agent's annotations (`A`, PLAN §19.3): shown whenever a
    /// review opens, never persisted.
    annotations_shown: bool,
    /// The thread the stream's reply composer answers, whose card stays drawn while it does
    /// ([`ReviewSession::shows_card`]): a question being written about an annotation never
    /// loses its card to `A` or to the agent rewriting its code.
    replying_to: Option<ThreadId>,
    /// A PR review's pull request as last refreshed (`pr`).
    pr: pr::PrState,
    /// GitHub's threads on that pull request (`github`).
    github: github::GithubState,
}

impl EventEmitter<SessionEvent> for ReviewSession {}

impl ReviewSession {
    /// A session over an opened review ([`crate::services::Reviews::open_review`]); it starts
    /// loading the file list at once.
    pub fn new(opened: OpenedReview, cx: &mut Context<Self>) -> Self {
        let OpenedReview {
            origin,
            backend,
            loaded,
            reviewer,
            labels,
            connection,
            pr,
            ..
        } = opened;
        let comparison = default_comparison(&loaded.review);
        let mut session = Self {
            repo: GitRepo::new(backend.clone()),
            store: ReviewStore::new(backend.clone()),
            backend,
            origin,
            review: loaded.review,
            warnings: loaded.warnings,
            reviewer,
            // As the review opened, so the target is named in full before the first listing
            // reads them again.
            labels,
            spec: None,
            resolved: None,
            comparison,
            asked: comparison,
            comparison_error: None,
            list: FileIndex::default(),
            target_list: None,
            states: HashMap::new(),
            previous: HashMap::new(),
            placements: RefCell::new(HashMap::new()),
            fuzzy: RefCell::new(HashMap::new()),
            review_texts: HashMap::new(),
            histories: HashMap::new(),
            histories_held: false,
            side_reads: HashMap::new(),
            side_paths: None,
            file_view: None,
            file_view_from: side::ViewFrom::Review,
            #[cfg(test)]
            worked_out: std::cell::Cell::new(0),
            #[cfg(test)]
            loads_held: false,
            unlimited: HashSet::new(),
            wanted: Vec::new(),
            searching: false,
            extra: HashMap::new(),
            queue: VecDeque::new(),
            loads_in_flight: 0,
            highlighting: HashMap::new(),
            next_token: 0,
            generation: 0,
            refreshing: None,
            queued: None,
            listed: false,
            error: None,
            fold_task: None,
            fold_again: false,
            folds_started: 0,
            fold_error: None,
            writes: 0,
            viewed_pending: 0,
            viewed_intents: HashMap::new(),
            viewed_error: None,
            watch: WatchState::default(),
            link: connection::Link::new(connection),
            annotations_shown: true,
            replying_to: None,
            pr: pr::PrState::default(),
            github: github::GithubState::default(),
        };
        session.follow_connection(cx);
        // The watch first, so it is registered as early as possible; what the first list reads
        // before then is read again once it is (`watch`).
        session.ensure_watch(cx);
        session.refresh(Batch::everything(), cx);
        // A PR review is refreshed as it opens (PLAN §9.4): an open by the pull request's
        // reference did that already.
        match pr {
            Some(read) => session.pr.read = Some(read),
            None => session.refresh_pr(cx),
        }
        session
    }

    /// The backend of the review's home worktree, where its target resolves.
    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    /// The repository the review was opened from: the switcher row it belongs to.
    pub fn origin(&self) -> &Location {
        &self.origin
    }

    /// Whether the review was opened from its own home worktree's row, not another
    /// worktree's.
    pub fn is_home_origin(&self) -> bool {
        self.backend.repo().location == self.origin
    }

    pub fn store(&self) -> &ReviewStore {
        &self.store
    }

    pub fn review(&self) -> &Review {
        &self.review
    }

    pub fn review_id(&self) -> &ReviewId {
        &self.review.id
    }

    /// Problems found in the review's log (PLAN §7).
    pub fn warnings(&self) -> &[LogWarning] {
        &self.warnings
    }

    pub fn reviewer(&self) -> &Author {
        &self.reviewer
    }

    /// The target as the title bar names it ("working tree vs main"; a pull request by its
    /// number and title, "#482 Fix the cache").
    pub fn target_label(&self) -> String {
        pr::label(&self.review, self.pr.read.as_ref())
            .unwrap_or_else(|| target_label(&self.review.target, &self.labels))
    }

    /// The target as a prompt names it: in dv's own words alone. A pull request is
    /// `owner/repo#482`, without its title, which its author wrote (PLAN §9.6).
    pub fn prompt_label(&self) -> String {
        target_label(&self.review.target, &self.labels)
    }

    /// What the stream's side labels call the old and the new side (`main` / `working tree`;
    /// `round 1` / `working tree` in an interdiff, DESIGN §4.1).
    pub fn side_labels(&self) -> (String, String) {
        let (old, new) = self.target_side_labels();
        match self.comparison {
            Comparison::SinceRound(n) => (format!("round {n}"), new),
            Comparison::All => (old, new),
        }
    }

    /// What the review's own sides are called (all changes, whichever comparison shows):
    /// `main` / `working tree`, for the file view (PLAN §18.1); a pull request's base and
    /// head branches once it was read.
    pub fn target_side_labels(&self) -> (String, String) {
        match self.pr.read.as_ref().and_then(|read| read.mirror.as_ref()) {
            Some(mirror) if self.review.is_pr_review() => {
                (mirror.pr.base_ref.clone(), mirror.pr.head_ref.clone())
            }
            _ => side_labels(&self.review.target, &self.labels),
        }
    }

    /// The spec the files were listed with at the last refresh (an interdiff's, in one);
    /// `None` until one landed.
    pub fn spec(&self) -> Option<&DiffSpec> {
        self.spec.as_ref()
    }

    /// What the files and diffs shown are: an interdiff or all changes.
    pub fn comparison(&self) -> Comparison {
        self.comparison
    }

    /// Whether the stream draws the agent's annotations (`A`, PLAN §19.3).
    pub fn annotations_shown(&self) -> bool {
        self.annotations_shown
    }

    /// Keeps `thread`'s card drawn while the stream's reply composer answers it (`None` once
    /// it closes); see [`ReviewSession::shows_card`].
    pub fn set_replying_to(&mut self, thread: Option<ThreadId>) {
        self.replying_to = thread;
    }

    /// Shows or hides the agent's annotations in the stream and the file view (`A`, a click
    /// on an annotation's row in the comments list): a render filter alone, so placement and
    /// the files loaded for threads stay as they are.
    pub fn set_annotations_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if self.annotations_shown != shown {
            self.annotations_shown = shown;
            cx.emit(SessionEvent::AnnotationsShown);
            cx.notify();
        }
    }

    /// The comparison asked for: what [`ReviewSession::comparison`] becomes once the listing
    /// under way lands (the toggle shows it at once).
    pub fn asked_comparison(&self) -> Comparison {
        self.asked
    }

    /// Whether the review offers the toggle between an interdiff and all changes.
    pub fn comparison_available(&self) -> bool {
        comparison_available(&self.review)
    }

    /// Why the interdiff asked for isn't shown (its round's snapshot is missing), until it is
    /// dismissed or another comparison is asked for.
    pub fn comparison_error(&self) -> Option<&Failure> {
        self.comparison_error.as_ref()
    }

    /// Forgets [`ReviewSession::comparison_error`] (its banner's Dismiss).
    pub fn dismiss_comparison_error(&mut self, cx: &mut Context<Self>) {
        if self.comparison_error.take().is_some() {
            cx.notify();
        }
    }

    /// Shows `comparison` (all changes when the review offers no toggle, or names a round it
    /// hasn't closed): the files are listed again from scratch under a new generation, a
    /// listing already under way being dropped; the diffs shown stay until theirs land.
    pub fn set_comparison(&mut self, comparison: Comparison, cx: &mut Context<Self>) {
        let closed = |n: u32| n >= 1 && (n as usize) <= self.review.rounds.len();
        let comparison = match comparison {
            Comparison::SinceRound(n) if self.comparison_available() && closed(n) => comparison,
            _ => Comparison::All,
        };
        if comparison == self.asked {
            return;
        }
        self.asked = comparison;
        self.comparison_error = None;
        // Its result belongs to the comparison asked for before.
        self.refreshing = None;
        let mut batch = self.queued.take().unwrap_or_default();
        batch.merge(Batch::everything());
        self.refresh(batch, cx);
    }

    /// `I`: the interdiff since the last closed round, or all changes, whichever isn't asked
    /// for; nothing when the review offers no toggle.
    pub fn toggle_comparison(&mut self, cx: &mut Context<Self>) {
        let Some(last) = self
            .review
            .last_round()
            .map(|round| round.n)
            .filter(|_| self.comparison_available())
        else {
            return;
        };
        let next = match self.asked {
            Comparison::All => Comparison::SinceRound(last),
            Comparison::SinceRound(_) => Comparison::All,
        };
        self.set_comparison(next, cx);
    }

    /// The files of the last refresh, sorted by path: what the stream shows.
    pub fn files(&self) -> &[FileEntry] {
        &self.list.files
    }

    /// The review's changed files as of the last refresh (all changes), whichever comparison is
    /// shown: what finishing the round finishes (DESIGN §4.2).
    pub fn target_files(&self) -> &[FileEntry] {
        &self.target_index().files
    }

    fn target_index(&self) -> &FileIndex {
        self.target_list.as_ref().unwrap_or(&self.list)
    }

    pub fn file(&self, path: &str) -> Option<&FileEntry> {
        self.list.get(path)
    }

    pub fn file_state(&self, path: &str) -> Option<&FileState> {
        self.states.get(path)
    }

    /// The diff the stream shows for `path`: its loaded diff, or, while it loads again, the one
    /// it had before. `None` before its first load lands and after a failed one.
    pub fn shown(&self, path: &str) -> Option<Shown> {
        match self.states.get(path)? {
            FileState::Loaded { diff, highlights } => Some(Shown {
                diff: diff.clone(),
                highlights: highlights.clone(),
            }),
            FileState::Unloaded | FileState::Loading { .. } => self
                .previous
                .get(path)
                .map(|previous| previous.shown.clone()),
            FileState::Failed(_) => None,
        }
    }

    /// The comparison [`ReviewSession::shown`]'s diff was listed under: the one shown, or for
    /// the diff a file had while it loads again, the one it was listed under then.
    pub fn shown_comparison(&self, path: &str) -> Option<Comparison> {
        match self.states.get(path)? {
            FileState::Loaded { .. } => Some(self.comparison),
            FileState::Unloaded | FileState::Loading { .. } => {
                self.previous.get(path).map(|previous| previous.comparison)
            }
            FileState::Failed(_) => None,
        }
    }

    /// [`ReviewSession::shown`]'s diff.
    pub fn shown_diff(&self, path: &str) -> Option<&Arc<FileDiff>> {
        match self.states.get(path)? {
            FileState::Loaded { diff, .. } => Some(diff),
            FileState::Unloaded | FileState::Loading { .. } => {
                self.previous.get(path).map(|previous| &previous.shown.diff)
            }
            FileState::Failed(_) => None,
        }
    }

    /// The load generation (PLAN §11).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the file list is (re)loading.
    pub fn is_loading(&self) -> bool {
        self.refreshing.is_some() || self.is_first_load()
    }

    /// Whether the first file list is still loading: the views show skeletons then, never
    /// over a list that is only refreshing (DESIGN §4.4).
    pub fn is_first_load(&self) -> bool {
        !self.listed && self.error.is_none()
    }

    /// Why the last refresh (or, failing that, the last fold reload) failed.
    pub fn error(&self) -> Option<&Failure> {
        self.error.as_ref().or(self.fold_error.as_ref())
    }

    /// Why the last viewed toggle failed, until the next one lands or it is dismissed.
    pub fn viewed_error(&self) -> Option<&Failure> {
        self.viewed_error.as_ref()
    }

    /// Forgets [`ReviewSession::viewed_error`] (its banner's Dismiss).
    pub fn dismiss_viewed_error(&mut self, cx: &mut Context<Self>) {
        if self.viewed_error.take().is_some() {
            cx.notify();
        }
    }

    /// An approved, closed or submitted review is read-only (DESIGN §4.4).
    pub fn is_read_only(&self) -> bool {
        self.review.status.is_final()
    }

    /// Whether `path` is viewed in the round in progress, or in the round that finished a
    /// finished review ([`dv_core::review::Review::shown_round`]).
    pub fn is_viewed(&self, path: &str) -> bool {
        self.review.is_viewed(self.review.shown_round(), path)
    }

    /// Whether `path` will be viewed once the reviewer's viewed toggles have landed: the state
    /// the last one asked for while the fold hasn't read it back yet, else
    /// [`ReviewSession::is_viewed`]. What the next toggle flips.
    pub fn intends_viewed(&self, path: &str) -> bool {
        match self.viewed_intents.get(path) {
            Some(intent) if intent.round == self.review.current_round() => intent.on,
            _ => self.is_viewed(path),
        }
    }

    /// How many files of the list are viewed in the round [`ReviewSession::is_viewed`] reads.
    pub fn viewed_count(&self) -> usize {
        self.list
            .files
            .iter()
            .filter(|entry| self.is_viewed(&entry.path))
            .count()
    }

    /// Why the watch can't cover the worktree (the session then refreshes on window
    /// activation); `None` while it does.
    pub fn watch_notice(&self) -> Option<&str> {
        self.watch.notice()
    }

    /// Nothing is loading, waiting or writing, and every file the stream asked for, and every
    /// file a thread shows in, has landed: automation's `settled`.
    pub fn is_settled(&self) -> bool {
        let landed = |path: &String| {
            self.states
                .get(path)
                .is_none_or(|state| !state.is_pending())
        };
        // While the connection is down nothing loads: what waits for it can't land before.
        let wanted_landed = self.spec.is_none()
            || self.link.down
            || (self.wanted.iter().all(landed)
                && (self.file_view_from != side::ViewFrom::Review
                    || self.file_view.iter().all(landed))
                && self.extra_paths().all(landed)
                && self.thread_files().iter().all(landed)
                && (!self.searching || self.list.files.iter().all(|entry| landed(&entry.path))));
        self.refreshing.is_none()
            && self.queued.is_none()
            && self.watch.is_settled()
            && self.link.is_settled()
            && self.writes == 0
            && self.viewed_pending == 0
            && self.fold_task.is_none()
            && self.loads_in_flight == 0
            && self.highlighting.is_empty()
            && !self.review_texts.values().any(ReviewText::is_loading)
            && !self.histories_loading()
            && !self.side_reads_loading()
            && self.pr.is_settled()
            && self.github.is_settled()
            && wanted_landed
    }

    /// The side a comment on the whole of `path` goes on (PLAN §8): its new side, or for a
    /// deleted file its old one; `None` where none can: a file not among the review's changes
    /// (an interdiff's alone), or in an interdiff a deleted file, whose old column there is a
    /// round's snapshot, which no anchor names (DESIGN §5).
    pub fn file_comment_side(&self, path: &str) -> Option<AnchorSide> {
        self.file(path)?;
        let entry = self.target_index().get(path)?;
        let side = match entry.status {
            FileStatus::Deleted => AnchorSide::Old,
            _ => AnchorSide::New,
        };
        let snapshot = self.comparison.is_interdiff() && side == AnchorSide::Old;
        (!snapshot).then_some(side)
    }

    /// The anchor of a comment on the whole of `path`, on `side` of it, in the round in
    /// progress (a file-level anchor, PLAN §8), named by the review's own entry for the file
    /// as [`ReviewSession::anchor_for`] names one; it reads none of the file's text, so a file
    /// shown without lines (binary, too large) takes one too.
    pub fn file_anchor_for(&self, path: &str, side: AnchorSide) -> Result<Anchor, Failure> {
        if self.comparison.is_interdiff() && side == AnchorSide::Old {
            return Err(Failure::new(
                "the old side here is a round's snapshot: show all changes to comment on it",
            ));
        }
        self.file(path)
            .ok_or_else(|| Failure::new(format!("{path} isn't in this review's diff")))?;
        let entry = self.target_index().get(path).ok_or_else(|| {
            Failure::new(format!(
                "{path} isn't among this review's changes any more: nothing to comment on"
            ))
        })?;
        if !has_side(entry, side) {
            return Err(Failure::new(format!(
                "{path} has no {} side to comment on",
                crate::thread::side_word(side)
            )));
        }
        Ok(AnchorPlace::for_entry(entry, side, self.review.current_round()).file_level())
    }

    /// The anchor for lines `from` to `to` (either order) of `side` of the file at `path` as
    /// the file view shows it (`text`: its side's full text, PLAN §18.1), in the round in
    /// progress: named by the review's own entry for the file when the review changes it, else
    /// by its path alone (a file the review doesn't change, PLAN §8). An interdiff's old side
    /// takes none, as [`ReviewSession::anchor_for`] says.
    pub fn anchor_in_text(
        &self,
        path: &str,
        side: AnchorSide,
        text: &dv_core::diff::TextDiff,
        from: u32,
        to: u32,
    ) -> Result<Anchor, Failure> {
        if self.comparison.is_interdiff() && side == AnchorSide::Old {
            return Err(Failure::new(
                "the old side here is a round's snapshot: show all changes to comment on it",
            ));
        }
        let round = self.review.current_round();
        let place = match self.target_index().get(path) {
            Some(entry) => AnchorPlace::for_entry(entry, side, round),
            None => AnchorPlace {
                round,
                path: path.to_string(),
                old_path: None,
                side,
            },
        };
        anchor_selection(place, &side_lines(text, side), from, to)
            .map_err(|error: AnchorError| Failure::new(error.to_string()))
    }

    /// The anchor for lines `from` to `to` (either order) of `path`'s `side` in its loaded
    /// text diff, in the round in progress (`dv_core::review::anchor_selection`), named by the
    /// review's own entry for the file. An interdiff's old side is a round's snapshot, which no
    /// anchor names: only its new side takes a comment (DESIGN §5), and only on a file among
    /// the review's changes (one the agent reverted after the round is in the interdiff alone),
    /// as `dv comment add` refuses one that isn't.
    pub fn anchor_for(
        &self,
        path: &str,
        side: AnchorSide,
        from: u32,
        to: u32,
    ) -> Result<Anchor, Failure> {
        if self.comparison.is_interdiff() && side == AnchorSide::Old {
            return Err(Failure::new(
                "the old side here is a round's snapshot: show all changes to comment on it",
            ));
        }
        self.file(path)
            .ok_or_else(|| Failure::new(format!("{path} isn't in this review's diff")))?;
        let entry = self.target_index().get(path).ok_or_else(|| {
            Failure::new(format!(
                "{path} isn't among this review's changes any more: nothing to comment on"
            ))
        })?;
        // The lines the reviewer sees: while the file loads again, the ones it showed before
        // (not an old side listed under the other comparison, which is another text).
        let text = self
            .placement_diff(path, side)
            .and_then(|diff| diff.as_text())
            .ok_or_else(|| Failure::new(format!("{path}'s lines aren't loaded")))?;
        anchor_selection(
            AnchorPlace::for_entry(entry, side, self.review.current_round()),
            &side_lines(text, side),
            from,
            to,
        )
        .map_err(|error: AnchorError| Failure::new(error.to_string()))
    }
}
