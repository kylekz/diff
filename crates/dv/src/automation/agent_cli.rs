//! The agent's side of a scenario through the real CLI (PLAN §14.2): this same `dv` binary,
//! started as a child process with a headless command, as an agent's shell starts it.
//!
//! - `cli {args, cwd?, stdin?, env?, timeout_ms?}` runs `dv <args>` to its end and replies
//!   `{exit, stdout, stderr, json}`: the exit code (`null` when the child was killed by a
//!   signal), its output as text, and stdout parsed as JSON when it parses (a `--json`
//!   document), else `null`. A child still running after `timeout_ms` (default 20 000) is
//!   killed and the command fails.
//! - `cli_spawn {name, args, cwd?, stdin?, env?}` starts one and replies `{name, pid}` at once:
//!   a command that blocks (`dv review wait`) while the scenario plays the reviewer.
//! - `cli_join {name, timeout_ms?}` waits for that child (default 20 000 ms) and replies like
//!   `cli`; one still running then fails the command and keeps running, so a later `cli_join`
//!   can wait again.
//!
//! `args` must name a headless command (or be `--help`, `--version`, or a line clap refuses,
//! which the child reports itself): `dv` alone and `dv open` would open a window on the owner's
//! desktop, so they are refused. `cwd` defaults to the app's working directory (the data dir
//! under the e2e runner); a relative one resolves against it. The child's environment is the
//! app's own (under the runner its hermetic one, `DV_GH` naming `mock_gh`), then, in order:
//! `HOME` and `USERPROFILE` set to `<data dir>/home` and `CODEX_HOME` to its `.codex` (so `dv
//! setup agents` never writes the owner's real home), the fixture overlay of the seeded repo `cwd` is in, or of the
//! seeded repo whose linked worktree it is (its git configuration, never the machine's, and its
//! own `HOME`),
//! `DV_DATA_DIR` set to the app's data dir, `DV_USER_PATH_FILE` to the app's stand-in for the
//! user `Path` (`<data dir>/user-path.txt`, so `dv setup path` never writes the registry),
//! `CLAUDECODE`, `DV_AUTHOR` and `DV_REVIEW` removed
//! (Claude Code exports `CLAUDECODE` to what it runs, and the CLI takes it for the agent's
//! identity, PLAN §2.5), `DV_LINUX_DIR` naming a folder without a Linux build unless the app
//! itself runs sandboxed (`DV_WSL_HOME` naming a scratch home, as `dv_backend::wsl::sandbox`
//! reads it), so a command for a WSL repository stops at exit 4
//! rather than run inside a distro's real home (PLAN §10.4, CLAUDE.md), `DV_ONLY_DISTROS`
//! naming the only distro its setup checks reach (the sandbox's `DV_TEST_WSL_DISTRO`, else
//! none), then `env`
//! (`{"NAME": "value"}` sets, `{"NAME": null}` removes; the runner's own variables,
//! `DV_LINUX_DIR`, `DV_WSL_HOME`, `DV_ONLY_DISTROS` and `DV_USER_PATH_FILE` can't be changed). `stdin` is written to the child's stdin,
//! which is then closed; without it stdin is empty.
//!
//! Children start and are waited for off the UI thread, never count against `settled`, are
//! listed in `state.cli` until joined, and any still running when the app quits (a `quit` or a
//! `restart`) is killed. On Windows they start without a console window.

use std::ffi::OsString;
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::Parser as _;
use dv_ui::ActiveServices as _;
use gpui::{AppContext as _, BackgroundExecutor, Context, Task};
use serde_json::{Map, Value, json};

use super::commands::{object, string};
use super::{DvAutomation, Seeded};
use crate::cli::{Cli, Command as CliCommand};

/// An environment change: `Some` sets the variable, `None` removes it.
pub type EnvOverlay = Vec<(OsString, Option<OsString>)>;

/// The variables the e2e runner sets for every launch (tests/e2e/README.md, "Runner"); `env`
/// can't change them, so a child stays as hermetic as the app.
const RUNNER_VARS: &[&str] = &["DV_DATA_DIR", "DV_GH", "DV_MOCK_GH", "DV_MOCK_GH_LOG"];

/// The variables that decide whether a child's CLI reaches into a WSL distro, and whose home
/// it uses there (PLAN §10.4): `env` can't change them, so a scenario's commands reach WSL
/// only when the app itself runs sandboxed (module docs).
const WSL_VARS: &[&str] = &["DV_LINUX_DIR", "DV_WSL_HOME", ONLY_DISTROS_VAR];

/// The variable naming the only distros a child's setup checks reach (`dv_cli::checks`).
const ONLY_DISTROS_VAR: &str = dv_cli::checks::ONLY_DISTROS_VAR;

/// The variables that make the CLI an agent or pick its review (PLAN §2.5, §10.2): never
/// inherited, set through `env` when a scenario means them.
const AGENT_VARS: &[&str] = &["CLAUDECODE", "DV_AUTHOR", "DV_REVIEW"];

/// Where a child finds its home directory: `<data dir>/home` for every child (the app's own
/// setup checks' home too, `crate::app::AUTOMATION_HOME`), so nothing a scenario runs (`dv
/// setup agents`) ever writes the owner's real one (CLAUDE.md). Codex's (`CODEX_HOME`) is its
/// `.codex`, as Codex's own default is.
const HOME_VARS: &[&str] = &["HOME", "USERPROFILE"];

/// The variable naming the file that stands in for the user `Path` (`dv setup path`, PLAN
/// §12.2): the app's own stand-in in its data dir for every child, never the registry.
const USER_PATH_VAR: &str = dv_cli::checks::user_path::USER_PATH_FILE_VAR;

/// How long `cli` and `cli_join` wait when the line doesn't say: under the runner's own 30 s
/// reply timeout for a line without `timeout_ms`.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// The longest wait `gpui_automation` allows any command (its `MAX_WAIT`).
const MAX_TIMEOUT: Duration = Duration::from_secs(120);

/// How often a wait looks at the child.
const POLL: Duration = Duration::from_millis(20);

/// How long the output pipes may stay open after the child exits: a process it started may
/// still hold them (the CLI's own children are git calls that end with it).
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// A child to start: what `cli` and `cli_spawn` take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// `dv`'s arguments, without the program name.
    pub args: Vec<String>,
    /// Absolute.
    pub cwd: PathBuf,
    pub stdin: Option<String>,
    /// `env`, in the order given: `Some` sets, `None` removes.
    pub env: Vec<(String, Option<String>)>,
}

impl Request {
    /// The request in `args` (already checked for unknown keys), its `cwd` resolved against
    /// `base`, the app's working directory.
    pub fn parse(cmd: &str, args: &Map<String, Value>, base: &Path) -> Result<Self> {
        let list = args
            .get("args")
            .and_then(Value::as_array)
            .with_context(|| format!("{cmd} needs \"args\", a list of strings"))?;
        let argv: Vec<String> = list
            .iter()
            .map(|arg| arg.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .with_context(|| {
                format!(
                    "{cmd}'s \"args\" are strings: {}",
                    Value::Array(list.clone())
                )
            })?;
        check_headless(&argv)?;
        let cwd = match args.get("cwd") {
            None | Some(Value::Null) => base.to_path_buf(),
            Some(Value::String(cwd)) => base.join(cwd),
            Some(other) => bail!("{cmd}'s \"cwd\" is a path, not {other}"),
        };
        let stdin = match args.get("stdin") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(text.clone()),
            Some(other) => bail!("{cmd}'s \"stdin\" is a string, not {other}"),
        };
        let env = match args.get("env") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Object(vars)) => vars
                .iter()
                .map(|(name, value)| env_change(cmd, name, value))
                .collect::<Result<_>>()?,
            Some(other) => bail!("{cmd}'s \"env\" is an object, not {other}"),
        };
        Ok(Self {
            args: argv,
            cwd,
            stdin,
            env,
        })
    }

    /// `dv <args>`, for messages.
    pub fn line(&self) -> String {
        command_line(&self.args)
    }
}

/// One entry of `env`: a string sets the variable, `null` removes it. The runner's own
/// variables stay as it set them.
fn env_change(cmd: &str, name: &str, value: &Value) -> Result<(String, Option<String>)> {
    if RUNNER_VARS.contains(&name) {
        bail!("{cmd} can't change {name}: the e2e runner sets it for every launch");
    }
    if WSL_VARS.contains(&name) {
        bail!(
            "{cmd} can't change {name}: a command reaches WSL only as the app does (a \
             scenario header's DV_WSL_HOME)"
        );
    }
    if name == USER_PATH_VAR {
        bail!("{cmd} can't change {name}: the user Path is the app's stand-in for every child");
    }
    if name.is_empty() || name.contains('=') {
        bail!("{name:?} isn't an environment variable name");
    }
    match value {
        Value::String(value) => Ok((name.to_string(), Some(value.clone()))),
        Value::Null => Ok((name.to_string(), None)),
        other => bail!("{cmd}'s \"env\" values are strings or null, not {other}"),
    }
}

/// `dv <args>`, for messages and `state.cli`.
fn command_line(args: &[String]) -> String {
    std::iter::once("dv")
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Refuses an argument list that would start the app: `dv` without a command and `dv open`
/// open a window (PLAN §10.1). Anything clap refuses (and `--help`, `--version`) is the
/// child's to report, without a window.
pub fn check_headless(args: &[String]) -> Result<()> {
    let line = std::iter::once("dv").chain(args.iter().map(String::as_str));
    match Cli::try_parse_from(line) {
        Ok(cli) => match cli.command {
            Some(CliCommand::Headless(_)) => Ok(()),
            Some(CliCommand::Open(_)) => bail!(
                "cli runs headless commands only: `{}` would open a window",
                command_line(args)
            ),
            None => bail!(
                "cli runs headless commands only: `{}` names no command and would open the app",
                command_line(args)
            ),
        },
        Err(_) => Ok(()),
    }
}

/// A `timeout_ms` argument, `default` when absent, at most [`MAX_TIMEOUT`].
pub fn timeout(cmd: &str, args: &Map<String, Value>, default: Duration) -> Result<Duration> {
    let timeout = match args.get("timeout_ms") {
        None | Some(Value::Null) => return Ok(default),
        Some(value) => value
            .as_u64()
            .map(Duration::from_millis)
            .with_context(|| format!("{cmd}'s \"timeout_ms\" is a whole number, not {value}"))?,
    };
    if timeout > MAX_TIMEOUT {
        bail!(
            "{cmd}'s \"timeout_ms\" is at most {} (the automation's cap on any wait)",
            MAX_TIMEOUT.as_millis()
        );
    }
    Ok(timeout)
}

/// A `cli_spawn` name: plain, so it reads well in `state.cli` and in messages.
fn check_name(name: &str) -> Result<()> {
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !plain {
        bail!("a cli_spawn name is letters, digits, '-', '_' and '.', not {name:?}");
    }
    Ok(())
}

/// The overlay of the deepest root in `roots` that holds `cwd`, comparing canonical paths (so
/// `D:\data\repos\api` and `d:/data/repos/api/src` match).
pub fn overlay_for(cwd: &Path, roots: Vec<(PathBuf, EnvOverlay)>) -> Option<EnvOverlay> {
    let cwd = std::fs::canonicalize(cwd).ok()?;
    roots
        .into_iter()
        .filter_map(|(root, env)| {
            let root = std::fs::canonicalize(root).ok()?;
            cwd.starts_with(&root).then_some((root, env))
        })
        .max_by_key(|(root, _)| root.components().count())
        .map(|(_, env)| env)
}

/// The changes a child's environment gets over the app's, in order (module docs); `sandboxed`
/// when the app runs with a `DV_WSL_HOME` of its own.
pub fn child_env(
    overlay: Option<EnvOverlay>,
    data_dir: &Path,
    env: &[(String, Option<String>)],
    sandboxed: bool,
) -> EnvOverlay {
    let home = data_dir.join(crate::app::AUTOMATION_HOME);
    let mut changes: EnvOverlay = HOME_VARS
        .iter()
        .map(|name| (OsString::from(name), Some(home.as_os_str().to_owned())))
        .collect();
    changes.push((
        "CODEX_HOME".into(),
        Some(home.join(".codex").into_os_string()),
    ));
    changes.extend(overlay.unwrap_or_default());
    changes.push(("DV_DATA_DIR".into(), Some(data_dir.as_os_str().to_owned())));
    changes.push((
        USER_PATH_VAR.into(),
        Some(
            data_dir
                .join(crate::app::AUTOMATION_USER_PATH)
                .into_os_string(),
        ),
    ));
    changes.extend(AGENT_VARS.iter().map(|name| (OsString::from(name), None)));
    if !sandboxed {
        let nowhere = data_dir.join("no-linux-build");
        changes.push(("DV_LINUX_DIR".into(), Some(nowhere.into_os_string())));
    }
    // The setup checks (`dv doctor`, `dv setup`) reach no distro but the sandbox's.
    let only = if sandboxed {
        dv_ui::services::setup::sandbox_distros().join(",")
    } else {
        String::new()
    };
    changes.push((ONLY_DISTROS_VAR.into(), Some(only.into())));
    changes.extend(
        env.iter()
            .map(|(name, value)| (name.into(), value.as_ref().map(OsString::from))),
    );
    changes
}

/// What a child printed and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The exit code; `None` when a signal ended it.
    pub exit: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// The reply of `cli` and `cli_join`: `{exit, stdout, stderr, json}`.
    pub fn to_json(&self) -> Value {
        let parsed = serde_json::from_str::<Value>(self.stdout.trim()).ok();
        json!({
            "exit": self.exit,
            "stdout": self.stdout,
            "stderr": self.stderr,
            "json": parsed,
        })
    }
}

/// A child process and the threads reading its output. Dropping it kills the child if it is
/// still running.
pub struct Running {
    child: Child,
    args: Vec<String>,
    stdout: Receiver<Vec<u8>>,
    stderr: Receiver<Vec<u8>>,
    status: Option<ExitStatus>,
}

impl Running {
    /// Starts `exe` with the request's arguments, working directory and stdin, and `env` laid
    /// over this process's environment.
    pub fn start(exe: &Path, request: &Request, env: &EnvOverlay) -> Result<Self> {
        // `dv.exe` is a console program (PLAN §13.1): `toolbox_process` creates it with no
        // console window, which an app that has no console of its own would give each child.
        let mut command = env
            .iter()
            .fold(
                toolbox_process::Command::new(exe)
                    .args(&request.args)
                    .current_dir(&request.cwd),
                |command, (name, value)| match value {
                    Some(value) => command.env(name, value),
                    None => command.env_remove(name),
                },
            )
            .to_std();
        command
            .stdin(if request.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().with_context(|| {
            format!(
                "couldn't start `{}` ({}) in {}",
                request.line(),
                exe.display(),
                request.cwd.display()
            )
        })?;
        if let (Some(text), Some(mut pipe)) = (request.stdin.clone(), child.stdin.take()) {
            // A child may exit without reading all of its stdin; its exit code says so.
            std::thread::spawn(move || {
                let _ = pipe.write_all(text.as_bytes());
            });
        }
        let stdout = read_in_background(child.stdout.take());
        let stderr = read_in_background(child.stderr.take());
        Ok(Self {
            child,
            args: request.args.clone(),
            stdout,
            stderr,
            status: None,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Its exit status once it has exited, `None` while it runs.
    pub fn exited(&mut self) -> Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self
                .child
                .try_wait()
                .with_context(|| format!("waiting for `{}`", command_line(&self.args)))?;
        }
        Ok(self.status)
    }

    /// Kills it if it still runs, and waits for it to go.
    pub fn kill(&mut self) {
        if self.status.is_some() {
            return;
        }
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
        }
        self.status = self.child.wait().ok();
    }

    /// What it printed, once it has exited with `status`. A pipe that some process it started
    /// still holds open after [`DRAIN_GRACE`] reads as what arrived so far: nothing.
    pub fn output(&self, status: ExitStatus) -> Output {
        let deadline = Instant::now() + DRAIN_GRACE;
        let drain = |rx: &Receiver<Vec<u8>>| match rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout) => String::new(),
        };
        let stdout = drain(&self.stdout);
        let stderr = drain(&self.stderr);
        Output {
            exit: status.code(),
            stdout,
            stderr,
        }
    }
}

impl std::fmt::Debug for Running {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Running")
            .field("line", &command_line(&self.args))
            .field("pid", &self.pid())
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Reads a pipe to its end on a new thread; the receiver yields the bytes.
fn read_in_background(pipe: Option<impl Read + Send + 'static>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = tx.send(bytes);
    });
    rx
}

/// Which child: one `cli_spawn` named, or a `cli` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Spawned(String),
    Run(u64),
}

/// The children the automation host started and hasn't collected yet, oldest first.
#[derive(Default)]
pub struct Children {
    entries: Vec<(Key, Running)>,
    runs: u64,
}

impl Children {
    /// A key for a new `cli` run.
    pub fn next_run(&mut self) -> Key {
        self.runs += 1;
        Key::Run(self.runs)
    }

    pub fn contains(&self, key: &Key) -> bool {
        self.entries.iter().any(|(known, _)| known == key)
    }

    /// Keeps `running` under `key`; a key already in use is an error (and `running`, dropped,
    /// is killed).
    pub fn insert(&mut self, key: Key, running: Running) -> Result<()> {
        if self.contains(&key) {
            bail!("{} is already running", describe(&key));
        }
        self.entries.push((key, running));
        Ok(())
    }

    /// Takes `key`'s child out once it has exited, with its status; `None` while it runs.
    pub fn take_exited(&mut self, key: &Key) -> Result<Option<(Running, ExitStatus)>> {
        let Some(index) = self.entries.iter().position(|(known, _)| known == key) else {
            return Err(self.missing(key));
        };
        let Some(status) = self.entries[index].1.exited()? else {
            return Ok(None);
        };
        let (_, running) = self.entries.remove(index);
        Ok(Some((running, status)))
    }

    /// Takes `key`'s child out, whether or not it has exited.
    pub fn remove(&mut self, key: &Key) -> Option<Running> {
        let index = self.entries.iter().position(|(known, _)| known == key)?;
        Some(self.entries.remove(index).1)
    }

    /// Why `key` isn't here: an unknown (or already joined) name, with the names there are; a
    /// run killed as the app quit.
    fn missing(&self, key: &Key) -> anyhow::Error {
        match key {
            Key::Spawned(name) => {
                let names: Vec<&str> = self
                    .entries
                    .iter()
                    .filter_map(|(key, _)| match key {
                        Key::Spawned(name) => Some(name.as_str()),
                        Key::Run(_) => None,
                    })
                    .collect();
                anyhow!(
                    "no cli_spawn named {name:?} is waiting to be joined (unjoined: {})",
                    if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(", ")
                    }
                )
            }
            Key::Run(_) => anyhow!("the child was killed as the app quit"),
        }
    }

    /// `state.cli.spawned`: the `cli_spawn` children not yet joined, oldest first, each
    /// `{name, args, pid, running}`.
    pub fn spawned_state(&mut self) -> Vec<Value> {
        self.entries
            .iter_mut()
            .filter_map(|(key, running)| match key {
                Key::Spawned(name) => Some(json!({
                    "name": name,
                    "args": running.args,
                    "pid": running.pid(),
                    "running": matches!(running.exited(), Ok(None)),
                })),
                Key::Run(_) => None,
            })
            .collect()
    }

    /// Kills every child still running and forgets them all; returns how many there were.
    pub fn kill_all(&mut self) -> usize {
        let count = self.entries.len();
        for (key, running) in &mut self.entries {
            if matches!(running.exited(), Ok(None)) {
                log::info!(
                    "killing {} (`{}`, pid {}): still running as the app quits",
                    describe(key),
                    command_line(&running.args),
                    running.pid()
                );
            }
            running.kill();
        }
        self.entries.clear();
        count
    }
}

impl Drop for Children {
    fn drop(&mut self) {
        self.kill_all();
    }
}

fn describe(key: &Key) -> String {
    match key {
        Key::Spawned(name) => format!("a cli_spawn named {name:?}"),
        Key::Run(n) => format!("cli run {n}"),
    }
}

/// The registry, whatever a panicking holder left it as (each entry stays consistent).
pub fn lock(children: &Mutex<Children>) -> MutexGuard<'_, Children> {
    children.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Waits until `key`'s child exits (its output) or `deadline` passes (`None`), polling on the
/// background executor's timer.
async fn wait_for(
    children: &Mutex<Children>,
    key: &Key,
    deadline: Instant,
    executor: &BackgroundExecutor,
) -> Result<Option<Output>> {
    loop {
        let exited = lock(children).take_exited(key)?;
        if let Some((running, status)) = exited {
            return Ok(Some(running.output(status)));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        executor.timer(POLL).await;
    }
}

/// Everything starting a child needs, gathered on the UI thread so the start itself (the
/// seeded repos' locks, canonical paths, the spawn) runs off it.
struct Start {
    exe: PathBuf,
    request: Request,
    data_dir: PathBuf,
    seeded: Vec<Seeded>,
}

impl Start {
    fn start(&self) -> Result<Running> {
        // Each seeded repo's work tree and linked worktrees, with its environment.
        let roots = self
            .seeded
            .iter()
            .filter_map(|fixture| {
                let fixture = fixture.lock().ok()?;
                let env = fixture.env_overlay();
                Some(
                    super::repos::roots(&fixture)
                        .into_iter()
                        .map(move |root| (root, env.clone())),
                )
            })
            .flatten()
            .collect();
        let overlay = overlay_for(&self.request.cwd, roots);
        // `DV_WSL_HOME` means what dv_backend's one check says it does: blank or not a POSIX
        // path is no sandbox, never the distro user's own home.
        let sandboxed = dv_backend::wsl::is_sandbox(dv_backend::wsl::home_var());
        let env = child_env(overlay, &self.data_dir, &self.request.env, sandboxed);
        Running::start(&self.exe, &self.request, &env)
    }
}

impl DvAutomation {
    /// What starting `request` needs (see [`Start`]).
    fn start_for(&self, request: Request, cx: &Context<Self>) -> Result<Start> {
        let exe = self
            .cli_exe
            .clone()
            .context("cli needs dv's own executable, which this process couldn't name")?;
        Ok(Start {
            exe,
            request,
            data_dir: cx.services().data_dir().to_path_buf(),
            seeded: self
                .seeded
                .values()
                .map(|(fixture, _)| fixture.clone())
                .collect(),
        })
    }

    /// `cli {args, cwd?, stdin?, env?, timeout_ms?}` (module docs).
    pub(super) fn cli(
        &mut self,
        args: Value,
        cx: &mut Context<Self>,
    ) -> Result<Task<Result<Value>>> {
        let args = object("cli", args, &["args", "cwd", "stdin", "env", "timeout_ms"])?;
        let request = Request::parse("cli", &args, &working_dir()?)?;
        let timeout = timeout("cli", &args, DEFAULT_TIMEOUT)?;
        let start = self.start_for(request, cx)?;
        let children = self.children.clone();
        let executor = cx.background_executor().clone();
        Ok(cx.background_spawn(async move {
            let running = start.start()?;
            let key = {
                let mut children = lock(&children);
                let key = children.next_run();
                children.insert(key.clone(), running)?;
                key
            };
            let deadline = Instant::now() + timeout;
            match wait_for(&children, &key, deadline, &executor).await? {
                Some(output) => Ok(output.to_json()),
                None => {
                    // Dropping it kills it, once the registry's lock is released.
                    let killed = lock(&children).remove(&key);
                    drop(killed);
                    Err(anyhow!(
                        "`{}` didn't finish within {} ms, so it was killed",
                        start.request.line(),
                        timeout.as_millis()
                    ))
                }
            }
        }))
    }

    /// `cli_spawn {name, args, cwd?, stdin?, env?}` (module docs).
    pub(super) fn cli_spawn(
        &mut self,
        args: Value,
        cx: &mut Context<Self>,
    ) -> Result<Task<Result<Value>>> {
        let args = object("cli_spawn", args, &["name", "args", "cwd", "stdin", "env"])?;
        let name = string("cli_spawn", &args, "name")?.to_string();
        check_name(&name)?;
        let request = Request::parse("cli_spawn", &args, &working_dir()?)?;
        let key = Key::Spawned(name.clone());
        if lock(&self.children).contains(&key) {
            bail!("a cli_spawn named {name:?} hasn't been joined yet (cli_join it first)");
        }
        let start = self.start_for(request, cx)?;
        let children = self.children.clone();
        Ok(cx.background_spawn(async move {
            let running = start.start()?;
            let pid = running.pid();
            lock(&children).insert(key, running)?;
            Ok(json!({"name": name, "pid": pid}))
        }))
    }

    /// `cli_join {name, timeout_ms?}` (module docs).
    pub(super) fn cli_join(
        &mut self,
        args: Value,
        cx: &mut Context<Self>,
    ) -> Result<Task<Result<Value>>> {
        let args = object("cli_join", args, &["name", "timeout_ms"])?;
        let name = string("cli_join", &args, "name")?.to_string();
        let timeout = timeout("cli_join", &args, DEFAULT_TIMEOUT)?;
        let key = Key::Spawned(name.clone());
        {
            // An unknown name fails at once, saying which there are.
            let children = lock(&self.children);
            if !children.contains(&key) {
                return Err(children.missing(&key));
            }
        }
        let children = self.children.clone();
        let executor = cx.background_executor().clone();
        Ok(cx.background_spawn(async move {
            let deadline = Instant::now() + timeout;
            match wait_for(&children, &key, deadline, &executor).await? {
                Some(output) => Ok(output.to_json()),
                None => Err(anyhow!(
                    "{name:?} is still running after {} ms; it keeps running until a cli_join \
                     sees it end or the app quits",
                    timeout.as_millis()
                )),
            }
        }))
    }

    /// `state.cli`: `{spawned: [{name, args, pid, running}]}`, the `cli_spawn` children not
    /// yet joined.
    pub(super) fn cli_state(&self) -> Value {
        json!({"spawned": lock(&self.children).spawned_state()})
    }
}

/// The app's working directory, which a relative `cwd` resolves against.
fn working_dir() -> Result<PathBuf> {
    std::env::current_dir().context("the app's working directory can't be read")
}

#[cfg(test)]
mod tests;
