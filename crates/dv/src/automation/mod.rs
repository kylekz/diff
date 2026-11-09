//! dv's automation host (PLAN §14): the `state` document e2e scenarios assert on, the `settled`
//! predicate and dv's own commands. Built only with the `automation` feature and started only
//! with `--automation` / `--automation-pipe`. `tests/e2e/README.md` "dv's `state`" documents
//! every key and command; `gpui_automation` adds `window` and `windows` itself.
//!
//! `state` ([`state`]): `app`, `data_dir`, `settings`, `shell`, `system`, `repos` (the known
//! repositories), `switcher` and `source_picker` (the title bar's overlays), `file_list` (the
//! panel), `session` (the open review, or `null`), `stream` (the diff stream, or `null`),
//! `comments_list`, `find`, `file_view`, `file_finder`, `palette`, `history`, `intel` (code
//! intelligence: the language servers, the hover card, the references), `cli` (the
//! `cli_spawn` children not yet joined), `backends` (the connections to `dv backend
//! --stdio` servers), `setup` (the first-run screen, Settings and every environment's
//! setup checks, PLAN §12) and `update` (dv's own update ready to restart into, PLAN D9).
//!
//! Commands:
//! - `set_setting {key, value}` changes `material` (`mica`, `acrylic`, `opaque`) or `mode`
//!   (`system`, `dark`, `light`) through the settings service, exactly as the UI would: it saves
//!   to the data dir and the window repaints in the new theme before the reply.
//! - `seed_repo {name, steps, backend?}` builds a fixture repo at `<data_dir>/repos/<name>` and
//!   `repo_edit {name, steps}` changes one ("the agent edits"; [`repos`]). With `"backend":
//!   "loopback"` the repo opens through the loopback connection `loopback`: this executable's
//!   `backend --stdio` as a child, as a WSL repo opens through its distro's (PLAN §5.2).
//! - `backend_kill {name}` kills a connection's server as a crash would, `backend_block
//!   {name, blocked}` makes the loopback's respawns fail (to reach `failed`) or start again,
//!   `backend_hold {name, held}` makes its starts wait (a start in progress) or go on, and
//!   `backend_stop {name}` stops the loopback's host as `wsl --shutdown` stops a distro
//!   (to reach `stopped`) ([`backends`]); `wsl_distros {distros}` tells the stand-in for WSL
//!   which distros there are and which run, in a run without `DV_WSL_HOME`
//!   (`dv_ui::services::WslAccess`).
//! - `open {path}`, `open_review {path, target?}` and `review_op {op, thread, …}` ([`commands`]).
//! - `update_ready {version}` makes an update of dv ready through the seam (the status bar's
//!   Restart to update, which records the restart, [`commands`]).
//! - `home_file {path, contents}` writes a file in the run's scratch home (`<data_dir>/home`,
//!   the home of the setup checks and the `cli` children): a fake agent's folder, a skill an
//!   older dv installed, a `.gitconfig` ([`commands`]).
//! - `cli {args, …}`, `cli_spawn {name, args, …}` and `cli_join {name, timeout_ms?}` run this
//!   binary's headless commands as child processes: the agent's side of a scenario through the
//!   real CLI ([`agent_cli`]).
//!
//! Seeded repos, and their linked worktrees, open with their fixture's environment
//! ([`dv_ui::services::Locations`]'s overlay), never the owner's git configuration, and are found
//! again after a restart.

mod agent_cli;
mod backends;
mod commands;
mod repos;
mod state;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use dv_test_support::FixtureRepo;
use dv_ui::services::locations::EnvOverlay;
use dv_ui::services::{Mode, Route};
use dv_ui::theme::Palette;
use dv_ui::{ActiveServices as _, Material, Shell};
use gpui::{App, AppContext as _, Context, Entity, Subscription, Task, Window};
use gpui_automation::AutomationHost;
use serde_json::{Value, json};

/// A seeded fixture repo, shared with the background jobs that edit it.
type Seeded = Arc<Mutex<FixtureRepo>>;

/// How a seeded repo is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeededBackend {
    /// In process, as a local repository is.
    Local,
    /// Through the loopback connection [`backends::LOOPBACK`].
    Loopback,
}

/// The entity `gpui_automation` drives: the shell it describes, the repos it seeded and the
/// CLI children it started.
pub struct DvAutomation {
    shell: Entity<Shell>,
    seeded: HashMap<String, (Seeded, SeededBackend)>,
    /// The switches of the loopback connections this host made ([`backends`]).
    switches: HashMap<String, backends::Switches>,
    /// How the loopback's server starts and respawns ([`backends::Loopback`]).
    loopback: backends::Loopback,
    /// `cli` runs and `cli_spawn` children not yet collected ([`agent_cli`]).
    children: Arc<Mutex<agent_cli::Children>>,
    /// The program `cli` starts: this process's own executable.
    cli_exe: Option<PathBuf>,
    /// Kills the children still running as the app quits.
    _quit: Subscription,
}

impl DvAutomation {
    /// A host over `shell`. Repos seeded by an earlier run on the same data dir (before a
    /// `restart`) are found again and their environments registered, so they keep opening
    /// with the fixture's configuration.
    pub fn new(shell: Entity<Shell>, cx: &mut App) -> Self {
        let children: Arc<Mutex<agent_cli::Children>> = Arc::default();
        // The hook holds the registry itself: the driver lets go of this host as it quits,
        // before the app runs its quit hooks.
        let quit = cx.on_app_quit({
            let children = children.clone();
            move |_| {
                agent_cli::lock(&children).kill_all();
                async {}
            }
        });
        let mut host = Self {
            shell,
            seeded: HashMap::new(),
            switches: HashMap::new(),
            loopback: backends::Loopback::default(),
            children,
            cli_exe: std::env::current_exe().ok(),
            _quit: quit,
        };
        for (name, fixture, backend) in repos::reopen_seeded(cx.services().data_dir()) {
            let roots = repos::roots(&fixture);
            host.register(name, fixture, backend, roots, cx);
        }
        host
    }

    /// Keeps `fixture` as the seeded repo `name` and has every backend opened for it, or for
    /// one of its linked worktrees (`roots`, [`repos::roots`]), use its environment, through
    /// `backend`.
    fn register(
        &mut self,
        name: String,
        fixture: FixtureRepo,
        backend: SeededBackend,
        roots: Vec<PathBuf>,
        cx: &mut App,
    ) {
        self.register_roots(roots, fixture.env_overlay(), backend, cx);
        self.seeded
            .insert(name, (Arc::new(Mutex::new(fixture)), backend));
    }

    /// Has every backend opened for one of `roots` (a seeded repo's work tree and worktrees)
    /// use `env`, the fixture's environment, reached through `backend`.
    fn register_roots(
        &mut self,
        roots: Vec<PathBuf>,
        env: EnvOverlay,
        backend: SeededBackend,
        cx: &mut App,
    ) {
        let connection = match backend {
            SeededBackend::Local => None,
            SeededBackend::Loopback => Some(self.loopback(cx)),
        };
        let locations = cx.services().locations.clone();
        locations.update(cx, |locations, _| {
            for root in roots {
                let route = Route {
                    env: env.clone(),
                    connection: connection.clone(),
                };
                locations.register(root, route);
            }
        });
    }
}

impl AutomationHost for DvAutomation {
    fn state(&self, window: &Window, cx: &App) -> Value {
        json!({
            "app": "dv",
            "data_dir": cx.services().data_dir().display().to_string(),
            "settings": state::settings(cx),
            "shell": state::shell(self.shell.read(cx), window, cx),
            "system": state::system(cx),
            "repos": state::repos(cx),
            "switcher": state::switcher(self.shell.read(cx), cx),
            "source_picker": state::source_picker(self.shell.read(cx), cx),
            "file_list": state::file_list(self.shell.read(cx), cx),
            "session": state::session(self.shell.read(cx), cx),
            "stream": state::stream(self.shell.read(cx), window, cx),
            "find": state::find(self.shell.read(cx), window, cx),
            "file_view": state::file_view(self.shell.read(cx), cx),
            "file_finder": state::file_finder(self.shell.read(cx), window, cx),
            "palette": state::palette(self.shell.read(cx), window, cx),
            "history": state::history(self.shell.read(cx)),
            "intel": state::intel(self.shell.read(cx), cx),
            "comments_list": state::comments_list(self.shell.read(cx), cx),
            "cli": self.cli_state(),
            "backends": state::backends(cx),
            "setup": state::setup(self.shell.read(cx), cx),
            "update": state::update(cx),
        })
    }

    fn settled(&self, cx: &App) -> bool {
        let shell = self.shell.read(cx);
        cx.services().is_settled(cx)
            && shell.is_settled(cx)
            && shell.diff_pane().read(cx).is_settled()
            && shell.file_view().read(cx).is_settled()
            && shell
                .session()
                .is_none_or(|session| session.read(cx).is_settled())
            && shell
                .intel()
                .is_none_or(|intel| intel.read(cx).is_settled(cx))
    }

    fn commands(&self) -> &'static [&'static str] {
        COMMANDS
    }

    fn custom(
        &mut self,
        cmd: &str,
        args: Value,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Result<Value>> {
        match cmd {
            "set_setting" => Some(set_setting(&args, cx)),
            "backend_block" => Some(self.backend_block(args)),
            "backend_hold" => Some(self.backend_hold(args)),
            "wsl_distros" => Some(backends::wsl_distros(args, cx)),
            "home_file" => Some(commands::home_file(&args, cx)),
            "update_ready" => Some(commands::update_ready(&args, cx)),
            _ => None,
        }
    }

    fn custom_task(
        &mut self,
        cmd: &str,
        args: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<Value>>> {
        let task = match cmd {
            "seed_repo" => self.seed_repo(args, cx),
            "repo_edit" => self.repo_edit(args, cx),
            "open" => commands::open(self.shell.clone(), args, window, cx),
            "open_review" => commands::open_review(self.shell.clone(), args, window, cx),
            "review_op" => commands::review_op(self.shell.clone(), args, cx),
            "cli" => self.cli(args, cx),
            "cli_spawn" => self.cli_spawn(args, cx),
            "cli_join" => self.cli_join(args, cx),
            "backend_kill" => backends::backend_kill(args, cx),
            "backend_stop" => self.backend_stop(args, cx),
            _ => return None,
        };
        Some(task.unwrap_or_else(|error| Task::ready(Err(error))))
    }
}

/**
 * Every command `custom` and `custom_task` answer, sent in the `ready` handshake so the runner
 * rejects a scenario's misspelt command before its first step (toolbox-rs docs/automation.md
 * "Protocol").
 */
const COMMANDS: &[&str] = &[
    "set_setting",
    "backend_block",
    "backend_hold",
    "wsl_distros",
    "home_file",
    "update_ready",
    "seed_repo",
    "repo_edit",
    "open",
    "open_review",
    "review_op",
    "cli",
    "cli_spawn",
    "cli_join",
    "backend_kill",
    "backend_stop",
];

/// A `set_setting` request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingChange {
    Palette(Palette),
    Material(Material),
    Mode(Mode),
}

impl SettingChange {
    fn parse(args: &Value) -> Result<Self> {
        let key = args
            .get("key")
            .and_then(Value::as_str)
            .context("set_setting needs \"key\": \"palette\", \"material\" or \"mode\"")?;
        let value = args
            .get("value")
            .and_then(Value::as_str)
            .with_context(|| format!("set_setting {key:?} needs a string \"value\""))?;
        match key {
            "palette" => Palette::from_key(value).map(Self::Palette).ok_or_else(|| {
                anyhow!("palette must be \"aura\", \"claude\" or \"dracula\", not {value:?}")
            }),
            "material" => Material::from_key(value)
                .map(Self::Material)
                .ok_or_else(|| {
                    anyhow!(
                        "material must be \"mica\", \"acrylic\", \"vibrancy\" or \"opaque\", \
                         not {value:?}"
                    )
                }),
            "mode" => Mode::from_key(value).map(Self::Mode).ok_or_else(|| {
                anyhow!("mode must be \"system\", \"dark\" or \"light\", not {value:?}")
            }),
            other => {
                bail!("unknown setting {other:?}; dv has \"palette\", \"material\" and \"mode\"")
            }
        }
    }
}

/// `set_setting {key, value}`: the change through the settings service; replies with
/// `state.settings` after it.
fn set_setting(args: &Value, cx: &mut Context<DvAutomation>) -> Result<Value> {
    let change = SettingChange::parse(args)?;
    let settings = cx.services().settings.clone();
    settings.update(cx, |settings, cx| match change {
        SettingChange::Palette(palette) => settings.set_palette(palette, cx),
        SettingChange::Material(material) => settings.set_material(material, cx),
        SettingChange::Mode(mode) => settings.set_mode(mode, cx),
    });
    Ok(json!({"settings": state::settings(cx)}))
}

/// Starts automation on the main window.
pub fn start(
    window: gpui::AnyWindowHandle,
    shell: Entity<Shell>,
    pipe: Option<String>,
    cx: &mut App,
) -> Result<()> {
    let host = cx.new(|cx| DvAutomation::new(shell, cx));
    let options = gpui_automation::Options {
        pipe,
        ..Default::default()
    };
    gpui_automation::start(window, host, cx, options)
}
