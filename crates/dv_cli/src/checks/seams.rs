//! Everything the checks read and write outside the process ([`Seams`]): the home directory,
//! the user `Path`, this executable, `gh`, git's environment, the Linux builds and the WSL
//! sandbox. The CLI takes them from its [`Context`] ([`Seams::from_context`]); the app builds
//! its own, pointing them into its data dir under `--automation` (CLAUDE.md: nothing automated
//! touches the real home, registry or distro).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use dv_backend::install::{self, LINUX_DIR_VAR, WSL_HOME_VAR};
use dv_backend::wsl;

use super::user_path::{MACHINE_PATH_FILE_VAR, USER_PATH_FILE_VAR, UserPath};
use crate::context::{Context, Env};

/// The variable naming the program dv runs as `gh` (PLAN D7; tests point it at `mock_gh`).
pub use dv_core::github::GH_VAR;

/// The variable that, when set, names the only WSL distros the checks may reach, separated by
/// commas (empty: none): an automation run's, and its `cli` children's (CLAUDE.md).
pub const ONLY_DISTROS_VAR: &str = "DV_ONLY_DISTROS";

/// Where the checks look and write (module docs).
#[derive(Debug, Clone)]
pub struct Seams {
    /// The home directory (`%USERPROFILE%`, `$HOME`): the agents' folders, `~/.local/bin`;
    /// `None` when it can't be told, which the checks that need it report.
    pub home: Option<PathBuf>,
    /// `$CODEX_HOME`, Codex's own folder when it isn't `~/.codex`.
    pub codex_home: Option<PathBuf>,
    /// The user's and the machine's `Path` (Windows): the registry, or the files that stand in
    /// for them.
    pub user_path: UserPath,
    /// The environment `%NAME%` in a `Path` entry expands from.
    pub vars: Env,
    /// This dv executable: its folder is the one `dv setup path` puts on `PATH` (Windows), the
    /// build `~/.local/bin/dv` should run (Linux).
    pub exe: Option<PathBuf>,
    /// The program run as `gh`: `$DV_GH`, else `gh`.
    pub gh: OsString,
    /// Environment changes for every program the checks run (git, `gh`): a test's variables
    /// and its fixture's git overlay, the scratch home under automation; nothing for the real
    /// CLI, whose children inherit its environment.
    pub child_env: Vec<(OsString, Option<OsString>)>,
    /// Where this app's Linux builds are (Windows: `DV_LINUX_DIR`, else beside this
    /// executable).
    pub linux_dir: PathBuf,
    /// `DV_WSL_HOME` as set, the sandbox every distro check runs with (`dv_backend::wsl::
    /// sandbox` decides what it means).
    pub wsl_home: Option<String>,
    /// The WSL distro this dv runs inside, when it does (`$WSL_DISTRO_NAME`).
    pub this_distro: Option<String>,
    /// The only distros checked, started or set up, when set ([`ONLY_DISTROS_VAR`]): an
    /// automation run's, so it reaches no distro but the one the run is sandboxed in. `None`:
    /// every distro.
    pub only_distros: Option<Vec<String>>,
    /// Variables a distro's dv runs with besides the sandbox's home: what crosses with a
    /// command run there ([`distro_env`], PLAN §10.4), or a test's or an automation run's own
    /// (`DV_GH` naming a program that isn't there, so nothing asks GitHub).
    pub distro_env: Vec<(String, String)>,
}

/// [`Seams::child_env`] for a command run with `ctx`: the variables of an explicit environment
/// (a test's, `Env::Vars`), which a child wouldn't otherwise see, then the git overlay.
fn child_env(ctx: &Context) -> Vec<(OsString, Option<OsString>)> {
    let mut env = Vec::new();
    if let Env::Vars(vars) = &ctx.env {
        env.extend(
            vars.iter()
                .map(|(name, value)| (OsString::from(name), Some(OsString::from(value)))),
        );
    }
    env.extend(ctx.git_env.iter().cloned());
    env
}

/// [`Seams::distro_env`] read from `env`: what a command run inside a distro takes across
/// besides who it acts as (`reexec::crossing`): `GIT_ALLOW_PROTOCOL` whenever it is set, and
/// `DV_GH` when the checks are sandboxed (`DV_WSL_HOME`), so a sandboxed run's distro asks the
/// same stand-in `gh`, or none.
fn distro_env(env: &Env) -> Vec<(String, String)> {
    crate::reexec::crossing(env, wsl::is_sandbox(env.var_lossy(WSL_HOME_VAR)))
        .into_iter()
        .filter(|name| !crate::reexec::ACTS_AS.contains(name))
        .filter_map(|name| Some((name.to_string(), env.var(name)?)))
        .collect()
}

impl Seams {
    /// The CLI's: its environment and working directory, this executable, its git overlay.
    pub fn from_context(ctx: &Context) -> Self {
        Self::from_env(&ctx.env, &ctx.cwd, child_env(ctx))
    }

    /// Read from `env` (`$CODEX_HOME` relative to `cwd`), this executable, and `child_env` for
    /// the programs the checks run: what the app builds for itself (`Env::Process`).
    pub fn from_env(env: &Env, cwd: &Path, child_env: Vec<(OsString, Option<OsString>)>) -> Self {
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        Self {
            home: env.non_empty(home_var).map(PathBuf::from),
            codex_home: env.non_empty("CODEX_HOME").map(|dir| cwd.join(dir)),
            user_path: UserPath::from_vars(
                env.var(USER_PATH_FILE_VAR),
                env.var(MACHINE_PATH_FILE_VAR),
            ),
            vars: env.clone(),
            exe: crate::this_executable(env).ok(),
            gh: dv_core::github::program(env.non_empty(GH_VAR).map(OsString::from)),
            child_env,
            linux_dir: install::linux_dir(env.var(LINUX_DIR_VAR)),
            wsl_home: env.var_lossy(WSL_HOME_VAR),
            this_distro: crate::repo::this_distro(env),
            only_distros: env.var(ONLY_DISTROS_VAR).map(|names| {
                names
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
                    .collect()
            }),
            distro_env: distro_env(env),
        }
    }

    /// `%NAME%`'s value for a `Path` entry ([`Seams::vars`]).
    pub fn var(&self, name: &str) -> Option<String> {
        self.vars.var(name)
    }

    /// The folder `dv setup path` puts on `PATH` on Windows: this executable's.
    pub fn exe_dir(&self) -> Option<&Path> {
        self.exe.as_deref().and_then(Path::parent)
    }

    /// The home directory, or why the checks can't tell it.
    pub fn home(&self) -> Result<&Path, String> {
        self.home.as_deref().ok_or_else(|| {
            let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
            format!("can't tell the home directory: ${name} isn't set")
        })
    }

    /// Whether the distro `name` may be checked ([`Seams::only_distros`]).
    pub fn may_check(&self, name: &str) -> bool {
        self.only_distros.as_ref().is_none_or(|only| {
            only.iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(name))
        })
    }
}
