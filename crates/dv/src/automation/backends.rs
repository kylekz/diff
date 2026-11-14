//! Scenario control over the app's connections to `dv backend --stdio` (PLAN §5.2, §14.2;
//! tests/e2e/README.md):
//!
//! - **The loopback.** A repo seeded with `"backend": "loopback"` opens through the connection
//!   named [`LOOPBACK`]: this executable's `backend --stdio` as a child process, as a WSL repo
//!   opens through its distro's server, so a scenario plays every state a distro's connection
//!   goes through on any machine.
//! - `backend_kill {name}` kills a connection's server as a crash would
//!   (`Connection::kill`): calls in flight fail with `Disconnected` and it reconnects.
//! - `backend_block {name, blocked}` makes the loopback's starts fail while `blocked`, so the
//!   respawns after a kill run out and the connection is `failed`; unblocked, Retry connects.
//! - `backend_hold {name, held}` makes the loopback's starts wait while `held` (up to a minute),
//!   as a distro takes seconds to boot, so a scenario sees what a start in progress looks like
//!   (the switcher's Start row, inert); released, the start goes on.
//! - `backend_stop {name}` stops the loopback's host as `wsl --shutdown` stops a distro: its
//!   server dies, and the host reads as stopped to the connection's host check
//!   (`Launcher::with_host_check`), so the respawn starts nothing and the connection is
//!   `stopped`; a server started anyway (an explicit start: the banner's Start, an open)
//!   boots it again. A distro's real connection has this check; this one stands in for it.
//! - `wsl_distros {distros: [{name, running}]}` tells the stand-in for WSL
//!   (`WslAccess::Scripted`, an automation run without `DV_WSL_HOME`) which distros there are
//!   and which run (its connections' host check reads them).

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use dv_backend::wsl::Distro;
use dv_backend::{Connection, ConnectionKey, Launcher};
use dv_ui::ActiveServices as _;
use gpui::{App, AppContext as _, Context, Task};
use serde_json::{Value, json};

use super::DvAutomation;
use super::commands::{object, string};

/// The loopback connection's name, as `state.backends` and the status bar say it.
pub const LOOPBACK: &str = "loopback";

/// A loopback's switches: whether its starts fail (`backend_block`) or wait (`backend_hold`),
/// and whether its host is stopped (`backend_stop`).
#[derive(Clone, Default)]
pub struct Switches {
    blocked: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

/// How long a held start waits at most for `backend_hold` to let it go.
const HOLD_LIMIT: Duration = Duration::from_secs(60);

/// How the loopback's server starts and when it respawns: this executable's `backend --stdio`
/// after `dv_backend::client::BACKOFF`, in the app; a server on a thread and short waits in
/// the host's tests, which can't name the dv executable.
pub struct Loopback {
    /// `None`: this executable's `backend --stdio`.
    pub server: Option<Launcher>,
    pub backoff: Vec<Duration>,
}

impl Default for Loopback {
    fn default() -> Self {
        Self {
            server: None,
            backoff: dv_backend::client::BACKOFF.to_vec(),
        }
    }
}

impl DvAutomation {
    /// The loopback connection, made the first time with its switches (`backend_block`,
    /// `backend_stop`). Nothing starts until a seeded repo opens through it.
    pub(super) fn loopback(&mut self, cx: &mut App) -> Arc<Connection> {
        let switches = self
            .switches
            .entry(LOOPBACK.to_string())
            .or_default()
            .clone();
        let server = self.loopback.server.clone().unwrap_or_else(|| {
            let program = self.cli_exe.clone().unwrap_or_else(|| PathBuf::from("dv"));
            server(program)
        });
        let backoff = self.loopback.backoff.clone();
        let connections = cx.services().locations.read(cx).connections().clone();
        connections.get_or_insert_with(ConnectionKey::loopback(LOOPBACK), || {
            Connection::with_backoff(LOOPBACK, switched(server, switches), backoff)
        })
    }

    /// The switches of the loopback named `name`, for `cmd`.
    fn switches(&self, cmd: &str, name: &str) -> Result<&Switches> {
        self.switches.get(name).with_context(|| {
            format!(
                "{cmd}: no loopback connection named {name:?}: seed a repo with \"backend\": \
                 \"loopback\""
            )
        })
    }

    /// `backend_block {name, blocked}`.
    pub(super) fn backend_block(&mut self, args: Value) -> Result<Value> {
        let args = object("backend_block", args, &["name", "blocked"])?;
        let name = string("backend_block", &args, "name")?;
        let blocked = args
            .get("blocked")
            .and_then(Value::as_bool)
            .context("backend_block needs a boolean \"blocked\"")?;
        self.switches("backend_block", name)?
            .blocked
            .store(blocked, Ordering::SeqCst);
        Ok(json!({"name": name, "blocked": blocked}))
    }

    /// `backend_hold {name, held}`.
    pub(super) fn backend_hold(&mut self, args: Value) -> Result<Value> {
        let args = object("backend_hold", args, &["name", "held"])?;
        let name = string("backend_hold", &args, "name")?;
        let held = args
            .get("held")
            .and_then(Value::as_bool)
            .context("backend_hold needs a boolean \"held\"")?;
        self.switches("backend_hold", name)?
            .held
            .store(held, Ordering::SeqCst);
        Ok(json!({"name": name, "held": held}))
    }

    /// `backend_stop {name}`: stops the loopback's host, then kills its server; replies
    /// `{name, state}` once the server is killed. Off the UI thread: killing reaps the
    /// process.
    pub(super) fn backend_stop(
        &mut self,
        args: Value,
        cx: &mut Context<DvAutomation>,
    ) -> Result<Task<Result<Value>>> {
        let args = object("backend_stop", args, &["name"])?;
        let name = string("backend_stop", &args, "name")?.to_string();
        let stopped = self.switches("backend_stop", &name)?.stopped.clone();
        let connections = cx.services().locations.read(cx).connections().clone();
        let connection = connections
            .get(&ConnectionKey::loopback(name.clone()))
            .with_context(|| format!("backend_stop: no connection named {name:?}"))?;
        Ok(cx.background_spawn(async move {
            stopped.store(true, Ordering::SeqCst);
            connection.kill();
            Ok(json!({"name": name, "state": super::state::connection_state(&connection.state())}))
        }))
    }
}

/// `program backend --stdio`, the loopback's server in the app. The child gets none of this
/// process's agent identity (`CLAUDECODE`, `DV_AUTHOR`, `DV_MODEL`, `DV_REVIEW`): it serves the app, which
/// acts as the reviewer.
fn server(program: PathBuf) -> Launcher {
    Launcher::Command {
        program,
        args: vec!["backend".into(), "--stdio".into()],
        env: ["CLAUDECODE", "DV_AUTHOR", "DV_MODEL", "DV_REVIEW"]
            .into_iter()
            .map(|name| (name.into(), None))
            .collect(),
        build: None,
    }
}

/// `server` behind `switches`: starts wait while held, then fail while blocked; its host
/// check reads `stopped`, and a start boots the host.
fn switched(server: Launcher, switches: Switches) -> Launcher {
    let label = format!(
        "{} (backend_block stops it starting, backend_hold holds a start, backend_stop stops \
         its host)",
        server.describe()
    );
    let Switches {
        blocked,
        held,
        stopped,
    } = switches;
    Launcher::custom(label, {
        let stopped = Arc::clone(&stopped);
        move || {
            // The start thread waits, as it waits for a distro to boot; never the UI's.
            let until = Instant::now() + HOLD_LIMIT;
            while held.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            if blocked.load(Ordering::SeqCst) {
                return Err(io::Error::other(
                    "the scenario blocks dv backend --stdio from starting (backend_block)",
                ));
            }
            stopped.store(false, Ordering::SeqCst);
            server.start()
        }
    })
    .with_host_check(move |_| !stopped.load(Ordering::SeqCst))
}

/// `backend_kill {name}`: the loopback connection of that name, else the distro's; replies
/// `{name, state}` once its server is killed. Off the UI thread: killing reaps the process.
pub(super) fn backend_kill(
    args: Value,
    cx: &mut Context<DvAutomation>,
) -> Result<Task<Result<Value>>> {
    let args = object("backend_kill", args, &["name"])?;
    let name = string("backend_kill", &args, "name")?.to_string();
    let connections = cx.services().locations.read(cx).connections().clone();
    let connection = connections
        .get(&ConnectionKey::loopback(name.clone()))
        .or_else(|| connections.get(&ConnectionKey::wsl(&name)))
        .ok_or_else(|| {
            let known: Vec<String> = connections
                .all()
                .iter()
                .map(|connection| connection.name().to_string())
                .collect();
            anyhow!("no connection named {name:?} (there are: {known:?})")
        })?;
    Ok(cx.background_spawn(async move {
        connection.kill();
        Ok(json!({"name": name, "state": super::state::connection_state(&connection.state())}))
    }))
}

/// `wsl_distros {distros: [{name, running}]}`.
pub(super) fn wsl_distros(args: Value, cx: &mut Context<DvAutomation>) -> Result<Value> {
    let args = object("wsl_distros", args, &["distros"])?;
    let listed = args
        .get("distros")
        .and_then(Value::as_array)
        .context("wsl_distros needs a list \"distros\" of {\"name\", \"running\"}")?;
    let distros = listed
        .iter()
        .map(|distro| {
            let name = distro.get("name").and_then(Value::as_str);
            let running = distro.get("running").and_then(Value::as_bool);
            match (name, running) {
                (Some(name), Some(running)) if !name.is_empty() => Ok(Distro {
                    name: name.to_string(),
                    running,
                }),
                _ => bail!("a distro is {{\"name\": <text>, \"running\": <bool>}}, not {distro}"),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let wsl = cx.services().locations.read(cx).wsl().clone();
    if !wsl.set_distros(distros.clone()) {
        bail!(
            "this run reaches the machine's WSL (DV_WSL_HOME is set), which says itself which \
             distros there are"
        );
    }
    let names: Vec<Value> = distros
        .iter()
        .map(|distro| json!({"name": distro.name, "running": distro.running}))
        .collect();
    Ok(json!({"distros": names}))
}
