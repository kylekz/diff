//! App-wide services (PLAN §11), each a gpui entity shared by every window, installed once as
//! the [`Services`] global by [`init`] before the first window opens. Views reach them through
//! [`ActiveServices::services`].
//!
//! - [`Settings`] (`settings`): the window material, the light/dark mode and the panel's width,
//!   saved in the data dir as they change.
//! - [`Locations`] (`locations`): the one backend per repository, opened off the UI thread,
//!   and the connections to the `dv backend --stdio` servers remote ones go through (PLAN §5).
//! - [`Reviews`] (`reviews`): the known repositories (`known_repos.json`, each with the GitHub
//!   repository its clone is) and, per repository, its reviews and uncommitted changes, as
//!   the switcher lists them.
//! - [`ReviewUi`] (`review_ui`): what the reviewer chose per review (`review_ui.json`: split or
//!   unified).
//! - [`GitHub`] (`github`): the app's `gh` runner, the pull requests it opens and refreshes
//!   off the UI thread, and each repository's list of open pull requests for the PR picker
//!   (PLAN §9).
//! - [`LanguageServers`] (`language_servers`): the language servers of every worktree a review
//!   is open on (`crate::intel::servers`, PLAN §18.5), with `settings.json`'s `lsp`.
//! - [`Setup`] (`setup`): every environment's setup checks, the first-run marker and the
//!   status bar's out-of-date skill notice (PLAN §12).
//! - [`Updates`] (`updates`): dv's own update, downloaded and ready to restart into (PLAN D9),
//!   which the binary reports.
//!
//! An open review is a `crate::session::ReviewSession`, which the shell holds; it works through
//! the same `Locations` backend. The `System` seam (opening URLs, revealing files, the folder
//! picker, the clipboard) is the separate [`crate::system`] global, because automation swaps it
//! for a recorder.

pub mod github;
pub mod locations;
pub mod persist;
pub mod review_ui;
pub mod reviews;
pub mod settings;
pub mod setup;
pub mod updates;

use std::path::{Path, PathBuf};

use gpui::{App, AppContext as _, Entity, Global};

use crate::intel::LanguageServers;

pub use github::GitHub;
pub use locations::{EnvOverlay, Locations, LocationsEvent, Reach, Route, WslAccess};
pub use review_ui::{ReviewUi, ViewMode};
pub use reviews::{Reviews, ReviewsEvent};
pub use settings::{Mode, Settings, SettingsEvent, SettingsFile};
pub use setup::{Setup, SetupConfig};
pub use updates::{Unsaved, UpdateStatus, Updater, Updates};

/// What [`init`] needs from the binary.
#[derive(Clone)]
pub struct ServicesConfig {
    /// Where dv keeps its files (`--data-dir`, `DV_DATA_DIR`, else the platform's; PLAN §13.3).
    pub data_dir: PathBuf,
    /// How WSL repositories are reached: this machine's WSL, or a stand-in in tests and in
    /// automation runs without `DV_WSL_HOME` ([`WslAccess::for_run`]).
    pub wsl: WslAccess,
    /// Where the setup checks look and write (PLAN §12): the real machine, or a scratch home
    /// and a stand-in `Path` under the data dir ([`SetupConfig::sandboxed`]).
    pub setup: SetupConfig,
}

/// The app's services.
pub struct Services {
    pub settings: Entity<Settings>,
    pub locations: Entity<Locations>,
    pub reviews: Entity<Reviews>,
    pub review_ui: Entity<ReviewUi>,
    pub github: Entity<GitHub>,
    pub language_servers: Entity<LanguageServers>,
    pub setup: Entity<Setup>,
    pub updates: Entity<Updates>,
    data_dir: PathBuf,
}

impl Global for Services {}

impl Services {
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Nothing is loading or waiting to be saved: automation's `settled`.
    pub fn is_settled(&self, cx: &App) -> bool {
        self.settings.read(cx).is_settled()
            && self.locations.read(cx).is_settled()
            && self.reviews.read(cx).is_settled(cx)
            && self.review_ui.read(cx).is_settled(cx)
            && self.github.read(cx).is_settled()
            && self.language_servers.read(cx).is_settled()
            && self.setup.read(cx).is_settled()
    }
}

/// Builds the services (reading the settings, the known repositories and the per-review
/// choices synchronously) and installs them as a global.
pub fn init(config: ServicesConfig, cx: &mut App) {
    let ServicesConfig {
        data_dir,
        wsl,
        setup,
    } = config;
    let path = data_dir.join(settings::SETTINGS_FILE);
    let settings = cx.new(|cx| Settings::load(path, cx));
    let locations = cx.new(|cx| Locations::new(wsl, cx));
    let github = cx.new(|_| GitHub::new(&setup.seams));
    let reviews = cx.new(|cx| Reviews::new(locations.clone(), github.clone(), &data_dir, cx));
    let review_ui = cx.new(|cx| ReviewUi::new(&data_dir, cx));
    let lsp = settings.read(cx).file().lsp_settings();
    let language_servers = cx.new(|cx| LanguageServers::new(lsp, cx));
    let setup = cx.new(|cx| Setup::new(setup, &data_dir, locations.clone(), cx));
    let updates = cx.new(|_| Updates::new());
    cx.set_global(Services {
        settings,
        locations,
        reviews,
        review_ui,
        github,
        language_servers,
        setup,
        updates,
        data_dir,
    });
}

/// Access to the installed [`Services`].
pub trait ActiveServices {
    /// The app's services. Panics if [`init`] hasn't run, which is a startup bug.
    fn services(&self) -> &Services;

    /// The services, if installed.
    fn try_services(&self) -> Option<&Services>;
}

impl ActiveServices for App {
    fn services(&self) -> &Services {
        self.global::<Services>()
    }

    fn try_services(&self) -> Option<&Services> {
        self.try_global::<Services>()
    }
}
