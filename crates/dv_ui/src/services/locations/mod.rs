//! Where repositories live and the backends that reach them (PLAN §11 `Locations`, §5).
//!
//! [`Locations::open`] turns a [`Location`] into the one `Arc<dyn Backend>` dv keeps for that
//! repository, opened off the UI thread (opening runs git, and may start a distro) and cached by
//! both the location asked for and the worktree root it resolved to, so the switcher, the
//! reviews list and an open review share a backend.
//!
//! - **A local path** opens a [`LocalBackend`] with the user's own git configuration, unless a
//!   [`Route`] is registered for it ([`Locations::register`]): an environment overlay (the
//!   automation host registers its seeded fixture repos', so scenarios never depend on the
//!   owner's git configuration, PLAN §14.2), and optionally a loopback [`Connection`], through
//!   which it opens as a `RemoteBackend` (the e2e scenarios' `"backend": "loopback"`).
//! - **A WSL location** opens through its distro's connection in the app's [`Connections`]
//!   (PLAN §5.2), a `RemoteBackend` whose server is that distro's `dv backend --stdio`,
//!   started as [`WslAccess`] says.
//!
//! **The connecting rule** (PLAN §5.2). An explicit open ([`Locations::open`]: the user opening
//! a repository or a review, `dv open`, Start or Retry in the switcher) is the connection's
//! explicit start (`Connection::retry`): it connects whatever the connection's state, booting
//! a stopped distro, starting a failed or stopped connection once more; one that meets a
//! reconnect waits for it and goes on once it is back (or fails as the connection did). A
//! listing
//! ([`Locations::reach`]: the switcher's rows) only ever starts a server automatically, which
//! the connection does only on a host that runs: it goes through a connection already made, or
//! makes one only for a distro that is running. What it finds instead is the row's: a distro
//! that isn't running ([`Reach::Stopped`], whatever its connection did before), a connection
//! reconnecting ([`Reach::Reconnecting`]) or one that failed ([`Reach::Failed`] with `retry`).
//!
//! **Staleness.** A cached local backend whose worktree root has since disappeared is dropped
//! and the location opened again, so a deleted repo reports "no such directory" instead of a
//! git spawn error; that check reads this machine's file system, so it applies to local
//! backends only. A cached remote backend is kept, its connection started as the open allows,
//! also when an open fails only because its connection is down; a repository gone inside the
//! distro surfaces as the next call's error.
//!
//! Every connection this service goes through is followed: a change of its state repaints
//! whoever observes the service (the status bar says "installing dv…" while a review opens),
//! and one that leaves it reconnecting, stopped or failed, or brings it back from there, is a
//! [`LocationsEvent::ConnectionChanged`] (the reviews service lists its rows again).
//!
//! Which connection a location goes through ([`Locations::key_for`]) is known without the
//! file system, which the UI thread asking never reads: a WSL location's distro, else what the
//! last open of it found (its cached backend's connection, or its route's).
//!
//! **A distro's spelling.** `wsl.exe` takes distro names without case, so `\\wsl$\ubuntu\…` and
//! `\\wsl.localhost\Ubuntu\…` are one repository, reached through one connection
//! (`ConnectionKey` folds the name). That connection is named as WSL spells the distro
//! (`wsl.exe --list`): an explicit open of a distro dv hasn't seen lists WSL's distros first
//! (off the UI thread; the listing is cached for 2 s), and a listing's open names a connection
//! it makes from the list it reads anyway, so every location the connection hands back
//! (`RepoInfo::location`) is spelled WSL's way. [`Locations::canonical`] spells any location
//! so, from what dv has seen; where WSL can't be listed the name stays as given, and callers
//! compare locations with the distro folded (`Location::same_place`).

pub mod wsl;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dv_backend::{Asked, Connection, ConnectionKey, ConnectionState, Connections};
use dv_core::{Backend, BackendError, LocalBackend, LocalBackendOptions, Location};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::{AppContext as _, Context, EventEmitter, Task};

pub use wsl::WslAccess;

/// Environment changes for every git a backend spawns, as `LocalBackendOptions::env` takes
/// them: `(name, Some(value))` sets a variable, `(name, None)` removes it.
pub type EnvOverlay = Vec<(OsString, Option<OsString>)>;

/// What [`Locations::open`] hands back: the backend, or why there is none. The error is shared
/// because more than one view may show it.
pub type OpenResult = Result<Arc<dyn Backend>, Arc<BackendError>>;

/// What a listing found ([`Locations::reach`]).
#[derive(Clone)]
pub enum Reach {
    Opened(Arc<dyn Backend>),
    /// The repository's host isn't running: a WSL distro that isn't, whatever its connection
    /// did before, or a connection whose automatic start found its host stopped. Only an
    /// explicit open starts it (module docs). `distro` names it: as WSL lists it, or as the
    /// connection does; `error` is why its connection failed, when it did (a start that
    /// didn't boot it).
    Stopped {
        distro: String,
        error: Option<Arc<BackendError>>,
    },
    /// Its connection is reconnecting (PLAN §5.2): it comes back by itself.
    Reconnecting,
    /// It can't be reached: `error` says why, and `retry` that its connection failed, which an
    /// explicit open starts once more.
    Failed {
        error: Arc<BackendError>,
        retry: bool,
    },
}

/// What [`Locations`] tells its subscribers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocationsEvent {
    /// The connection of `key` started reconnecting, stopped or failed, or (`up`) came back
    /// from there (module docs).
    ConnectionChanged { key: ConnectionKey, up: bool },
}

/// How the repositories under a registered root are reached.
#[derive(Clone, Default)]
pub struct Route {
    /// Laid over the environment of every git their backends run.
    pub env: EnvOverlay,
    /// The loopback server they open through, as `RemoteBackend`s; `None` opens them in
    /// process.
    pub connection: Option<Arc<Connection>>,
}

/// A backend this service opened, and the connection it reaches its repository through.
#[derive(Clone)]
struct Cached {
    backend: Arc<dyn Backend>,
    connection: Option<Arc<Connection>>,
}

/// What an open found ([`OpenJob::run`]): the backend or why there is none, and the
/// connection the location goes through, when it goes through one made already.
struct Landed {
    opened: Result<Cached, Unreached>,
    connection: Option<Arc<Connection>>,
}

/// Why a repository wasn't reached.
enum Unreached {
    Stopped {
        distro: String,
        error: Option<BackendError>,
    },
    Reconnecting,
    Error {
        error: BackendError,
        retry: bool,
    },
}

impl From<BackendError> for Unreached {
    fn from(error: BackendError) -> Self {
        Unreached::Error {
            error,
            retry: false,
        }
    }
}

/// A connection state's kind as a listing cares about it ([`LocationsEvent`]): starting and
/// installing are on their way to one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    Idle,
    Connected,
    Reconnecting,
    Stopped,
    Failed,
}

impl Standing {
    fn of(state: &ConnectionState) -> Option<Self> {
        Some(match state {
            ConnectionState::Idle => Self::Idle,
            ConnectionState::Connected => Self::Connected,
            ConnectionState::Reconnecting { .. } => Self::Reconnecting,
            ConnectionState::Stopped => Self::Stopped,
            ConnectionState::Failed { .. } => Self::Failed,
            ConnectionState::Connecting | ConnectionState::Installing { .. } => return None,
        })
    }

    /// Whether going from `self` to `to` changes what a listing shows of its rows: down
    /// (reconnecting, stopped, failed), or back up from there. An idle close and the start
    /// after it don't: the rows stay as they are.
    fn changes_listing(self, to: Standing) -> bool {
        let down = |standing| {
            matches!(
                standing,
                Standing::Reconnecting | Standing::Stopped | Standing::Failed
            )
        };
        self != to && (down(to) || (to == Standing::Connected && down(self)))
    }
}

/// Whether an open may start a distro and retry a failed connection (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Explicit,
    Listing,
}

/// The backends of the repositories dv has opened (see the module docs).
pub struct Locations {
    backends: HashMap<Location, Cached>,
    /// Routes by the root they apply to (a path and everything below it).
    routes: Vec<(PathBuf, Route)>,
    /// Laid over the environment of every git a backend opened from now on runs, under its
    /// route's own ([`Locations::set_base_env`]).
    base_env: EnvOverlay,
    connections: Connections,
    wsl: WslAccess,
    /// The connections whose state changes repaint this service's observers.
    followed: HashMap<ConnectionKey, dv_backend::Subscription>,
    /// How each followed connection last stood ([`LocationsEvent`]).
    standing: HashMap<ConnectionKey, Standing>,
    /// What a followed connection's callback sends, to [`Locations::new`]'s task.
    changed: mpsc::UnboundedSender<(ConnectionKey, ConnectionState)>,
    _follow: Task<()>,
    /// The connection each location opened goes through, as its last open found it
    /// ([`Locations::key_for`]).
    through: HashMap<Location, Arc<Connection>>,
    /// WSL's own spelling of each distro listed so far, by its name folded (module docs).
    spellings: HashMap<String, String>,
    /// Opens in flight.
    opening: usize,
}

impl Locations {
    /// No repository opened yet; WSL reached as `wsl` says.
    pub fn new(wsl: WslAccess, cx: &mut Context<Self>) -> Self {
        let (changed, mut changes) = mpsc::unbounded::<(ConnectionKey, ConnectionState)>();
        let follow = cx.spawn(async move |this, cx| {
            while let Some((key, state)) = changes.next().await {
                let heard = this.update(cx, |this, cx| this.connection_changed(key, &state, cx));
                if heard.is_err() {
                    break;
                }
            }
        });
        Self {
            backends: HashMap::new(),
            routes: Vec::new(),
            base_env: Vec::new(),
            connections: Connections::new(),
            wsl,
            followed: HashMap::new(),
            standing: HashMap::new(),
            changed,
            _follow: follow,
            through: HashMap::new(),
            spellings: HashMap::new(),
            opening: 0,
        }
    }

    /// Makes every backend opened for `root`, or a path inside it, go as `route` says.
    /// Registering a root again replaces its route; a backend already open keeps the one it
    /// opened with.
    pub fn register(&mut self, root: PathBuf, route: Route) {
        if let Some(connection) = &route.connection {
            let key = ConnectionKey::loopback(connection.name());
            self.follow(key, connection);
        }
        self.routes.retain(|(known, _)| known != &root);
        self.routes.push((root, route));
    }

    /// [`Locations::register`] with an environment overlay alone: `root`'s repositories open in
    /// process, their git run with `env` laid over the inherited environment.
    pub fn register_env(&mut self, root: PathBuf, env: EnvOverlay) {
        self.register(
            root,
            Route {
                env,
                connection: None,
            },
        );
    }

    /// Has every backend opened from now on run its git with `env` laid over the inherited
    /// environment, whatever its root; a route's own environment goes over it. An automation
    /// run's: git that reaches nothing but a local path (PLAN §14.2).
    pub fn set_base_env(&mut self, env: EnvOverlay) {
        self.base_env = env;
    }

    /// The app's connections (automation kills and blocks them).
    pub fn connections(&self) -> &Connections {
        &self.connections
    }

    /// How WSL is reached (automation tells a stand-in its distros).
    pub fn wsl(&self) -> &WslAccess {
        &self.wsl
    }

    /// The backend already open for `location`, if any.
    pub fn cached(&self, location: &Location) -> Option<Arc<dyn Backend>> {
        self.backends
            .get(location)
            .map(|cached| cached.backend.clone())
    }

    /// The connection `backend` (one this service opened) reaches its repository through;
    /// `None` for a backend in process.
    pub fn connection_of(&self, backend: &Arc<dyn Backend>) -> Option<Arc<Connection>> {
        self.backends
            .values()
            .find(|cached| Arc::ptr_eq(&cached.backend, backend))
            .and_then(|cached| cached.connection.clone())
    }

    /// The key of the connection `location` is reached through, when it is reached through
    /// one (module docs): its distro's for a WSL location, else that of the loopback
    /// [`Locations::connection_for`] names; `None` for a local one no open has reached yet.
    /// Reads no file system: the UI thread asks.
    pub fn key_for(&self, location: &Location) -> Option<ConnectionKey> {
        match location {
            Location::Wsl { distro, .. } => Some(ConnectionKey::wsl(distro)),
            Location::Local(_) => self
                .connection_for(location)
                .map(|connection| ConnectionKey::loopback(connection.name())),
        }
    }

    /// The connection `location` is reached through, when one is made: its distro's for a WSL
    /// location, else the one its cached backend came through, or its route named when it was
    /// last opened (whether or not that open got through).
    pub fn connection_for(&self, location: &Location) -> Option<Arc<Connection>> {
        match location {
            Location::Wsl { distro, .. } => self.connections.get(&ConnectionKey::wsl(distro)),
            Location::Local(_) => self
                .backends
                .get(location)
                .and_then(|cached| cached.connection.clone())
                .or_else(|| self.through.get(location).cloned()),
        }
    }

    /// Nothing is opening: automation's `settled`.
    pub fn is_settled(&self) -> bool {
        self.opening == 0
    }

    /// `location` with its distro spelled as WSL spells it (module docs), as far as dv has
    /// seen: from a listing of WSL's distros, else from the connection already made to it; as
    /// given otherwise, and a local location as it is. Reads nothing: the UI thread asks.
    pub fn canonical(&self, location: &Location) -> Location {
        match location {
            Location::Wsl { distro, path } => Location::Wsl {
                distro: self.spelling(distro).unwrap_or_else(|| distro.clone()),
                path: path.clone(),
            },
            Location::Local(_) => location.clone(),
        }
    }

    /// WSL's spelling of `distro`, when a listing or a connection has told it.
    fn spelling(&self, distro: &str) -> Option<String> {
        self.spellings
            .get(&distro.to_lowercase())
            .cloned()
            .or_else(|| {
                self.connections
                    .get(&ConnectionKey::wsl(distro))
                    .map(|connection| connection.name().to_string())
            })
    }

    /// The backend for `location`, opened explicitly (module docs): the cached one while it
    /// is still good, else a newly opened one, starting its distro or retrying its failed
    /// connection as needed. Never blocks the UI thread. The open finishes (and is cached)
    /// even if the returned task is dropped.
    pub fn open(&mut self, location: Location, cx: &mut Context<Self>) -> Task<OpenResult> {
        let reached = self.start_spelled(location.clone(), Mode::Explicit, cx);
        cx.background_spawn(async move {
            match reached.await {
                Ok(backend) => Ok(backend),
                Err(Unreached::Error { error, .. }) => Err(Arc::new(error)),
                // An explicit open starts the distro, and waits out a reconnect, which ends
                // connected, failed, or with the distro stopped (the user shut it down
                // meanwhile): that last one is why it didn't open.
                Err(Unreached::Stopped { distro, .. }) => {
                    Err(Arc::new(BackendError::Unavailable {
                        location: location.display_name(),
                        reason: format!("{distro} isn't running"),
                    }))
                }
                // A listing's alone (`OpenJob::unreached`): never what an explicit open meets.
                Err(Unreached::Reconnecting) => Err(Arc::new(BackendError::Disconnected {
                    location: location.display_name(),
                    reason: "dv is reconnecting".to_string(),
                })),
            }
        })
    }

    /// The backend for `location` as a listing reaches it (module docs): through a connection
    /// already made, or a new one only to a running distro; never booting or retrying
    /// anything.
    pub fn reach(&mut self, location: Location, cx: &mut Context<Self>) -> Task<Reach> {
        let reached = self.start_spelled(location, Mode::Listing, cx);
        cx.background_spawn(async move {
            match reached.await {
                Ok(backend) => Reach::Opened(backend),
                Err(Unreached::Stopped { distro, error }) => Reach::Stopped {
                    distro,
                    error: error.map(Arc::new),
                },
                Err(Unreached::Reconnecting) => Reach::Reconnecting,
                Err(Unreached::Error { error, retry }) => Reach::Failed {
                    error: Arc::new(error),
                    retry,
                },
            }
        })
    }

    /// [`Locations::start`] with `location` spelled as WSL spells it (module docs). An explicit
    /// open of a distro whose spelling dv hasn't seen lists WSL's distros first, in the
    /// background, so the connection it makes is named WSL's way; a listing never lists for
    /// this (its own open names a connection it makes from the list it reads).
    fn start_spelled(
        &mut self,
        location: Location,
        mode: Mode,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<dyn Backend>, Unreached>> {
        let unspelled = match &location {
            Location::Wsl { distro, .. } => {
                mode == Mode::Explicit && self.spelling(distro).is_none()
            }
            Location::Local(_) => false,
        };
        if !unspelled {
            let location = self.canonical(&location);
            return self.start(location, mode, cx);
        }
        self.opening += 1;
        let wsl = self.wsl.clone();
        let listed = cx.background_spawn(async move { wsl.distros() });
        cx.spawn(async move |this, cx| {
            let listed = listed.await;
            let started = this.update(cx, |this, cx| {
                this.opening -= 1;
                match listed {
                    Ok(distros) => this.learn(&distros),
                    // The name stays as given; the open says what is wrong with WSL.
                    Err(error) => log::debug!("couldn't list WSL's distros: {error}"),
                }
                let location = this.canonical(&location);
                this.start(location, mode, cx)
            });
            match started {
                Ok(started) => started.await,
                Err(_) => Err(Unreached::from(BackendError::Unavailable {
                    location: location.display_name(),
                    reason: "dv is closing".to_string(),
                })),
            }
        })
    }

    /// Keeps WSL's spelling of every distro in `distros` (module docs).
    fn learn(&mut self, distros: &[dv_backend::wsl::Distro]) {
        for distro in distros {
            self.spellings
                .insert(distro.name.to_lowercase(), distro.name.clone());
        }
    }

    /// Opens `location` in the background as `mode` allows, and caches what it finds.
    fn start(
        &mut self,
        location: Location,
        mode: Mode,
        cx: &mut Context<Self>,
    ) -> Task<Result<Arc<dyn Backend>, Unreached>> {
        let cached = self.backends.get(&location).cloned();
        // An explicit open makes its distro's connection here, so its state is followed from
        // its first `Connecting`; a listing goes through one only when it exists (or, in the
        // background, makes one for a running distro).
        let distro = match (&location, mode) {
            (Location::Wsl { distro, .. }, Mode::Explicit) => Some(self.wsl_connection(distro)),
            (Location::Wsl { distro, .. }, Mode::Listing) => {
                let key = ConnectionKey::wsl(distro);
                let existing = self.connections.get(&key);
                if let Some(connection) = &existing {
                    self.follow(key, connection);
                }
                existing
            }
            (Location::Local(_), _) => None,
        };
        let job = OpenJob {
            location: location.clone(),
            cached: cached.clone(),
            routes: self.routes.clone(),
            base_env: self.base_env.clone(),
            distro,
            connections: self.connections.clone(),
            wsl: self.wsl.clone(),
            mode,
        };
        self.opening += 1;
        let opened = cx.background_spawn(async move { job.run() });
        let (sender, receiver) = oneshot::channel();
        cx.spawn({
            let location = location.clone();
            async move |this, cx| {
                let opened = opened.await;
                let landed = this.update(cx, |this, cx| {
                    this.opening -= 1;
                    cx.notify();
                    if let Some(connection) = opened.connection {
                        this.through.insert(location.clone(), connection);
                    }
                    this.opened(&location, cached.as_ref(), opened.opened)
                });
                let _ = sender.send(landed);
            }
        })
        .detach();
        cx.background_spawn(async move {
            match receiver.await {
                Ok(Ok(landed)) => landed,
                Ok(Err(_)) | Err(_) => Err(Unreached::from(BackendError::Unavailable {
                    location: location.display_name(),
                    reason: "dv is closing".to_string(),
                })),
            }
        })
    }

    /// `distro`'s connection, made the first time (nothing starts yet) and followed.
    fn wsl_connection(&mut self, distro: &str) -> Arc<Connection> {
        let key = ConnectionKey::wsl(distro);
        let wsl = self.wsl.clone();
        let connection = self.connections.get_or_insert_with(key.clone(), || {
            Connection::new(distro, wsl.launcher(distro))
        });
        self.follow(key, &connection);
        connection
    }

    /// Repaints this service's observers whenever `connection`'s state changes, and tells
    /// them when it changes what a listing shows (module docs).
    fn follow(&mut self, key: ConnectionKey, connection: &Connection) {
        if self.followed.contains_key(&key) {
            return;
        }
        let changed = self.changed.clone();
        let heard = key.clone();
        let subscription = connection.subscribe(move |state| {
            let _ = changed.unbounded_send((heard.clone(), state.clone()));
        });
        if let Some(standing) = Standing::of(&connection.state()) {
            self.standing.insert(key.clone(), standing);
        }
        self.followed.insert(key, subscription);
    }

    /// `key`'s connection is `state` now (module docs).
    fn connection_changed(
        &mut self,
        key: ConnectionKey,
        state: &ConnectionState,
        cx: &mut Context<Self>,
    ) {
        if let Some(now) = Standing::of(state) {
            let before = self.standing.insert(key.clone(), now);
            if before.unwrap_or(Standing::Idle).changes_listing(now) {
                cx.emit(LocationsEvent::ConnectionChanged {
                    key,
                    up: now == Standing::Connected,
                });
            }
        }
        cx.notify();
    }

    /// Caches what an open of `location` produced, given what was cached when it started.
    /// When another open of the same repository finished first (nothing was cached then),
    /// that backend wins, so a repository has one backend; a cached backend the open found
    /// stale is replaced, or forgotten when the location no longer opens. One whose connection
    /// is merely down (stopped, reconnecting, failed, or lost under the open) is kept.
    fn opened(
        &mut self,
        location: &Location,
        cached: Option<&Cached>,
        opened: Result<Cached, Unreached>,
    ) -> Result<Arc<dyn Backend>, Unreached> {
        let forget = |backends: &mut HashMap<Location, Cached>, stale: &Cached| {
            backends.retain(|_, kept| !Arc::ptr_eq(&kept.backend, &stale.backend));
        };
        match opened {
            Ok(fresh) => {
                if let Some(connection) = &fresh.connection {
                    let key = match &fresh.backend.repo().location {
                        Location::Wsl { distro, .. } => ConnectionKey::wsl(distro),
                        Location::Local(_) => ConnectionKey::loopback(connection.name()),
                    };
                    self.follow(key, connection);
                }
                let root = fresh.backend.repo().location.clone();
                let kept = match (cached, self.backends.get(&root)) {
                    (None, Some(first)) => first.clone(),
                    _ => fresh,
                };
                if let Some(stale) =
                    cached.filter(|stale| !Arc::ptr_eq(&stale.backend, &kept.backend))
                {
                    forget(&mut self.backends, stale);
                }
                self.backends.insert(location.clone(), kept.clone());
                self.backends.insert(root, kept.clone());
                Ok(kept.backend)
            }
            // The connection is down, not the repository gone: the backend is kept for when
            // it is back (a remote backend outlives its server).
            Err(
                unreached @ (Unreached::Stopped { .. }
                | Unreached::Reconnecting
                | Unreached::Error { retry: true, .. }
                | Unreached::Error {
                    error: BackendError::Disconnected { .. },
                    ..
                }),
            ) if cached.is_some() => Err(unreached),
            Err(unreached) => {
                self.backends.remove(location);
                if let Some(stale) = cached {
                    forget(&mut self.backends, stale);
                }
                Err(unreached)
            }
        }
    }
}

impl EventEmitter<LocationsEvent> for Locations {}

/// One open, run off the UI thread ([`Locations::start`]).
struct OpenJob {
    location: Location,
    cached: Option<Cached>,
    routes: Vec<(PathBuf, Route)>,
    base_env: EnvOverlay,
    /// A WSL location's connection, when there is one to go through.
    distro: Option<Arc<Connection>>,
    connections: Connections,
    wsl: WslAccess,
    mode: Mode,
}

impl OpenJob {
    /// Opens the location (blocking), and says which connection it goes through.
    fn run(self) -> Landed {
        // The route is read here, off the UI thread: it compares canonical paths.
        let route = match &self.location {
            Location::Local(path) => route_for(path, &self.routes),
            Location::Wsl { .. } => None,
        };
        let connection = match &self.location {
            Location::Wsl { .. } => None,
            Location::Local(_) => self
                .cached
                .as_ref()
                .and_then(|cached| cached.connection.clone())
                .or_else(|| route.as_ref().and_then(|route| route.connection.clone())),
        };
        Landed {
            opened: self.open(route),
            connection,
        }
    }

    /// The cached backend while it is still good, else a new one as its `route`, or its
    /// distro, says. A connection already made is started first as the open allows (module
    /// docs).
    fn open(&self, route: Option<Route>) -> Result<Cached, Unreached> {
        let known = self
            .cached
            .as_ref()
            .and_then(|cached| cached.connection.clone())
            .or_else(|| self.distro.clone());
        if let Some(connection) = &known {
            self.start(connection)?;
        }
        if let Some(cached) = &self.cached {
            // A remote backend's connection is up now; a local one's repository must still be
            // where it was.
            let good = match &cached.connection {
                None => still_there(cached.backend.repo()),
                Some(_) => true,
            };
            if good {
                return Ok(cached.clone());
            }
        }
        match &self.location {
            Location::Local(path) => {
                let route = route.unwrap_or_default();
                // The route's own variables go over the base's.
                let env: EnvOverlay = self.base_env.iter().cloned().chain(route.env).collect();
                match &route.connection {
                    Some(connection) => {
                        let text = path.to_str().ok_or_else(|| BackendError::Unavailable {
                            location: path.display().to_string(),
                            reason: "its path isn't Unicode, which dv's protocol needs".into(),
                        })?;
                        self.through(connection, text, env)
                    }
                    None => {
                        let options = LocalBackendOptions { env };
                        Ok(Cached {
                            backend: Arc::new(LocalBackend::open_with(path, options)?),
                            connection: None,
                        })
                    }
                }
            }
            Location::Wsl { distro, path } => {
                let connection = match &self.distro {
                    Some(connection) => connection.clone(),
                    None => self.running(distro)?,
                };
                self.through(&connection, path, self.base_env.clone())
            }
        }
    }

    /// Starts `connection` as this open allows (module docs): an explicit open's explicit
    /// start (nothing to do once connected); a listing's automatic one, again from `Stopped`
    /// when the host runs by now, and none at all while it reconnects or after it failed.
    fn start(&self, connection: &Arc<Connection>) -> Result<(), Unreached> {
        let started = match (self.mode, connection.state()) {
            (Mode::Explicit, _) => connection.retry(),
            (Mode::Listing, ConnectionState::Reconnecting { .. }) => {
                return Err(Unreached::Reconnecting);
            }
            (Mode::Listing, ConnectionState::Failed { .. }) => {
                return Err(self.unreached(connection, None));
            }
            (Mode::Listing, ConnectionState::Stopped) => connection.resume(),
            (Mode::Listing, _) => connection.connect(),
        };
        started.map_err(|error| self.unreached(connection, Some(error)))
    }

    /// The connection a listing makes to `distro` when it runs (module docs): it is
    /// [`Unreached::Stopped`] when it doesn't, and an error when WSL has no such distro.
    fn running(&self, distro: &str) -> Result<Arc<Connection>, Unreached> {
        let unavailable = |reason: String| BackendError::Unavailable {
            location: self.location.display_name(),
            reason,
        };
        let distros = self
            .wsl
            .distros()
            .map_err(|error| unavailable(error.to_string()))?;
        let found = distros
            .iter()
            .find(|known| known.name.eq_ignore_ascii_case(distro))
            .ok_or_else(|| unavailable(format!("there is no WSL distro named {distro}")))?;
        if !found.running {
            return Err(Unreached::Stopped {
                distro: found.name.clone(),
                error: None,
            });
        }
        // Named as WSL lists it (module docs).
        let name = found.name.clone();
        let wsl = self.wsl.clone();
        Ok(self
            .connections
            .get_or_insert_with(ConnectionKey::wsl(distro), || {
                Connection::new(name.as_str(), wsl.launcher(&name))
            }))
    }

    /// Opens `path` (as the server names it) through `connection`, started as this open
    /// allows. An explicit open whose server died under it waits out the reconnect and tries
    /// once more.
    fn through(
        &self,
        connection: &Arc<Connection>,
        path: &str,
        env: EnvOverlay,
    ) -> Result<Cached, Unreached> {
        let mut again = self.mode == Mode::Explicit;
        let backend = loop {
            self.start(connection)?;
            match connection.open(path, env.clone()) {
                Ok(backend) => break backend,
                Err(BackendError::Disconnected { .. }) if again => again = false,
                Err(error) => return Err(self.unreached(connection, Some(error))),
            }
        };
        Ok(Cached {
            backend: Arc::new(backend),
            connection: Some(connection.clone()),
        })
    }

    /// Why this open didn't get through `connection`, from its state now and `error`, what
    /// the last call failed with. A connection that failed is one to retry (its backend kept),
    /// unless, for a listing, its host isn't running, which is one to start (DESIGN §4.1);
    /// the listing's host check takes the cached answer of WSL's distros (`Asked::Listing`).
    fn unreached(&self, connection: &Connection, error: Option<BackendError>) -> Unreached {
        let listing = self.mode == Mode::Listing;
        let failed = || BackendError::Unavailable {
            location: self.location.display_name(),
            reason: "the connection to dv failed".to_string(),
        };
        match connection.state() {
            ConnectionState::Stopped => Unreached::Stopped {
                distro: connection.name().to_string(),
                error: None,
            },
            ConnectionState::Reconnecting { .. } if listing => Unreached::Reconnecting,
            ConnectionState::Failed { .. } if listing => {
                // A failed connection's calls fail at once, saying why, and start nothing.
                let error = match connection.connect() {
                    Err(error) => error,
                    Ok(()) => error.unwrap_or_else(failed),
                };
                if connection.host_running(Asked::Listing) == Some(false) {
                    return Unreached::Stopped {
                        distro: connection.name().to_string(),
                        error: Some(error),
                    };
                }
                Unreached::Error { error, retry: true }
            }
            ConnectionState::Failed { .. } => Unreached::Error {
                error: error.unwrap_or_else(failed),
                retry: true,
            },
            _ => match error {
                Some(BackendError::Disconnected { .. }) if listing => Unreached::Reconnecting,
                Some(error) => Unreached::from(error),
                None => Unreached::from(BackendError::Unavailable {
                    location: self.location.display_name(),
                    reason: format!("dv in {} can't be reached", connection.name()),
                }),
            },
        }
    }
}

/// Whether the repository a local backend was opened on is still there: its worktree root,
/// the root's `.git` (a directory, or a file for a linked worktree) and its git dir. These are
/// this machine's paths; a remote backend's are its server's, and never checked here.
fn still_there(repo: &dv_core::backend::RepoInfo) -> bool {
    let toplevel = Path::new(&repo.toplevel);
    toplevel.is_dir()
        && std::fs::symlink_metadata(toplevel.join(".git")).is_ok()
        && Path::new(&repo.git_dir).is_dir()
}

/// The route registered for `path` or the closest root above it. Paths compare in their
/// canonical form, so `D:\repos\api`, `d:/repos/api/` and a path through a junction match.
fn route_for(path: &Path, routes: &[(PathBuf, Route)]) -> Option<Route> {
    let canonical = |path: &Path| std::fs::canonicalize(path).ok();
    let path = canonical(path)?;
    routes
        .iter()
        .filter_map(|(root, route)| {
            let root = canonical(root)?;
            path.starts_with(&root).then_some((root, route))
        })
        .max_by_key(|(root, _)| root.components().count())
        .map(|(_, route)| route.clone())
}
