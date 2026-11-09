//! Running the small programs the checks ask (git, `gh`) through `toolbox_process`: to their
//! end or a timeout, stdin empty, output collected, no console window (the app runs detached,
//! and a console child of it would flash one), in a tree of their own that dies with dv.

use std::ffi::OsString;
use std::time::Duration;

use toolbox_process::{CancelToken, Command, ProcessError, Stop};

/// What a program printed and how it ended.
#[derive(Debug)]
pub(crate) struct Ran {
    /// `None` when a signal ended it.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs `command` to its end, killing it and everything it started after `timeout`
/// ([`ProcessError::TimedOut`]). A program that isn't there is a [`ProcessError::Start`] whose
/// source is [`std::io::ErrorKind::NotFound`].
pub(crate) fn run(command: Command, timeout: Duration) -> Result<Ran, ProcessError> {
    let output = toolbox_process::run(&command.stop(Stop::Kill), timeout, &CancelToken::new())?;
    Ok(Ran {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// `command` with `env` applied (`Some` sets, `None` removes).
pub(crate) fn apply_env(command: Command, env: &[(OsString, Option<OsString>)]) -> Command {
    env.iter()
        .fold(command, |command, (name, value)| match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        })
}

/// Whether `error` says the program isn't there.
pub(crate) fn not_found(error: &ProcessError) -> bool {
    matches!(error, ProcessError::Start { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
}
