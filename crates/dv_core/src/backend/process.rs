//! Processes a backend runs where its repository lives (docs/PLAN.md §18.3): a language server
//! for a WSL repository runs inside its distro, reading the distro's files natively.
//!
//! [`super::Backend::spawn_stdio`] starts a [`ProcessSpec`] — an executable by its absolute
//! path, its arguments, a working directory inside the worktree, environment changes — with
//! piped stdio, never through a shell. What it prints reaches a [`ProcessSink`] as
//! [`ProcessEvent`]s on a thread of the process's own, in order: stdout in chunks as they come,
//! stderr a line at a time, then its exit, once. The [`ProcessHandle`] writes its stdin and
//! kills it; dropping the handle kills it too.
//!
//! A process takes everything it starts with it: it runs in a process group of its own on Unix
//! (a job of its own on Windows), which is killed when the process is killed, when its handle
//! is dropped, and when the process itself exits, so a language server's children never
//! outlive it. The job also dies with dv however dv ends. `toolbox_process` does all of this
//! (`local`).

mod local;

use std::fmt;

use serde::{Deserialize, Serialize};

use super::BackendError;

pub(super) use local::spawn;

/// Starts `spec` on this machine as [`super::Backend::spawn_stdio`] does, its working
/// directory relative to `dir` rather than to a worktree: for what runs where dv runs with no
/// repository open, such as the user's shell asked for its `PATH` by `dv doctor` (PLAN §12.2).
pub fn spawn_in(
    dir: &std::path::Path,
    spec: &ProcessSpec,
    sink: ProcessSink,
) -> Result<ProcessHandle, BackendError> {
    local::spawn(dir, spec, sink)
}

/// The longest stderr line a sink is given, in bytes: a longer one is cut at a character
/// boundary and ends in `…`. Stderr is diagnostics, kept a few lines at a time.
pub const STDERR_LINE_MAX: usize = 8 * 1024;

/// What [`super::Backend::spawn_stdio`] runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSpec {
    /// The executable: an absolute path on the backend's machine (inside the distro for a WSL
    /// repository), which a resolver found (PLAN §18.4). It runs as itself, never through a
    /// shell, so on Windows a batch file (`.bat`, `.cmd`), which only `cmd.exe` runs, is
    /// refused.
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// The working directory, relative to the worktree root and validated like every worktree
    /// path; empty is the root itself.
    #[serde(default)]
    pub cwd: String,
    /// Changes to the backend's own environment: `(name, Some(value))` sets a variable,
    /// `(name, None)` removes it.
    #[serde(default)]
    pub env: Vec<(String, Option<String>)>,
}

impl ProcessSpec {
    /// `program` with no arguments, in the worktree root, in the backend's environment.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, rel: impl Into<String>) -> Self {
        self.cwd = rel.into();
        self
    }

    pub fn env(mut self, name: impl Into<String>, value: Option<&str>) -> Self {
        self.env.push((name.into(), value.map(str::to_string)));
        self
    }
}

/// What a running process did, as its [`ProcessSink`] hears it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessEvent {
    /// The next bytes of its stdout, as the pipe gave them: a message can span chunks.
    Stdout(Vec<u8>),
    /// One line of its stderr, without its line ending, decoded leniently and at most
    /// [`STDERR_LINE_MAX`] bytes.
    Stderr(String),
    /// It ended, with its exit code; `None` when a signal killed it (a kill on Unix), or when
    /// the connection to the server running it was lost (PLAN §5.2). A kill on Windows ends its
    /// job with exit code 1, so it reads `Some(1)` there. Always the last event, after
    /// everything it printed (unless something it started still holds its output open a while
    /// after).
    Exit(Option<i32>),
}

/// Receives a process's [`ProcessEvent`]s on a thread of the process's own. It should return
/// quickly (hand the event to a channel); the next event waits for it.
pub type ProcessSink = Box<dyn FnMut(ProcessEvent) + Send + 'static>;

/// What a [`ProcessHandle`] does with its process, one implementation per backend. Its drop
/// stops the delivery to the sink, as [`ProcessHandle`] says.
pub trait ProcessControl: Send + Sync {
    /// Writes `bytes` to the process's stdin, returning once they are written (into the pipe,
    /// or on the server for a remote process).
    fn write(&self, bytes: &[u8]) -> Result<(), BackendError>;

    /// Kills the process and everything it started; its [`ProcessEvent::Exit`] follows. Does
    /// nothing once it has ended.
    fn kill(&self);
}

/// A running process (module docs). Dropping it kills the process and stops the delivery:
/// once `drop` returns, the sink is never called again and has itself been dropped (except
/// when the handle is dropped from inside the sink, where delivery stops after the sink
/// returns).
pub struct ProcessHandle {
    control: Box<dyn ProcessControl>,
    pid: u32,
}

impl ProcessHandle {
    pub fn new(pid: u32, control: impl ProcessControl + 'static) -> Self {
        Self {
            control: Box::new(control),
            pid,
        }
    }

    /// The process's id where it runs: inside the distro for a WSL repository.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Writes `bytes` to the process's stdin ([`ProcessControl::write`]). Writes made from one
    /// thread arrive in order. Fails once the process has ended.
    pub fn write(&self, bytes: &[u8]) -> Result<(), BackendError> {
        self.control.write(bytes)
    }

    /// Kills the process and everything it started; the sink hears its exit.
    pub fn kill(&self) {
        self.control.kill();
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        self.control.kill();
    }
}

impl fmt::Debug for ProcessHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessHandle")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// The error a write to a process that has ended fails with.
pub fn ended(program: &str) -> BackendError {
    BackendError::Io {
        op: "writing to".into(),
        path: program.to_string(),
        source: std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the process has ended"),
    }
}

/// `text` cut to at most [`STDERR_LINE_MAX`] bytes at a character boundary, `…` marking a cut.
pub fn bounded_line(text: String) -> String {
    if text.len() <= STDERR_LINE_MAX {
        return text;
    }
    let mut end = STDERR_LINE_MAX - '…'.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
