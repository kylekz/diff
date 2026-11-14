//! What a command runs with (docs/PLAN.md §10): its command line, the working directory, the
//! environment, the three standard streams, whether they are this process's own and whether
//! they are terminals, and the environment overlay every git the command spawns gets.
//!
//! The `dv` binary builds one from the process ([`Context::process`]); tests build their own,
//! so commands run in-process, hermetically (the fixture's git configuration, no inherited
//! `CLAUDECODE` or `$DV_REVIEW`) and fast.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, IsTerminal as _, Read, Write};
use std::path::PathBuf;

/// The environment a command reads its variables from (`$DV_REVIEW`, `$DV_AUTHOR`,
/// `$DV_MODEL`, `CLAUDECODE`, PLAN §10.2, §2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Env {
    /// The process's own environment.
    Process,
    /// Exactly these variables and no others.
    Vars(BTreeMap<String, String>),
}

impl Env {
    /// An environment with no variables.
    pub fn empty() -> Self {
        Self::Vars(BTreeMap::new())
    }

    /// The variable's value, `None` when it is unset or not Unicode.
    pub fn var(&self, name: &str) -> Option<String> {
        match self {
            Self::Process => std::env::var(name).ok(),
            Self::Vars(vars) => vars.get(name).cloned(),
        }
    }

    /// The variable's value, `None` when it is unset; a value that isn't Unicode reads with
    /// U+FFFD where it isn't (what `dv_backend::wsl::sandbox` refuses for `DV_WSL_HOME`).
    pub fn var_lossy(&self, name: &str) -> Option<String> {
        match self {
            Self::Process => {
                std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
            }
            Self::Vars(vars) => vars.get(name).cloned(),
        }
    }

    /// The variable's value when it is set to something other than whitespace.
    pub fn non_empty(&self, name: &str) -> Option<String> {
        self.var(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }
}

/// Everything a command reads from and writes to besides the repository. The streams are
/// `Send`: `dv backend --stdio` hands its output to the server's writer thread.
pub struct Context {
    /// The command line as the headless commands read it, the program name first (without
    /// the app's own options): what a command run inside a WSL distro passes on (PLAN §10.4).
    /// Empty when it isn't known, and then such a command can't run.
    pub args: Vec<OsString>,
    /// Where relative paths (`--repo`, `--body-file`) resolve, and the repository a command
    /// means when it names none.
    pub cwd: PathBuf,
    pub env: Env,
    pub stdin: Box<dyn Read + Send>,
    pub stdout: Box<dyn Write + Send>,
    pub stderr: Box<dyn Write + Send>,
    /// Whether stdout is a terminal (a person reads it) rather than a pipe or a file.
    pub stdout_is_terminal: bool,
    /// Whether stderr is a terminal.
    pub stderr_is_terminal: bool,
    /// Whether stdin is a terminal, so a command may ask a question there.
    pub stdin_is_terminal: bool,
    /// Whether `stdin`, `stdout` and `stderr` are this process's own. A child process (dv run
    /// inside a WSL distro, PLAN §10.4) shares them when stdout and stderr are both terminals;
    /// otherwise, and when they are captured (tests), its output is copied into them.
    pub process_stdio: bool,
    /// Environment changes for every git the command's backend spawns
    /// (`dv_core::LocalBackendOptions::env`): empty for the real CLI, which honours the user's
    /// own git configuration; a fixture's overlay in tests (PLAN §5.1, §14.1).
    pub git_env: Vec<(OsString, Option<OsString>)>,
}

impl Context {
    /// The process's own command line, working directory, environment and standard streams.
    /// A working directory that can't be read (deleted under the shell) is empty, which a
    /// command that needs it reports.
    pub fn process() -> Self {
        Self {
            args: std::env::args_os().collect(),
            cwd: std::env::current_dir().unwrap_or_default(),
            env: Env::Process,
            stdin: Box::new(io::stdin()),
            stdout: Box::new(io::stdout()),
            stderr: Box::new(io::stderr()),
            stdout_is_terminal: io::stdout().is_terminal(),
            stderr_is_terminal: io::stderr().is_terminal(),
            stdin_is_terminal: io::stdin().is_terminal(),
            process_stdio: true,
            git_env: Vec::new(),
        }
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("env", &self.env)
            .field("stdout_is_terminal", &self.stdout_is_terminal)
            .field("stderr_is_terminal", &self.stderr_is_terminal)
            .field("stdin_is_terminal", &self.stdin_is_terminal)
            .field("process_stdio", &self.process_stdio)
            .field("git_env", &self.git_env)
            .finish_non_exhaustive()
    }
}
