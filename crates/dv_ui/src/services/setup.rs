//! dv's setup in the app (PLAN §12, DESIGN §4.3): every environment's checks (`dv_cli::checks`:
//! this machine, then each WSL distro), the first-run marker, the fixes the first-run screen and
//! Settings › Setup apply, and the status bar's notice of an out-of-date skill.
//!
//! **Checking** runs on threads of its own (git, `gh`, `wsl.exe` block), each environment on
//! its own so a slow distro never holds this machine's rows back; a check that lands after a
//! newer one of the same environment started is dropped. At startup this machine's skills alone
//! are checked (for the notice; no git, `gh` or shell runs), everything on the first run. The
//! distros are checked when the setup screen asks (the first-run screen, Settings › Setup,
//! **Check again**) and each one once its connection comes up this session (a WSL repository
//! opened there), never by booting one: a stopped distro is listed as stopped, with **Start**,
//! which boots it (`dv_cli::checks::distro::start`) and checks it. Under the stand-in for WSL
//! (`WslAccess::Scripted`, an automation run without a scratch `DV_WSL_HOME`) the distros it
//! names are listed but never reached; a sandboxed automation run reaches only its sandbox's
//! distro (`Seams::only_distros`), for checks, starts and fixes alike.
//!
//! **Fixes** that write one file run once: Codex's and Cursor's skill is one file, and a
//! distro's `dv setup agents` installs every agent's skill, so a second such fix while the first
//! runs joins it. A fix's failure shows under its row until the row reads ok.
//!
//! **The first run** is the data dir without `onboarding.json` ([`ONBOARDING_FILE`], not the
//! first implementation's `setup.json`): the first-run screen shows until **Continue** or **Skip
//! for now** writes it. An automation run shows it only when asked (`first_run_allowed`), since
//! every scenario starts on a fresh data dir.
//!
//! **The notice** ([`Setup::notice`]): a skill that is out of date in any environment (an agent
//! working from an old skill misuses the CLI), or missing for an installed agent in a distro
//! (a WSL repository opened in a distro that isn't set up: PLAN §12.1), shows once in the status
//! bar, "dv-review skill out of date in Ubuntu · Update", until it is fixed or dismissed. A
//! skill missing on this machine is the first-run screen's and Setup's to offer, not a nag.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use dv_backend::Launcher;
use dv_backend::wsl::Distro;
use dv_cli::checks::{self, Check, Environment, Kind, Seams, State, distro, id, skills};
use futures::StreamExt as _;
use gpui::{AppContext as _, Context, Entity};
use serde_json::json;

use super::locations::{Locations, WslAccess};

/// The first-run marker in the data dir.
pub const ONBOARDING_FILE: &str = "onboarding.json";

/// The program an automation run or a test runs as `gh` when `DV_GH` names none: one that
/// isn't there, so nothing automated reaches the real `gh` (CLAUDE.md). The app's own, and
/// the one its `cli` children get.
pub const NO_GH: &str = "dv-sandbox-has-no-gh";

/// Whether `DV_GH`'s `value` names the program to run as `gh`: set and not empty. An empty
/// one is unset to the runner (`dv_core::github::program`), which would then run the real
/// `gh`.
pub fn names_gh(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// What the app's setup is built with (the `dv` binary decides; PLAN §14.1).
#[derive(Clone, Debug)]
pub struct SetupConfig {
    /// Where the checks look and write: the real home, `Path` and distros for the app, a scratch
    /// home and a stand-in `Path` under the data dir for an automation run.
    pub seams: Seams,
    /// Whether the first-run screen may show (an automation run: only when asked).
    pub first_run_allowed: bool,
}

impl SetupConfig {
    /// The app's own: this process's environment (the real home, the registry's `Path`, `gh`),
    /// the first-run screen allowed.
    pub fn real() -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        Self {
            seams: Seams::from_env(&dv_cli::Env::Process, &cwd, Vec::new()),
            first_run_allowed: true,
        }
    }

    /// An automation run's or a test's (CLAUDE.md): `home` as the home directory of every check
    /// and of the git and `gh` they run (its `.gitconfig`, no system configuration),
    /// `user_path` as the file standing in for the user `Path` (unless `DV_USER_PATH_FILE`
    /// names one), `gh` as `DV_GH` names it or a program that isn't there (in a distro too),
    /// and no distro reached but `DV_TEST_WSL_DISTRO`'s (whose home is the `DV_WSL_HOME`
    /// sandbox: without one the run's WSL is the stand-in, which reaches none).
    pub fn sandboxed(home: PathBuf, user_path: PathBuf, first_run_allowed: bool) -> Self {
        use std::ffi::OsString;
        let cwd = std::env::current_dir().unwrap_or_default();
        let set = |name: &str, value: &std::path::Path| {
            (OsString::from(name), Some(value.as_os_str().to_os_string()))
        };
        let child_env = vec![
            set("HOME", &home),
            set("USERPROFILE", &home),
            set("XDG_CONFIG_HOME", &home.join(".config")),
            set("GIT_CONFIG_GLOBAL", &home.join(".gitconfig")),
            (OsString::from("GIT_CONFIG_NOSYSTEM"), Some("1".into())),
            set("GH_CONFIG_DIR", &home.join(".config").join("gh")),
            (OsString::from("CODEX_HOME"), None),
        ];
        let mut seams = Seams::from_env(&dv_cli::Env::Process, &cwd, child_env);
        seams.home = Some(home);
        seams.codex_home = None;
        if std::env::var_os(dv_cli::checks::user_path::USER_PATH_FILE_VAR).is_none() {
            seams.user_path = dv_cli::checks::user_path::UserPath::file(user_path);
        }
        if !names_gh(std::env::var_os(dv_cli::checks::GH_VAR).as_deref()) {
            seams.gh = NO_GH.into();
        }
        seams.distro_env = vec![(
            dv_cli::checks::GH_VAR.to_string(),
            "/dv-sandbox-has-no-gh".to_string(),
        )];
        seams.only_distros = Some(sandbox_distros());
        Self {
            seams,
            first_run_allowed,
        }
    }
}

/// The only distro a sandboxed run reaches: `DV_TEST_WSL_DISTRO`'s, else none.
pub fn sandbox_distros() -> Vec<String> {
    std::env::var("DV_TEST_WSL_DISTRO")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .into_iter()
        .collect()
}

/// One environment as the setup screen shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvState {
    pub env: Environment,
    /// It is this machine (else a WSL distro seen from here).
    pub here: bool,
    /// A check of it is running; its rows are the last ones meanwhile.
    pub checking: bool,
    /// Its Start is booting it.
    pub starting: bool,
    /// Why its last Start failed.
    pub start_error: Option<String>,
}

impl EnvState {
    fn new(env: Environment, here: bool) -> Self {
        Self {
            env,
            here,
            checking: false,
            starting: false,
            start_error: None,
        }
    }

    fn distro(name: &str, running: bool) -> Self {
        Self::new(
            Environment {
                name: name.to_string(),
                kind: Kind::Wsl,
                running,
                note: None,
                checks: Vec::new(),
            },
            false,
        )
    }
}

/// A fix being applied: an install's bytes, when it streams them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applying {
    pub done: u64,
    pub total: u64,
}

/// The status bar's notice (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// The environment it is about.
    pub environment: String,
    /// "dv-review skill out of date in Ubuntu".
    pub text: String,
    /// Its button: "Update", or "Install" for a missing skill.
    pub action: &'static str,
    /// What dismissing it remembers, so it shows once.
    key: String,
}

/// How the first-run screen was left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirstRunEnd {
    Continue,
    Skip,
}

impl FirstRunEnd {
    fn key(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Skip => "skip",
        }
    }
}

/// The key a check's generation counts under: its environment's lowercase name, `""` for this
/// machine.
fn generation_key(here: bool, name: &str) -> String {
    if here {
        String::new()
    } else {
        name.to_lowercase()
    }
}

/// A fix being applied, and what it writes: fixes with the same `writes` run once (module
/// docs).
#[derive(Clone, Debug)]
struct Fixing {
    progress: Applying,
    writes: String,
}

/// The setup service (module docs).
pub struct Setup {
    seams: Arc<Seams>,
    wsl: WslAccess,
    locations: Entity<Locations>,
    marker: PathBuf,
    first_run: bool,
    /// This machine's environment, once checked, and whether that was every check (else the
    /// skills alone, at startup).
    here: Option<EnvState>,
    here_full: bool,
    /// Why the distros can't be listed, as a row of this machine's ([`id::WSL`]).
    wsl_row: Option<Check>,
    /// The distros, as last listed, by their own spelling.
    distros: Vec<EnvState>,
    /// Checks, listings, fixes and starts in flight.
    busy: usize,
    /// The last check started per environment ([`generation_key`]).
    generations: HashMap<String, u64>,
    /// Fixes being applied, by environment and check.
    applying: BTreeMap<(String, String), Fixing>,
    /// Why a fix last failed, by environment and check.
    failures: BTreeMap<(String, String), String>,
    /// Distros checked because their connection came up (once a session each).
    checked_on_connect: HashSet<String>,
    dismissed: HashSet<String>,
    _observe: gpui::Subscription,
}

impl Setup {
    pub fn new(
        config: SetupConfig,
        data_dir: &std::path::Path,
        locations: Entity<Locations>,
        cx: &mut Context<Self>,
    ) -> Self {
        let marker = data_dir.join(ONBOARDING_FILE);
        let first_run = config.first_run_allowed && !marker.exists();
        let wsl = locations.read(cx).wsl().clone();
        let observe = cx.observe(&locations, |this, _, cx| this.connections_changed(cx));
        let mut setup = Self {
            seams: Arc::new(config.seams),
            wsl,
            locations,
            marker,
            first_run,
            here: None,
            here_full: false,
            wsl_row: None,
            distros: Vec::new(),
            busy: 0,
            generations: HashMap::new(),
            applying: BTreeMap::new(),
            failures: BTreeMap::new(),
            checked_on_connect: HashSet::new(),
            dismissed: HashSet::new(),
            _observe: observe,
        };
        setup.check_here(first_run, cx);
        if first_run {
            setup.check_distros(cx);
        }
        setup
    }

    /// Whether the first-run screen shows.
    pub fn first_run(&self) -> bool {
        self.first_run
    }

    /// The only distros this app's checks reach, when it is restricted (an automation run's):
    /// what its `cli` children are given too (`DV_ONLY_DISTROS`).
    pub fn only_distros(&self) -> Option<&[String]> {
        self.seams.only_distros.as_deref()
    }

    /// Every environment: this machine (its distros' listing problem among its rows), then the
    /// distros. Empty until this machine's first check lands.
    pub fn environments(&self) -> Vec<EnvState> {
        let mut all = Vec::new();
        if let Some(here) = &self.here {
            let mut here = here.clone();
            here.env.checks.extend(self.wsl_row.clone());
            all.push(here);
        }
        all.extend(self.distros.iter().cloned());
        all
    }

    /// The fix of `env`'s check `id` being applied, if it is.
    pub fn applying(&self, env: &str, id: &str) -> Option<Applying> {
        self.applying
            .get(&(env.to_string(), id.to_string()))
            .map(|fixing| fixing.progress)
    }

    /// Why the fix of `env`'s check `id` last failed.
    pub fn failure(&self, env: &str, id: &str) -> Option<&str> {
        self.failures
            .get(&(env.to_string(), id.to_string()))
            .map(String::as_str)
    }

    /// Whether any check, listing, fix or start is in flight.
    pub fn is_checking(&self) -> bool {
        self.busy > 0
    }

    /// Nothing in flight: automation's `settled`.
    pub fn is_settled(&self) -> bool {
        self.busy == 0
    }

    /// **Check again**: everything on this machine and every distro.
    pub fn check_again(&mut self, cx: &mut Context<Self>) {
        self.check_here(true, cx);
        self.check_distros(cx);
    }

    /// The first-run screen's **Continue** or **Skip for now**: the marker written, the screen
    /// gone.
    pub fn finish_first_run(&mut self, how: FirstRunEnd, cx: &mut Context<Self>) {
        if !self.first_run {
            return;
        }
        self.first_run = false;
        let marker = self.marker.clone();
        let body = json!({
            "v": 1,
            "finished_at": dv_core::review::event::now_ms(),
            "how": how.key(),
        });
        self.run(
            cx,
            move || {
                super::persist::write_atomically(&marker, format!("{body}\n").as_bytes())
                    .map_err(|error| format!("writing {}: {error}", marker.display()))
            },
            |_, written, _| {
                if let Err(error) = written {
                    log::warn!("{error}");
                }
            },
        );
    }

    /// Starts a check of the environment `key`, returning its generation.
    fn next_generation(&mut self, key: String) -> u64 {
        let generation = self.generations.entry(key).or_insert(0);
        *generation += 1;
        *generation
    }

    fn current(&self, key: &str, generation: u64) -> bool {
        self.generations.get(key) == Some(&generation)
    }

    /// Checks this machine: every check (`full`), or the skills alone (module docs).
    fn check_here(&mut self, full: bool, cx: &mut Context<Self>) {
        if let Some(here) = &mut self.here {
            here.checking = true;
        }
        let generation = self.next_generation(generation_key(true, ""));
        let seams = self.seams.clone();
        self.run(
            cx,
            move || {
                if full {
                    checks::this_machine(&seams)
                } else {
                    checks::this_machine_skills(&seams)
                }
            },
            move |this, env, _| {
                if !this.current("", generation) {
                    return;
                }
                this.forget_fixed_failures(&env);
                this.here = Some(EnvState::new(env, true));
                this.here_full = full;
            },
        );
    }

    /// Drops the failures of `env`'s checks that read ok now (module docs).
    fn forget_fixed_failures(&mut self, env: &Environment) {
        for check in &env.checks {
            if !check.state.needs_fixing() {
                self.failures.remove(&(env.name.clone(), check.id.clone()));
            }
        }
    }

    /// Lists the distros (booting none) and checks each running one.
    fn check_distros(&mut self, cx: &mut Context<Self>) {
        if !cfg!(windows) {
            return;
        }
        let seams = self.seams.clone();
        let wsl = self.wsl.clone();
        self.run(
            cx,
            move || match &wsl {
                WslAccess::System => {
                    super::locations::wsl::guard_tests();
                    distro::listed(&seams).map_err(|unlisted| match unlisted {
                        distro::Unlisted::NoBuild(reason) => Some(reason),
                        distro::Unlisted::NoWsl => None,
                    })
                }
                WslAccess::Scripted(_) => Ok(wsl
                    .distros()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|distro| !distro::is_vm(&distro.name))
                    .collect()),
            },
            |this, listed, cx| match listed {
                Ok(distros) => {
                    this.wsl_row = None;
                    this.listed(distros, cx);
                }
                Err(reason) => {
                    this.distros.clear();
                    this.wsl_row = reason.map(|reason| Check {
                        id: id::WSL.to_string(),
                        title: "WSL distros".to_string(),
                        state: State::Skipped,
                        detail: reason,
                        fix: None,
                    });
                }
            },
        );
    }

    /// The distros as listed now: their order and running state, each running one checked.
    fn listed(&mut self, listed: Vec<Distro>, cx: &mut Context<Self>) {
        let before = std::mem::take(&mut self.distros);
        for distro in listed {
            let previous = before
                .iter()
                .find(|known| known.env.name.eq_ignore_ascii_case(&distro.name));
            let mut state = match previous {
                Some(previous) if distro.running && previous.env.running => previous.clone(),
                _ => EnvState::distro(&distro.name, distro.running),
            };
            // A failed Start says why until the next one.
            if let Some(previous) = previous {
                state.starting = previous.starting;
                state.start_error = previous.start_error.clone();
            }
            if !distro.running {
                state.env = distro::environment(&self.seams, &distro);
                // A stopped distro's check in flight no longer counts.
                self.next_generation(generation_key(false, &distro.name));
            } else if self.wsl.is_scripted() {
                state.env.note = Some(format!(
                    "this run doesn't reach {}: it starts dv in a WSL distro only with \
                     DV_WSL_HOME set",
                    distro.name
                ));
            }
            let check = distro.running && !self.wsl.is_scripted();
            self.distros.push(state);
            if check {
                self.check_distro(distro.name, cx);
            }
        }
    }

    /// Checks the distro `name`, which `dv_cli` asks afresh whether it runs before anything
    /// starts in it (a stopped one comes back stopped).
    fn check_distro(&mut self, name: String, cx: &mut Context<Self>) {
        if self.wsl.is_scripted() {
            return;
        }
        if let Some(state) = self.distro_mut(&name) {
            state.checking = true;
        }
        let key = generation_key(false, &name);
        let generation = self.next_generation(key.clone());
        let seams = self.seams.clone();
        let distro = Distro {
            name: name.clone(),
            running: true,
        };
        self.run(
            cx,
            move || distro::environment(&seams, &distro),
            move |this, env, _| {
                if !this.current(&key, generation) {
                    return;
                }
                this.forget_fixed_failures(&env);
                match this.distro_mut(&name) {
                    Some(state) => {
                        state.env = env;
                        state.checking = false;
                    }
                    None => this.distros.push(EnvState::new(env, false)),
                }
            },
        );
    }

    fn distro_mut(&mut self, name: &str) -> Option<&mut EnvState> {
        self.distros
            .iter_mut()
            .find(|state| state.env.name.eq_ignore_ascii_case(name))
    }

    /// A WSL connection that came up checks its distro, once a session (module docs).
    fn connections_changed(&mut self, cx: &mut Context<Self>) {
        let up: Vec<String> = self
            .locations
            .read(cx)
            .connections()
            .all()
            .into_iter()
            .filter(|connection| matches!(connection.launcher(), Launcher::Wsl { .. }))
            .filter(|connection| {
                matches!(connection.state(), dv_backend::ConnectionState::Connected)
            })
            .map(|connection| connection.name().to_string())
            .collect();
        for name in up {
            if self.checked_on_connect.insert(name.to_lowercase()) {
                if self.distro_mut(&name).is_none() {
                    self.distros.push(EnvState::distro(&name, true));
                }
                self.check_distro(name, cx);
            }
        }
    }

    /// What applying `env`'s check `id` writes (module docs): a skill's file on this machine,
    /// every skill in a distro, else the check itself.
    fn writes(&self, here: bool, env: &str, id: &str) -> String {
        if id::is_skill(id) {
            if !here {
                return format!("{env}/skills");
            }
            if let Some(path) = skills::skill_path(&self.seams, id) {
                return format!("{env}/{}", path.display());
            }
        }
        format!("{env}/{id}")
    }

    /// Applies the fix of `env`'s check `id`, then checks that environment again. A fix that
    /// writes what one in flight writes joins it (module docs).
    pub fn apply(&mut self, env: &str, id: &str, cx: &mut Context<Self>) {
        let key = (env.to_string(), id.to_string());
        if self.applying.contains_key(&key) {
            return;
        }
        let here = self.here.as_ref().is_some_and(|here| here.env.name == env);
        if !here && !self.seams.may_check(env) {
            self.failures.insert(
                key,
                format!("this run doesn't reach {env} (an automation run reaches only its sandbox's distro)"),
            );
            cx.notify();
            return;
        }
        let writes = self.writes(here, env, id);
        self.failures.remove(&key);
        let joined = self.applying.values().any(|fixing| fixing.writes == writes);
        self.applying.insert(
            key.clone(),
            Fixing {
                progress: Applying::default(),
                writes: writes.clone(),
            },
        );
        if joined {
            cx.notify();
            return;
        }
        let seams = self.seams.clone();
        let (env_name, check) = key.clone();
        let (progress, mut heard) = futures::channel::mpsc::unbounded::<(u64, u64)>();
        // An install's progress, as it streams.
        cx.spawn({
            let writes = writes.clone();
            async move |this, cx| {
                while let Some((done, total)) = heard.next().await {
                    let went = this.update(cx, |this, cx| {
                        for fixing in this.applying.values_mut() {
                            if fixing.writes == writes {
                                fixing.progress = Applying { done, total };
                            }
                        }
                        cx.notify();
                    });
                    if went.is_err() {
                        break;
                    }
                }
            }
        })
        .detach();
        self.run(
            cx,
            move || {
                if here {
                    checks::apply_here(&seams, &check)
                } else {
                    distro::apply(&seams, &env_name, &check, &|done, total| {
                        let _ = progress.unbounded_send((done, total));
                    })
                }
            },
            move |this, applied, cx| {
                let done: Vec<(String, String)> = this
                    .applying
                    .iter()
                    .filter(|(_, fixing)| fixing.writes == writes)
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in &done {
                    this.applying.remove(key);
                }
                match applied {
                    Ok(line) => log::info!("{line}"),
                    Err(reason) => {
                        for key in done {
                            this.failures.insert(key, reason.clone());
                        }
                    }
                }
                if here {
                    let full = this.here_full;
                    this.check_here(full, cx);
                } else {
                    this.check_distro(key.0.clone(), cx);
                }
            },
        );
    }

    /// **Start** on a stopped distro: boots it (the user's own explicit start, PLAN §5.2), then
    /// lists and checks it. A sandboxed run boots none but its own distro.
    pub fn start(&mut self, name: &str, cx: &mut Context<Self>) {
        let may = self.seams.may_check(name);
        let Some(state) = self.distro_mut(name) else {
            return;
        };
        if state.starting {
            return;
        }
        if !may {
            state.start_error = Some(format!(
                "this run doesn't reach {name} (an automation run reaches only its sandbox's \
                 distro)"
            ));
            cx.notify();
            return;
        }
        state.starting = true;
        state.start_error = None;
        let seams = self.seams.clone();
        let wsl = self.wsl.clone();
        let name = name.to_string();
        let distro_name = name.clone();
        self.run(
            cx,
            move || match wsl {
                WslAccess::System => distro::start(&seams, &distro_name),
                WslAccess::Scripted(_) => Err(super::locations::wsl::REFUSED.to_string()),
            },
            move |this, started, cx| {
                if let Some(state) = this.distro_mut(&name) {
                    state.starting = false;
                    state.start_error = started.err();
                }
                this.check_distros(cx);
            },
        );
    }

    /// The notice the status bar shows, if any (module docs). Not while the first-run screen
    /// shows: it says so itself.
    pub fn notice(&self) -> Option<Notice> {
        if self.first_run {
            return None;
        }
        self.environments().iter().find_map(|state| {
            let env = &state.env;
            let skills = env.checks.iter().filter(|check| id::is_skill(&check.id));
            let out_of_date = skills.clone().any(|check| check.state == State::OutOfDate);
            let missing = !state.here && skills.clone().any(|check| check.state == State::Missing);
            let (what, action) = if out_of_date {
                ("out of date", "Update")
            } else if missing {
                ("not installed", "Install")
            } else {
                return None;
            };
            let key = format!("{}/{what}", env.name.to_lowercase());
            if self.dismissed.contains(&key) {
                return None;
            }
            let place = if state.here && env.kind != Kind::Wsl {
                format!("on {}", env.name)
            } else {
                format!("in {}", env.name)
            };
            Some(Notice {
                environment: env.name.clone(),
                text: format!("dv-review skill {what} {place}"),
                action,
                key,
            })
        })
    }

    /// The notice's Update (or Install): every skill fix of its environment, each file once.
    pub fn apply_notice(&mut self, cx: &mut Context<Self>) {
        let Some(notice) = self.notice() else {
            return;
        };
        let envs = self.environments();
        let Some(state) = envs
            .iter()
            .find(|state| state.env.name == notice.environment)
        else {
            return;
        };
        let ids: Vec<String> = state
            .env
            .checks
            .iter()
            .filter(|check| id::is_skill(&check.id) && check.can_apply())
            .map(|check| check.id.clone())
            .collect();
        for id in ids {
            self.apply(&notice.environment, &id, cx);
        }
    }

    /// The notice's dismiss: it doesn't show again this session.
    pub fn dismiss_notice(&mut self, cx: &mut Context<Self>) {
        if let Some(notice) = self.notice() {
            self.dismissed.insert(notice.key);
            cx.notify();
        }
    }

    /// A distro environment as if checked (tests).
    #[cfg(test)]
    pub(crate) fn insert_distro_for_test(&mut self, env: Environment, cx: &mut Context<Self>) {
        self.distros.push(EnvState::new(env, false));
        cx.notify();
    }

    /// Runs `work`, which blocks (git, `gh`, `wsl.exe`), on the background executor (in tests,
    /// gpui's deterministic scheduler), counted as busy, then `done` with its result.
    fn run<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        self.busy += 1;
        let job = cx.background_spawn(async move { work() });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.busy -= 1;
                done(this, result, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}
