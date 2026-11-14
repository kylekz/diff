//! The reviews dv knows about (PLAN §11 `Reviews`): the known repositories
//! ([`known_repos`], persisted) and, per repository, what the switcher lists — its name and
//! location, its open and finished reviews (`ReviewStore::list`, newest first; an unreadable
//! log is a row with its error, PLAN §7), and a summary of its uncommitted changes
//! ("uncommitted changes · 7 files", from the `working` target's file list).
//!
//! A repository's row is computed in the background ([`Reviews::refresh`]): when it is added,
//! whenever the switcher opens ([`Reviews::refresh_all`]), and when the connection a row is
//! reached through starts reconnecting, stops or fails, or comes back, whether or not the
//! switcher is open ([`LocationsEvent::ConnectionChanged`]): a connection that comes back
//! clears what its rows said about it being down at once, so the switcher never opens on a
//! stale **Start** or "reconnecting…". Nothing here watches a repository: an open review's
//! session does that for its own repository (PLAN §11). A row is read through
//! [`Locations::reach`], which never boots a distro (PLAN §5.2's connecting rule):
//! - a WSL repository whose distro isn't running lists as "Ubuntu isn't running" and offers
//!   **Start Ubuntu**, whatever its connection did before;
//! - one whose connection failed says why and offers **Retry**;
//! - one whose connection is reconnecting reads "reconnecting…" and keeps the rows its last
//!   listing had.
//!
//! Start and Retry are the same explicit open ([`Reviews::start`]): it connects, booting the
//! distro, and then lists the repository as usual.
//!
//! [`Reviews::open_review`] opens or resumes a target's review (PLAN §2.4: at most one open
//! review per target) as the reviewer, records that the repository was used, and hands back
//! what a `ReviewSession` starts from. An open whose connection is lost under it, at any of its
//! steps, waits for the reconnect (starting nothing itself) and runs once more (PLAN §5.2). A
//! remote repository that can't be reached, or whose connection is lost again, fails with a
//! one-line headline ("couldn't open Ubuntu:/home/kyle/api"), the connection's error and
//! diagnostics in its detail (DESIGN §4.4).
//!
//! **One row per repository.** A known location is a worktree root, its WSL distro spelled as
//! WSL spells it (`Locations::canonical`). One added before it could be resolved (a subfolder
//! added while its connection was down, so its row could say why) or under another spelling of
//! its distro becomes that root the first time a listing or an open resolves it
//! ([`Reviews::resolved`]): its row and `known_repos.json` are re-keyed, or it goes when the
//! root is known already. Locations are compared with the distro folded
//! (`Location::same_place`), so a spelling WSL couldn't confirm never adds a second row.

pub mod known_repos;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use dv_backend::{Connection, ConnectionKey};
use dv_core::diff::file_list;
use dv_core::git::branches::Branch;
use dv_core::label::{LabelContext, Labels};
use dv_core::review::{
    Author, Home, HomeError, ListEntry, LoadedReview, ReviewId, ReviewStatus, ReviewStore,
    ReviewSummary, home, ops, reviewer_identity,
};
use dv_core::{Backend, BackendError, GitRepo, Location, Target};
use futures::channel::oneshot;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Subscription, Task, WeakEntity,
};

use super::locations::{Locations, LocationsEvent, Reach};
use super::persist::{JsonFile, ReadObject, read_object};
use crate::failure::Failure;
pub use known_repos::{KNOWN_REPOS_FILE, KnownRepo, KnownReposFile};

/// What [`Reviews`] tells its subscribers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewsEvent {
    /// The known repositories or a repository's row changed.
    Changed,
}

/// A repository's uncommitted changes: what the `working` target's file list holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkingSummary {
    pub files: usize,
    pub additions: u64,
    pub deletions: u64,
}

/// One review in a repository's row.
#[derive(Clone, Debug, PartialEq)]
pub enum ReviewRow {
    Ok {
        summary: Box<ReviewSummary>,
        /// `dv_core::label::target_label` of its target.
        label: String,
    },
    /// A log that didn't load (PLAN §7: one bad file never hides the others).
    Err { id: ReviewId, error: Failure },
}

impl ReviewRow {
    pub fn id(&self) -> &ReviewId {
        match self {
            Self::Ok { summary, .. } => &summary.id,
            Self::Err { id, .. } => id,
        }
    }
}

/// A known repository as the switcher lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct RepoRow {
    pub location: Location,
    /// The short name (`api-server`).
    pub name: String,
    /// The full location (`D:\code\api-server`, `Ubuntu:/home/kyle/api-server`).
    pub path: String,
    /// A refresh is running.
    pub loading: bool,
    /// Why the repository (or part of its row) couldn't be read, from the last refresh.
    pub error: Option<Failure>,
    /// Why the last refresh couldn't open the repository at all, in a few words ("Ubuntu isn't
    /// running", "no such directory: …"); `None` while it opens. Such a row lists no targets
    /// to review.
    pub unreachable: Option<String>,
    /// The distro to start, when the repository's host isn't running (a WSL distro, or a
    /// connection found stopped): its row offers **Start** (DESIGN §4.1).
    pub stopped: Option<String>,
    /// The repository's connection failed: its row offers **Retry** (DESIGN §4.1).
    pub retry: bool,
    /// The repository's connection is reconnecting: the row says so and keeps what its last
    /// listing had.
    pub reconnecting: bool,
    /// Its distro is being started, or its connection retried ([`Reviews::start`]).
    pub starting: bool,
    /// The worktree root as its reviews record it (`ReviewSummary::worktree`), once a refresh
    /// opened the repository: which `working` and `staged` reviews are this worktree's.
    pub toplevel: Option<String>,
    /// Its uncommitted changes; `None` until a refresh computed them, or when it couldn't.
    pub working: Option<WorkingSummary>,
    /// Its reviews, newest first.
    pub reviews: Vec<ReviewRow>,
    pub labels: LabelContext,
    /// Its local branches, the most recently committed first, each with its stack base: what
    /// the switcher's branch picker offers (DESIGN §4.1). Empty until a refresh read them, or
    /// when it couldn't.
    pub branches: Vec<Branch>,
    /// When it was last used, for the order rows are listed in.
    pub last_used: u64,
}

impl RepoRow {
    /// The open review of `target` in this worktree, if the last refresh listed one (the rule
    /// `ops::open_review` resumes by).
    pub fn open_review_of(&self, target: &Target) -> Option<&ReviewSummary> {
        let toplevel = self.toplevel.as_deref()?;
        self.reviews.iter().find_map(|row| match row {
            ReviewRow::Ok { summary, .. }
                if summary.status.is_open() && ops::is_review_of(summary, target, toplevel) =>
            {
                Some(&**summary)
            }
            _ => None,
        })
    }

    /// Whether the last refresh could read the repository: it opened, and its changes were
    /// listed (or haven't been yet); while its connection reconnects, whether a listing before
    /// did. One that can't be read offers no rows (DESIGN §4.1).
    pub fn is_readable(&self) -> bool {
        self.unreachable.is_none()
            && !self.starting
            && !(self.error.is_some() && self.working.is_none())
            && !(self.reconnecting && self.working.is_none())
    }

    fn new(repo: &KnownRepo) -> Self {
        Self {
            location: repo.location.clone(),
            name: repo.location.short_name(),
            path: repo.location.display_name(),
            loading: false,
            error: None,
            unreachable: None,
            stopped: None,
            retry: false,
            reconnecting: false,
            starting: false,
            toplevel: None,
            working: None,
            reviews: Vec::new(),
            labels: LabelContext::default(),
            branches: Vec::new(),
            last_used: repo.last_used(),
        }
    }
}

/// What a background scan of a repository found.
struct Scan {
    working: Result<WorkingSummary, Failure>,
    reviews: Result<Vec<ReviewRow>, Failure>,
    labels: LabelContext,
    /// Empty when they couldn't be read: the picker then offers none, the row the rest.
    branches: Vec<Branch>,
    toplevel: String,
    /// The repository's location as its backend resolved it: its worktree root.
    root: Location,
}

impl Scan {
    /// The connection died under the scan: what it read is the reconnect's to read again.
    fn lost_connection(&self) -> bool {
        [self.working.as_ref().err(), self.reviews.as_ref().err()]
            .into_iter()
            .flatten()
            .any(|failure| failure.disconnected)
    }
}

/// What a refresh found.
enum Refreshed {
    Scanned(Scan),
    /// The repository's connection is reconnecting: the row keeps what it had.
    Reconnecting,
    Unopened(Unopened),
}

/// Why a refresh couldn't open a repository: the failure, the few words a switcher row shows
/// for it, the distro to start when that is why, and whether its connection failed (Retry).
struct Unopened {
    failure: Failure,
    reason: String,
    stopped: Option<String>,
    retry: bool,
}

impl Unopened {
    fn new(error: &BackendError, retry: bool) -> Self {
        let reason = match error {
            BackendError::Unavailable { reason, .. } => reason.clone(),
            other => other.to_string(),
        };
        Self {
            failure: Failure::from(error),
            reason,
            stopped: None,
            retry,
        }
    }

    /// A repository whose host isn't running (`Reach::Stopped`); `error` is why its
    /// connection failed, when it did (a start that didn't boot it).
    fn stopped(distro: String, error: Option<&BackendError>) -> Self {
        let reason = format!("{distro} isn't running");
        Self {
            failure: error.map_or_else(|| Failure::new(reason.clone()), Failure::from),
            reason,
            stopped: Some(distro),
            retry: false,
        }
    }
}

/// What [`Reviews::open`] opens: a target's review (resumed when one is open) or a review by id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenRequest {
    Target(Target),
    Review(ReviewId),
}

/// A target's review, opened or resumed ([`Reviews::open_review`]).
pub struct OpenedReview {
    /// The repository it was opened from: the switcher's row, `dv open`'s path.
    pub origin: Location,
    /// Its home worktree's backend, where its target resolves ([`Reviews::open`]).
    pub backend: Arc<dyn Backend>,
    pub loaded: LoadedReview,
    /// `false` when an open review of the target already existed.
    pub created: bool,
    /// Who the reviewer is in this repository.
    pub reviewer: Author,
    /// The connection `backend` reaches its repository through (a WSL distro's, a
    /// loopback's); `None` for a repository opened in process. The session follows it (PLAN
    /// §5.2).
    pub connection: Option<Arc<Connection>>,
    /// The repository's labels as the review opened, so its target is named in full
    /// ("working tree vs main") from the start rather than once its files are listed.
    pub labels: LabelContext,
}

/// The reviews service (see the module docs).
pub struct Reviews {
    locations: Entity<Locations>,
    known: KnownReposFile,
    file: Entity<JsonFile>,
    rows: HashMap<Location, RepoRow>,
    /// Refreshes running, by repository, with the generation each started at.
    refreshing: HashMap<Location, (u64, Task<()>)>,
    generation: u64,
    /// `add_repo` and `open_review` calls running.
    opening: usize,
    _locations: Subscription,
}

impl EventEmitter<ReviewsEvent> for Reviews {}

impl Reviews {
    /// Reads `<data_dir>/known_repos.json` synchronously (it is small) and lists every known
    /// repository; their rows fill in as [`Reviews::refresh`] computes them.
    pub fn new(locations: Entity<Locations>, data_dir: &Path, cx: &mut Context<Self>) -> Self {
        let path = data_dir.join(KNOWN_REPOS_FILE);
        let (known, unsaveable) = match read_object(&path) {
            ReadObject::Missing => (KnownReposFile::default(), None),
            ReadObject::Object(object) => match KnownReposFile::from_object(object) {
                Ok((known, problems)) => {
                    for problem in problems {
                        log::warn!("{}: {problem}", path.display());
                    }
                    (known, None)
                }
                Err(problem) => {
                    log::warn!("{}: {problem}", path.display());
                    (
                        KnownReposFile::default(),
                        Some(format!(
                            "{} isn't a file dv can read ({problem}), so it's left as it is and \
                             changes aren't saved this session",
                            path.display()
                        )),
                    )
                }
            },
            ReadObject::Unreadable(reason) => {
                log::warn!("{reason}");
                (KnownReposFile::default(), Some(reason))
            }
        };
        let rows = known
            .repos()
            .map(|repo| (repo.location.clone(), RepoRow::new(repo)))
            .collect();
        let file = cx.new(|cx| JsonFile::new(path, unsaveable, cx));
        cx.observe(&file, |_, _, cx| cx.notify()).detach();
        // A connection going down or coming back changes its rows (module docs).
        let subscription =
            cx.subscribe(
                &locations,
                |this, _, event: &LocationsEvent, cx| match event {
                    LocationsEvent::ConnectionChanged { key, up } => {
                        this.connection_changed(key, *up, cx)
                    }
                },
            );
        Self {
            locations,
            known,
            file,
            rows,
            refreshing: HashMap::new(),
            generation: 0,
            opening: 0,
            _locations: subscription,
        }
    }

    /// The known repositories' rows, most recently used first.
    pub fn rows(&self) -> Vec<&RepoRow> {
        let mut rows: Vec<&RepoRow> = self.rows.values().collect();
        rows.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.path.cmp(&b.path)));
        rows
    }

    pub fn row(&self, location: &Location) -> Option<&RepoRow> {
        self.rows.get(location)
    }

    pub fn known(&self) -> &KnownReposFile {
        &self.known
    }

    /// Why the known repositories can't be saved, or the last save failed.
    pub fn save_error<'a>(&self, cx: &'a App) -> Option<&'a str> {
        self.file.read(cx).last_error()
    }

    /// Nothing is refreshing, opening or waiting to be saved: automation's `settled`.
    pub fn is_settled(&self, cx: &App) -> bool {
        self.refreshing.is_empty() && self.opening == 0 && self.file.read(cx).is_settled()
    }

    /// Adds the repository at `location` (like `dv open <path>`), opened explicitly (a WSL
    /// one's distro is started, PLAN §5.2): its worktree root becomes a known repository and
    /// its row is computed. A location whose backend can't be reached (its connection failed
    /// or is down) is added as given (its distro spelled as WSL spells it), so its row says
    /// why, until a listing resolves it (module docs); any other location that doesn't open is
    /// refused. Resolves to the location as known.
    pub fn add_repo(
        &mut self,
        location: Location,
        cx: &mut Context<Self>,
    ) -> Task<Result<Location, Failure>> {
        self.opening += 1;
        let open = self
            .locations
            .update(cx, |locations, cx| locations.open(location.clone(), cx));
        detached(cx, async move |this, cx| {
            let opened = open.await;
            this.update(cx, |this, cx| {
                this.opening -= 1;
                cx.notify();
                let known = match opened {
                    Ok(backend) => {
                        let root = backend.repo().location.clone();
                        this.resolved(&location, &root, cx);
                        root
                    }
                    Err(error)
                        if matches!(
                            *error,
                            BackendError::Unavailable { .. } | BackendError::Disconnected { .. }
                        ) =>
                    {
                        this.locations.read(cx).canonical(&location)
                    }
                    Err(error) => return Err(Failure::from(&*error)),
                };
                let known = this.add_known(known, cx);
                this.refresh(known.clone(), cx);
                Ok(known)
            })
            .unwrap_or_else(|_| Err(closing()))
        })
    }

    /// Opens the review of `target` in the repository at `location`, or resumes its open one
    /// (`ops::open_review`), as the reviewer. The repository becomes known (and most recently
    /// used) and its row is refreshed.
    pub fn open_review(
        &mut self,
        location: Location,
        target: Target,
        cx: &mut Context<Self>,
    ) -> Task<Result<OpenedReview, Failure>> {
        self.open(location, OpenRequest::Target(target), cx)
    }

    /// [`Reviews::open_review`] for either kind of [`OpenRequest`]: a review asked for by id is
    /// loaded as it is (the switcher's rows for reviews of other targets).
    ///
    /// The review comes back bound to its home worktree (`dv_core::review::home`, PLAN §2.2,
    /// §2.4): the backend of the worktree its target resolves in, which may be another
    /// worktree of the repository than `location`'s (a `working` review made in a linked
    /// worktree, a `branch` review of a branch checked out in one), opened through
    /// [`Locations`] like any other. The CLI resolves it there too, so both list the same
    /// files, place threads at the same lines and snapshot the same content. A home worktree
    /// that is gone is a failure, the banner's. The repository marked as used is `location`'s,
    /// the row it was opened from.
    ///
    /// When the connection is lost under any step (the server dies while the review is found
    /// or loaded), the open waits for the reconnect, starting nothing itself
    /// (`Connection::wait_for_reconnect`), and runs once more from the start; lost again, or
    /// the reconnect failing, it fails as "couldn't open <location>" (module docs), flagged
    /// [`Failure::disconnected`] when the connection was lost under it.
    pub fn open(
        &mut self,
        location: Location,
        request: OpenRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<OpenedReview, Failure>> {
        self.opening += 1;
        let locations = self.locations.clone();
        let open = self
            .locations
            .update(cx, |locations, cx| locations.open(location.clone(), cx));
        detached(cx, async move |this, cx| {
            let mut result = open_once(&locations, &location, request.clone(), open, cx).await;
            let lost = result.as_ref().is_err_and(|failure| failure.disconnected);
            let connection =
                locations.read_with(cx, |locations, _| locations.connection_for(&location));
            if lost && let Some(connection) = connection {
                let waited = cx
                    .background_spawn({
                        let connection = connection.clone();
                        async move { connection.wait_for_reconnect() }
                    })
                    .await;
                result = match waited {
                    Ok(()) => {
                        let open = locations
                            .update(cx, |locations, cx| locations.open(location.clone(), cx));
                        open_once(&locations, &location, request, open, cx).await
                    }
                    Err(error) => {
                        let named =
                            locations.read_with(cx, |locations, _| locations.canonical(&location));
                        Err(open_failure(&named, &error, Some(&connection)))
                    }
                };
            }
            this.update(cx, |this, cx| {
                this.opening -= 1;
                if let Ok(opened) = &result {
                    this.resolved(&location, &opened.origin, cx);
                    let known = this.add_known(opened.origin.clone(), cx);
                    if this
                        .known
                        .mark_opened(&known, dv_core::review::event::now_ms())
                    {
                        this.save(cx);
                    }
                    if let Some(row) = this.rows.get_mut(&known) {
                        row.last_used = this.known.get(&known).map_or(0, KnownRepo::last_used);
                    }
                    this.refresh(known, cx);
                }
                cx.notify();
            })
            .ok();
            result
        })
    }

    /// Recomputes every known repository's row (the switcher opening).
    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        let locations: Vec<Location> = self.rows.keys().cloned().collect();
        for location in locations {
            self.refresh(location, cx);
        }
    }

    /// Recomputes the rows of the repositories reached through the connection of `key`.
    pub fn refresh_connection(&mut self, key: &ConnectionKey, cx: &mut Context<Self>) {
        for location in self.rows_through(key, cx) {
            self.refresh(location, cx);
        }
    }

    /// The known repositories reached through the connection of `key` (`Locations::key_for`,
    /// which reads no file system).
    fn rows_through(&self, key: &ConnectionKey, cx: &App) -> Vec<Location> {
        let locations = self.locations.read(cx);
        self.rows
            .keys()
            .filter(|location| locations.key_for(location).as_ref() == Some(key))
            .cloned()
            .collect()
    }

    /// The connection of `key` went down, or (`up`) came back
    /// (`LocationsEvent::ConnectionChanged`, module docs): its rows are listed again; back,
    /// what they said about it being down goes at once, so they read as loading meanwhile
    /// rather than as stopped, failed or reconnecting. A row whose Start or Retry runs keeps
    /// its row until its own listing lands ([`Reviews::start`]).
    fn connection_changed(&mut self, key: &ConnectionKey, up: bool, cx: &mut Context<Self>) {
        let through = self.rows_through(key, cx);
        if up {
            for location in &through {
                let Some(row) = self.rows.get_mut(location).filter(|row| !row.starting) else {
                    continue;
                };
                if row.unreachable.is_some() {
                    row.error = None;
                }
                row.unreachable = None;
                row.stopped = None;
                row.retry = false;
                row.reconnecting = false;
            }
        }
        for location in through {
            self.refresh(location, cx);
        }
    }

    /// Recomputes `location`'s row in the background: its reviews, its uncommitted changes and
    /// what its target labels need. A refresh already running for it is superseded. The
    /// repository is reached as a listing reaches it ([`Locations::reach`]): a stopped distro
    /// stays stopped.
    pub fn refresh(&mut self, location: Location, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get_mut(&location) else {
            return;
        };
        row.loading = true;
        self.generation += 1;
        let generation = self.generation;
        let reach = self
            .locations
            .update(cx, |locations, cx| locations.reach(location.clone(), cx));
        let task = cx.spawn({
            let location = location.clone();
            async move |this, cx| {
                let refreshed = match reach.await {
                    Reach::Opened(backend) => {
                        let scan = cx.background_spawn(async move { scan(backend) }).await;
                        if scan.lost_connection() {
                            Refreshed::Reconnecting
                        } else {
                            Refreshed::Scanned(scan)
                        }
                    }
                    Reach::Stopped { distro, error } => {
                        Refreshed::Unopened(Unopened::stopped(distro, error.as_deref()))
                    }
                    Reach::Reconnecting => Refreshed::Reconnecting,
                    Reach::Failed { error, retry } => {
                        Refreshed::Unopened(Unopened::new(&error, retry))
                    }
                };
                this.update(cx, |this, cx| {
                    this.finish_refresh(&location, generation, refreshed, cx)
                })
                .ok();
            }
        });
        self.refreshing.insert(location, (generation, task));
        cx.emit(ReviewsEvent::Changed);
        cx.notify();
    }

    fn finish_refresh(
        &mut self,
        location: &Location,
        generation: u64,
        refreshed: Refreshed,
        cx: &mut Context<Self>,
    ) {
        if self
            .refreshing
            .get(location)
            .is_none_or(|(current, _)| *current != generation)
        {
            return;
        }
        self.refreshing.remove(location);
        let Some(row) = self.rows.get_mut(location) else {
            return;
        };
        row.loading = false;
        // A Start or Retry ends with the listing that follows it ([`Reviews::start`]).
        row.starting = false;
        let mut resolved = None;
        match refreshed {
            Refreshed::Scanned(scan) => {
                resolved = Some(scan.root);
                row.labels = scan.labels;
                row.branches = scan.branches;
                row.unreachable = None;
                row.stopped = None;
                row.retry = false;
                row.reconnecting = false;
                row.toplevel = Some(scan.toplevel);
                row.error = scan.reviews.as_ref().err().cloned();
                row.reviews = scan.reviews.unwrap_or_default();
                row.working = match scan.working {
                    Ok(working) => Some(working),
                    Err(error) => {
                        row.error.get_or_insert(error);
                        None
                    }
                };
            }
            // The rows the last listing had stay, read again once it is back.
            Refreshed::Reconnecting => {
                row.reconnecting = true;
                row.unreachable = None;
                row.stopped = None;
                row.retry = false;
            }
            Refreshed::Unopened(Unopened {
                failure,
                reason,
                stopped,
                retry,
            }) => {
                row.error = Some(failure);
                row.unreachable = Some(reason);
                row.stopped = stopped;
                row.retry = retry;
                row.reconnecting = false;
                row.working = None;
                row.reviews.clear();
            }
        }
        // The listing resolved the repository: known by its root from now on (module docs).
        if let Some(root) = resolved {
            self.resolved(location, &root, cx);
        }
        cx.emit(ReviewsEvent::Changed);
        cx.notify();
    }

    /// **Start** on a repository whose host isn't running, or **Retry** on one whose
    /// connection failed (module docs): an explicit open, which connects, booting a stopped
    /// distro, then refreshes every known row reached through the same connection. The row
    /// says it is starting meanwhile, until the listing after the start lands; a start that
    /// fails leaves the row saying why.
    pub fn start(
        &mut self,
        location: Location,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), Failure>> {
        let Some(key) = self.locations.read(cx).key_for(&location) else {
            return Task::ready(Err(Failure::new(format!(
                "{} isn't reached through dv backend --stdio: there is nothing to start",
                location.display_name()
            ))));
        };
        if let Some(row) = self.rows.get_mut(&location) {
            row.starting = true;
            row.unreachable = None;
            row.error = None;
        }
        // A listing begun before the start would end it before its own listing lands.
        self.refreshing.remove(&location);
        self.opening += 1;
        cx.emit(ReviewsEvent::Changed);
        cx.notify();
        let open = self
            .locations
            .update(cx, |locations, cx| locations.open(location.clone(), cx));
        detached(cx, async move |this, cx| {
            let opened = open.await;
            this.update(cx, |this, cx| {
                this.opening -= 1;
                // Resolved: known by its root from now on (module docs).
                let location = match &opened {
                    Ok(backend) => {
                        let root = backend.repo().location.clone();
                        this.resolved(&location, &root, cx);
                        root
                    }
                    Err(_) => location,
                };
                // Every row that shares the connection: the distro's other repositories. The
                // row stays starting until its listing lands, so it never shows rows it
                // doesn't have meanwhile.
                this.refresh_connection(&key, cx);
                this.refresh(location, cx);
                cx.notify();
                opened.map(drop).map_err(|error| Failure::from(&*error))
            })
            .unwrap_or_else(|_| Err(closing()))
        })
    }

    /// Knows `location` from now on, unless it is known already however its WSL distro is
    /// spelled (module docs); the location as known.
    fn add_known(&mut self, location: Location, cx: &mut Context<Self>) -> Location {
        if let Some(known) = self.known.find_place(&location) {
            return known.location.clone();
        }
        let now = dv_core::review::event::now_ms();
        if self.known.add(location.clone(), now) {
            if let Some(repo) = self.known.get(&location) {
                self.rows.insert(location.clone(), RepoRow::new(repo));
            }
            self.save(cx);
            cx.emit(ReviewsEvent::Changed);
        }
        location
    }

    /// Forgets the known repository at `location` ("Remove from list", DESIGN §4.1): its row
    /// and any refresh of it go, and `known_repos.json` is saved without it. Nothing in the
    /// repository changes, its reviews included; opening it again adds it back with them. A
    /// refresh still running for it finds no row and is dropped. `false` when it wasn't known.
    pub fn remove_repo(&mut self, location: &Location, cx: &mut Context<Self>) -> bool {
        if !self.known.remove(location) {
            return false;
        }
        self.rows.remove(location);
        self.refreshing.remove(location);
        self.save(cx);
        cx.emit(ReviewsEvent::Changed);
        cx.notify();
        true
    }

    /// Closes the open review `review` of the repository at `location` as the reviewer
    /// (`ops::set_status`, PLAN §2.4: `closed` is final), from the switcher's row. The
    /// repository is reached as an open reaches it, and every row is listed again whether or
    /// not it closed, since another worktree's row may list the same review: a review that
    /// was finished meanwhile leaves its row either way.
    pub fn close_review(
        &mut self,
        location: Location,
        review: ReviewId,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), Failure>> {
        let open = self
            .locations
            .update(cx, |locations, cx| locations.open(location, cx));
        detached(cx, async move |this, cx| {
            let closed = match open.await {
                Ok(backend) => {
                    cx.background_spawn(async move {
                        ops::set_status(
                            &ReviewStore::new(backend.clone()),
                            &review,
                            ReviewStatus::Closed,
                            &reviewer_identity(backend.as_ref()),
                            None,
                        )
                        .map(|_| ())
                        .map_err(|error| Failure::from(&error))
                    })
                    .await
                }
                Err(error) => Err(Failure::from(&*error)),
            };
            this.update(cx, |this, cx| this.refresh_all(cx)).ok();
            closed
        })
    }

    /// A listing or an open of `given` reached the repository whose worktree root is `root`
    /// (`RepoInfo::location`, its distro spelled WSL's way): every known location that names
    /// it otherwise — `given` (a subfolder, another spelling of the distro) and any other
    /// spelling of `root` — is known as `root` from now on, its row re-keyed with what it
    /// listed, or dropped when `root` has a row already (module docs).
    fn resolved(&mut self, given: &Location, root: &Location, cx: &mut Context<Self>) {
        let stale: Vec<Location> = self
            .known
            .repos()
            .map(|repo| &repo.location)
            .filter(|known| *known != root && (*known == given || known.same_place(root)))
            .cloned()
            .collect();
        if stale.is_empty() {
            return;
        }
        for old in stale {
            self.known.rekey(&old, root);
            self.refreshing.remove(&old);
            let Some(mut row) = self.rows.remove(&old) else {
                continue;
            };
            if self.rows.contains_key(root) {
                continue;
            }
            let fresh = self.known.get(root).map(RepoRow::new);
            if let Some(fresh) = fresh {
                row.location = fresh.location;
                row.name = fresh.name;
                row.path = fresh.path;
                row.last_used = fresh.last_used;
                self.rows.insert(root.clone(), row);
            }
        }
        if let Some(row) = self.rows.get_mut(root) {
            row.last_used = self.known.get(root).map_or(0, KnownRepo::last_used);
        }
        self.save(cx);
        cx.emit(ReviewsEvent::Changed);
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let json = self.known.to_json();
        self.file.update(cx, |file, cx| file.save(json, cx));
    }
}

/// Runs `work` on the foreground as a detached task, so its bookkeeping (the `opening` count,
/// the known repos) happens even when the caller drops the returned task, which only carries
/// the result.
fn detached<T: Send + 'static>(
    cx: &mut Context<Reviews>,
    work: impl AsyncFnOnce(WeakEntity<Reviews>, &mut AsyncApp) -> Result<T, Failure> + 'static,
) -> Task<Result<T, Failure>> {
    let (sender, receiver) = oneshot::channel();
    cx.spawn(async move |this, cx| {
        let _ = sender.send(work(this, cx).await);
    })
    .detach();
    cx.background_spawn(async move { receiver.await.unwrap_or_else(|_| Err(closing())) })
}

fn closing() -> Failure {
    Failure::new("dv is closing")
}

/// What [`find_blocking`] found: the review, whether it was just created, and its home.
struct Found {
    id: ReviewId,
    created: bool,
    home: Home,
}

/// `ops::open_review` as the reviewer (or the review asked for by id) in `backend`'s
/// worktree, and the review's home (blocking). A review whose home worktree is gone fails,
/// saying so.
fn find_blocking(backend: Arc<dyn Backend>, request: OpenRequest) -> Result<Found, Failure> {
    let store = ReviewStore::new(backend.clone());
    let (review, created) = match request {
        OpenRequest::Target(target) => {
            let opened =
                ops::open_review_as(&store, target, || reviewer_identity(backend.as_ref()))
                    .map_err(|e| Failure::from(&e))?;
            (opened.review, opened.created)
        }
        OpenRequest::Review(id) => (
            store.load(&id).map_err(|e| Failure::from(&e))?.review,
            false,
        ),
    };
    let home = home(&GitRepo::new(backend), &review.target, &review.worktree).map_err(|error| {
        match error {
            HomeError::Git(error) => Failure::from(&error),
            // The headline says what is wrong; the path, long, is the detail.
            HomeError::Gone { worktree } => Failure::with_detail(
                format!(
                    "review {} can't be opened: its worktree is gone (removed, moved or deleted)",
                    review.id
                ),
                format!("{worktree} is no longer one of the repository's worktrees"),
            ),
        }
    })?;
    Ok(Found {
        id: review.id,
        created,
        home,
    })
}

/// What [`load_blocking`] read.
struct Loading {
    loaded: LoadedReview,
    reviewer: Author,
    labels: LabelContext,
}

/// The review's full load (warnings included) through `backend`, its home worktree's, with
/// who the reviewer is there and that worktree's labels (blocking): the three at once, since
/// none waits on another's git calls (PLAN §17's budgets).
fn load_blocking(backend: Arc<dyn Backend>, id: &ReviewId) -> Result<Loading, Failure> {
    let (reviewer, labels, loaded) = std::thread::scope(|scope| {
        let reviewer = scope.spawn(|| reviewer_identity(backend.as_ref()));
        let labels = scope.spawn(|| LabelContext::read(&GitRepo::new(backend.clone())));
        let loaded = ReviewStore::new(backend.clone()).load(id);
        (joined(reviewer), joined(labels), loaded)
    });
    let loaded = loaded.map_err(|e| Failure::from(&e))?;
    Ok(Loading {
        loaded,
        reviewer,
        labels,
    })
}

/// A scoped thread's result; its panic, if it panicked, goes on in the caller.
fn joined<T>(job: std::thread::ScopedJoinHandle<'_, T>) -> T {
    job.join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// One run of [`Reviews::open`]: `open` (the location's own, begun) and then the review found,
/// its home worktree opened when it is another one, and the review loaded. A failure of the
/// later steps that lost the connection under it reads as the open's ([`couldnt_open`]).
async fn open_once(
    locations: &Entity<Locations>,
    location: &Location,
    request: OpenRequest,
    open: Task<super::locations::OpenResult>,
    cx: &mut AsyncApp,
) -> Result<OpenedReview, Failure> {
    let origin = match open.await {
        Ok(origin) => origin,
        Err(error) => {
            let (named, connection) = locations.read_with(cx, |locations, _| {
                (
                    locations.canonical(location),
                    locations.connection_for(location),
                )
            });
            return Err(open_failure(&named, &error, connection.as_deref()));
        }
    };
    let found = async {
        let job = cx.background_spawn({
            let backend = origin.clone();
            async move { find_blocking(backend, request) }
        });
        let Found { id, created, home } = job.await?;
        let backend = match &home {
            Home::Worktree(root) if !home.is(&origin.repo().location, &origin.repo().toplevel) => {
                let there = sibling(&origin.repo().location, root);
                locations
                    .update(cx, |locations, cx| locations.open(there, cx))
                    .await
                    .map_err(|error| Failure::from(&*error))?
            }
            Home::Worktree(_) | Home::Anywhere => origin.clone(),
        };
        let job = cx.background_spawn({
            let backend = backend.clone();
            async move { load_blocking(backend, &id) }
        });
        let loading = job.await?;
        Ok::<_, Failure>((backend, created, loading))
    }
    .await;
    let (backend, created, loading) = match found {
        Ok(found) => found,
        Err(failure) if failure.disconnected => {
            let connection =
                locations.read_with(cx, |locations, _| locations.connection_of(&origin));
            return Err(couldnt_open(
                &origin.repo().location,
                failure,
                connection.as_deref(),
            ));
        }
        Err(failure) => return Err(failure),
    };
    let Loading {
        loaded,
        reviewer,
        labels,
    } = loading;
    let connection = locations.read_with(cx, |locations, _| locations.connection_of(&backend));
    Ok(OpenedReview {
        origin: origin.repo().location.clone(),
        backend,
        loaded,
        created,
        reviewer,
        labels,
        connection,
    })
}

/// Why the repository at `location` didn't open, for the main area's banner (DESIGN §4.4): a
/// remote repository whose connection is the trouble reads "couldn't open <location>"
/// ([`couldnt_open`]); anything else as the error says.
fn open_failure(
    location: &Location,
    error: &BackendError,
    connection: Option<&Connection>,
) -> Failure {
    let down = matches!(
        error,
        BackendError::Unavailable { .. } | BackendError::Disconnected { .. }
    );
    let remote = matches!(location, Location::Wsl { .. }) || connection.is_some();
    if !down || !remote {
        return Failure::from(error);
    }
    couldnt_open(location, Failure::from(error), connection)
}

/// "couldn't open <location>": one line for the headline, `failure` (what the connection's
/// trouble was) and the connection's diagnostics in the detail, which Copy diagnostics carries;
/// still flagged [`Failure::disconnected`] when the connection was lost under the open.
fn couldnt_open(location: &Location, failure: Failure, connection: Option<&Connection>) -> Failure {
    let mut detail = failure.message;
    if let Some(more) = &failure.detail {
        detail.push('\n');
        detail.push_str(more);
    }
    if let Some(connection) = connection {
        detail.push_str("\n\n");
        detail.push_str(connection.diagnostics().trim_end());
    }
    Failure {
        message: format!("couldn't open {}", location.display_name()),
        detail: Some(detail),
        disconnected: failure.disconnected,
    }
}

/// The location of the worktree whose root is `root` (as git lists it), another worktree of
/// the repository at `location`: a path on this machine, or in the same distro.
fn sibling(location: &Location, root: &str) -> Location {
    match location {
        Location::Local(_) => Location::Local(std::path::PathBuf::from(root)),
        Location::Wsl { distro, .. } => Location::Wsl {
            distro: distro.clone(),
            path: root.to_string(),
        },
    }
}

/// Reads a repository's row (blocking): its reviews, its uncommitted changes and its labels.
fn scan(backend: Arc<dyn Backend>) -> Scan {
    let toplevel = backend.repo().toplevel.clone();
    let root = backend.repo().location.clone();
    let repo = GitRepo::new(backend.clone());
    // Another worktree's `working` or `staged` review is named by that worktree's HEAD.
    let all_labels = Labels::read(&repo);
    let labels = all_labels.here.clone();
    let store = ReviewStore::new(backend);
    let reviews = store
        .list()
        .map(|entries| {
            entries
                .into_iter()
                .map(|entry| match entry {
                    ListEntry::Ok(summary) => ReviewRow::Ok {
                        label: all_labels.label(&summary.target, &summary.worktree),
                        summary,
                    },
                    ListEntry::Err { id, error } => ReviewRow::Err {
                        id,
                        error: Failure::from(&error),
                    },
                })
                .collect()
        })
        .map_err(|error| Failure::from(&error));
    let working = Target::Working
        .resolve(&repo)
        .map_err(|error| Failure::from(&error))
        .and_then(|spec| file_list(&repo, &spec).map_err(|error| Failure::from(&error)))
        .map(|files| WorkingSummary {
            files: files.len(),
            additions: files.iter().map(|file| u64::from(file.additions)).sum(),
            deletions: files.iter().map(|file| u64::from(file.deletions)).sum(),
        });
    let branches = repo
        .branches(labels.default_base.as_deref())
        .unwrap_or_else(|error| {
            log::warn!("couldn't list the branches of {toplevel}: {error}");
            Vec::new()
        });
    Scan {
        working,
        reviews,
        labels,
        branches,
        toplevel,
        root,
    }
}
