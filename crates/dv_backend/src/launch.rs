//! Starting a connection's server (docs/PLAN.md §5.1, §5.2): a [`Launcher`] says how, and
//! [`Launcher::launch`] yields a [`Transport`], the server's pipes and, when it is a process of
//! its own, that process.
//!
//! - [`Launcher::InProcess`]: `server::serve` on a thread of this process over in-memory pipes,
//!   the protocol and `RemoteBackend` without a process (the conformance suite).
//! - [`Launcher::Command`]: a child process, typically a dv executable's `backend --stdio` on
//!   this machine (a loopback, PLAN §14.2). Its stdin, stdout and stderr are piped and, on
//!   Windows, it is created with no console window (`toolbox_process::Command::to_std`): the
//!   detached app has no console, and a console child would flash a window.
//! - [`Launcher::Wsl`]: a distro's `dv backend --stdio` through `wsl.exe` (`crate::wsl`), this
//!   app's Linux build installed there first when it is missing (`crate::install`); the
//!   connection's state says `Installing` meanwhile.
//! - [`Launcher::Custom`]: whatever a function returns, a test seam for servers that misbehave
//!   or can't start.
//!
//! **The host check** ([`Launcher::host_running`], PLAN §5.2's connecting rule). Only an
//! explicit start may boot a stopped WSL distro, so before an automatic one (a listing's
//! first call, a respawn after a death, a start after an idle close) the connection asks
//! whether the server's host runs: a distro's running state, read afresh from `wsl.exe
//! --list` ([`Asked::Fresh`]). A listing looking at a connection already stopped or failed
//! takes `wsl::distros`' answer, up to 2 s old and shared by every listing asking at once
//! ([`Asked::Listing`]). This machine is always up; a [`Launcher::Custom`] may stand in for a
//! distro with a check of its own ([`Launcher::with_host_check`]).
//!
//! Every process a launcher starts dies with this one (`dv_core::job`), also when this one is
//! killed rather than closing the server's stdin.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::Arc;

use crate::install::{self, LINUX_DIR_VAR, LinuxBuilds};
use crate::wsl::{self, Arch, Shell, WslError};

/// A started server's pipes and process.
pub struct Transport {
    /// The server's stdout: replies and watch batches.
    pub reader: Box<dyn Read + Send>,
    /// The server's stdin: requests.
    pub writer: Box<dyn Write + Send>,
    /// The server's stderr, kept in the diagnostics.
    pub stderr: Option<Box<dyn Read + Send>>,
    /// The server's process, killed and reaped when the link dies; `None` for a server on a
    /// thread.
    pub child: Option<Child>,
    /// The build hash this server should be (PLAN §5.2), which `hello` asks it to confirm;
    /// `None` asks nothing.
    pub build: Option<String>,
    /// What the launcher heard on the way that the diagnostics keep (`wsl.exe`'s warnings, an
    /// install's).
    pub notes: Vec<String>,
}

impl Transport {
    /// A transport over a server's stdout (`reader`) and stdin (`writer`) alone.
    pub fn new(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> Self {
        Self {
            reader: Box::new(reader),
            writer: Box::new(writer),
            stderr: None,
            child: None,
            build: None,
            notes: Vec::new(),
        }
    }
}

/// What a connection asks of a start, and hears from it.
pub struct StartOptions<'a> {
    /// Install the build again before starting, for the architecture a server reported
    /// (`hello`'s `arch`) whose build didn't match (PLAN §5.2). Launchers that install nothing
    /// ignore it.
    pub reinstall: Option<&'a str>,
    /// An install's progress: bytes copied, of the total.
    pub progress: &'a dyn Fn(u64, u64),
}

impl StartOptions<'_> {
    /// No reinstall, no one listening.
    pub fn none() -> StartOptions<'static> {
        StartOptions {
            reinstall: None,
            progress: &|_, _| {},
        }
    }
}

/// Why a server didn't start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct LaunchError {
    pub message: String,
    /// Starting again can't help until someone changes something (no Linux build for the
    /// distro's architecture, no such distro): the connection fails at once, even on a
    /// respawn.
    pub permanent: bool,
}

impl From<io::Error> for LaunchError {
    fn from(error: io::Error) -> Self {
        Self {
            message: error.to_string(),
            permanent: false,
        }
    }
}

impl From<WslError> for LaunchError {
    fn from(error: WslError) -> Self {
        Self {
            permanent: error.permanent(),
            message: error.to_string(),
        }
    }
}

/// The function behind a [`Launcher::Custom`].
pub type StartFn = dyn Fn(&StartOptions<'_>) -> Result<Transport, LaunchError> + Send + Sync;

/// A [`Launcher::Custom`]'s host check: whether the host its servers run on is up, asked as
/// fresh as the argument says (module docs).
pub type HostCheck = dyn Fn(Asked) -> bool + Send + Sync;

/// How fresh a host check's answer must be (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    /// Read afresh: an automatic start, which may follow a death the last listing predates.
    Fresh,
    /// A listing's: an answer up to 2 s old does, one shared by everyone asking at once.
    Listing,
}

/// Starts a connection's server (module docs).
#[derive(Clone)]
pub enum Launcher {
    /// `server::serve` on a thread of this process over in-memory pipes (`std::io::pipe`).
    InProcess,
    /// `program args…` as a child process with piped stdio.
    Command {
        program: PathBuf,
        args: Vec<OsString>,
        /// Changes to this process's environment for the child: `(name, Some(value))` sets a
        /// variable, `(name, None)` removes it.
        env: Vec<(OsString, Option<OsString>)>,
        /// The build hash `hello` asks the server to confirm (PLAN §5.2): the sha256 of
        /// `program` (`build::sha256_file`) when it should be exactly that file; `None` asks
        /// nothing.
        build: Option<String>,
    },
    /// `distro`'s `dv backend --stdio` through `wsl.exe`, installing this app's Linux build
    /// there first when it is missing (module docs). [`Launcher::wsl`] reads the fields from
    /// the environment.
    Wsl {
        distro: String,
        /// The folder holding this app's Linux builds (`install::linux_dir`).
        builds: PathBuf,
        /// The home dv installs into and runs from inside the distro: `DV_WSL_HOME` (a POSIX
        /// path; the tests' sandbox), `None` for the distro user's own.
        home: Option<String>,
    },
    /// Whatever the function returns. `label` names it in the diagnostics; `reinstalls` says
    /// whether a server whose build didn't match is started once more with
    /// [`StartOptions::reinstall`] (as [`Launcher::Wsl`] is); `running`, when given, is its
    /// host check (module docs), standing in for a distro's.
    Custom {
        label: String,
        start: Arc<StartFn>,
        reinstalls: bool,
        running: Option<Arc<HostCheck>>,
    },
}

impl Launcher {
    /// `program backend --stdio` on this machine: a loopback server, whose repositories are
    /// this machine's paths (PLAN §14.2). The app passes its own executable.
    pub fn loopback(program: impl Into<PathBuf>) -> Self {
        Launcher::Command {
            program: program.into(),
            args: vec!["backend".into(), "--stdio".into()],
            env: Vec::new(),
            build: None,
        }
    }

    /// `distro`'s server, this app's Linux builds found through `DV_LINUX_DIR` (else beside
    /// this executable) and installed into `DV_WSL_HOME` when set (else the distro user's own
    /// home), as this process's environment says. The app builds it only through dv_ui's
    /// `WslAccess`, which keeps automation runs and tests out of real distros.
    pub fn wsl(distro: impl Into<String>) -> Self {
        Launcher::Wsl {
            distro: distro.into(),
            builds: install::linux_dir(std::env::var(LINUX_DIR_VAR).ok()),
            home: wsl::home_var(),
        }
    }

    /// A launcher whose server `start` returns (a test seam).
    pub fn custom(
        label: impl Into<String>,
        start: impl Fn() -> io::Result<Transport> + Send + Sync + 'static,
    ) -> Self {
        Launcher::Custom {
            label: label.into(),
            start: Arc::new(move |_: &StartOptions<'_>| start().map_err(LaunchError::from)),
            reinstalls: false,
            running: None,
        }
    }

    /// This launcher with `running` as its host check (module docs): a stand-in for a
    /// distro's running state, for tests and automation, told how fresh an answer is asked
    /// for. A launcher other than a [`Launcher::Custom`] becomes one that starts its servers
    /// as it did, on this machine ([`Launcher::host`]).
    pub fn with_host_check(self, running: impl Fn(Asked) -> bool + Send + Sync + 'static) -> Self {
        let running: Option<Arc<HostCheck>> = Some(Arc::new(running));
        match self {
            Launcher::Custom {
                label,
                start,
                reinstalls,
                ..
            } => Launcher::Custom {
                label,
                start,
                reinstalls,
                running,
            },
            other => Launcher::Custom {
                label: other.describe(),
                reinstalls: other.reinstalls(),
                start: Arc::new(move |options: &StartOptions<'_>| other.launch(options)),
                running,
            },
        }
    }

    /// Whether the host this launcher starts servers on is up, as fresh as `asked` says
    /// (module docs): `Some(false)` when it is stopped, or when that can't be told, so that
    /// only an explicit start boots it; `None` when there is nothing to ask (this machine).
    /// Blocks while `wsl.exe` lists the distros; never call it on a UI thread.
    pub fn host_running(&self, asked: Asked) -> Option<bool> {
        match self {
            Launcher::Wsl { distro, .. } => Some(match asked {
                Asked::Fresh => wsl::running_now(distro),
                Asked::Listing => wsl::running(distro),
            }),
            Launcher::Custom {
                running: Some(running),
                ..
            } => Some(running(asked)),
            Launcher::InProcess | Launcher::Command { .. } | Launcher::Custom { .. } => None,
        }
    }

    /// Starts a server. Blocks while a process starts (a distro boots, a build installs);
    /// never call it on a UI thread.
    pub fn launch(&self, options: &StartOptions<'_>) -> Result<Transport, LaunchError> {
        match self {
            Launcher::InProcess => Ok(in_process()?),
            Launcher::Command {
                program,
                args,
                env,
                build,
            } => Ok(spawn(program, args, env, build.clone())?),
            Launcher::Wsl {
                distro,
                builds,
                home,
            } => {
                let shell = Shell::wsl(distro.clone(), home.clone())?;
                let builds = LinuxBuilds::find(builds)?;
                let reinstall = options.reinstall.and_then(Arch::from_name);
                Ok(wsl::launch(&shell, &builds, reinstall, options.progress)?)
            }
            Launcher::Custom { start, .. } => start(options),
        }
    }

    /// [`Launcher::launch`] asking nothing more of the start.
    pub fn start(&self) -> io::Result<Transport> {
        self.launch(&StartOptions::none())
            .map_err(|error| io::Error::other(error.message))
    }

    /// Whether a server whose build didn't match is worth starting once more with
    /// [`StartOptions::reinstall`] (PLAN §5.2).
    pub fn reinstalls(&self) -> bool {
        match self {
            Launcher::Wsl { .. } => true,
            Launcher::Custom { reinstalls, .. } => *reinstalls,
            Launcher::InProcess | Launcher::Command { .. } => false,
        }
    }

    /// Hears that a server this launcher started answered `hello` and is in use: a distro's
    /// old builds are pruned, once per process (PLAN §5.2).
    pub fn connected(&self) {
        if let Launcher::Wsl {
            distro,
            builds,
            home,
        } = self
            && let Ok(shell) = Shell::wsl(distro.clone(), home.clone())
            && let Ok(builds) = LinuxBuilds::find(builds)
        {
            install::prune_once(&shell, &builds);
        }
    }

    /// How the server starts, for the diagnostics: the command line of a process.
    pub fn describe(&self) -> String {
        match self {
            Launcher::InProcess => "dv's server on a thread of this process".into(),
            Launcher::Command { program, args, .. } => command_line(program.as_os_str(), args),
            Launcher::Wsl {
                distro,
                builds,
                home,
            } => match Shell::wsl(distro.clone(), home.clone()) {
                Ok(shell) => format!(
                    "{} (the Linux builds from {})",
                    shell.describe("dv's launch script", &["<x86_64 build>", "<aarch64 build>"]),
                    builds.display()
                ),
                Err(error) => format!("dv in {distro} through wsl.exe, which can't start: {error}"),
            },
            Launcher::Custom { label, .. } => label.clone(),
        }
    }

    /// Where the server runs, which decides how its repositories' locations read.
    pub fn host(&self) -> Host {
        match self {
            Launcher::Wsl { distro, .. } => Host::Wsl {
                distro: distro.clone(),
            },
            Launcher::InProcess | Launcher::Command { .. } | Launcher::Custom { .. } => Host::Local,
        }
    }
}

impl fmt::Debug for Launcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Where a connection's server runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    /// This machine: a repository's location stays the server's local path.
    Local,
    /// A WSL distro: a repository is `Location::Wsl { distro, path: <its root> }`.
    Wsl { distro: String },
}

/// A server on a thread over two in-memory pipes ([`Launcher::InProcess`]). Closing the
/// client's end of the request pipe ends it.
fn in_process() -> io::Result<Transport> {
    let (server_in, client_out) = io::pipe()?;
    let (client_in, server_out) = io::pipe()?;
    std::thread::Builder::new()
        .name("dv-backend-in-process".into())
        .spawn(move || {
            if let Err(error) = crate::server::serve(server_in, server_out) {
                log::warn!("the in-process backend stopped: {error}");
            }
        })?;
    Ok(Transport::new(client_in, client_out))
}

/// Starts `program args…` with piped stdio and `env` applied ([`Launcher::Command`]).
fn spawn(
    program: &Path,
    args: &[OsString],
    env: &[(OsString, Option<OsString>)],
    build: Option<String>,
) -> io::Result<Transport> {
    let mut child = env
        .iter()
        .fold(
            toolbox_process::Command::new(program).args(args),
            |command, (name, value)| match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            },
        )
        .to_std()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", program.display())))?;
    dv_core::job::tie_to_this_process(&child);
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other(format!(
            "{}: its pipes weren't created",
            program.display()
        )));
    };
    let stderr = child
        .stderr
        .take()
        .map(|stderr| Box::new(stderr) as Box<dyn Read + Send>);
    Ok(Transport {
        reader: Box::new(stdout),
        writer: Box::new(stdin),
        stderr,
        child: Some(child),
        build,
        notes: Vec::new(),
    })
}

/// `program args…` as one line to read, an argument quoted when it is empty or has a space.
fn command_line(program: &OsStr, args: &[OsString]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(OsString::as_os_str))
        .map(|arg| {
            let arg = arg.to_string_lossy();
            if arg.is_empty() || arg.contains(char::is_whitespace) {
                format!("\"{arg}\"")
            } else {
                arg.into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_describes_itself_as_its_command_line() {
        let launcher = Launcher::loopback(r"C:\Program Files\dv\dv.exe");
        assert_eq!(
            launcher.describe(),
            r#""C:\Program Files\dv\dv.exe" backend --stdio"#
        );
        assert_eq!(launcher.host(), Host::Local);
        assert!(!launcher.reinstalls());
        assert_eq!(format!("{launcher:?}"), launcher.describe());
        assert_eq!(
            command_line(OsStr::new("dv"), &["".into(), "a b".into(), "c".into()]),
            r#"dv "" "a b" c"#
        );
    }

    #[test]
    fn a_command_that_cant_start_says_which() {
        let launcher = Launcher::loopback("dv-that-does-not-exist");
        let Err(error) = launcher.start() else {
            panic!("a missing program started");
        };
        assert!(
            error.to_string().starts_with("dv-that-does-not-exist: "),
            "{error}"
        );
    }

    #[test]
    fn a_distro_describes_its_wsl_command_and_names_its_host() {
        let launcher = Launcher::Wsl {
            distro: "Ubuntu".into(),
            builds: PathBuf::from(r"C:\dv"),
            home: Some("/tmp/dv-home".into()),
        };
        assert_eq!(
            launcher.describe(),
            "wsl.exe -d Ubuntu --cd / --exec /usr/bin/env HOME=/tmp/dv-home /bin/sh -c \
             <dv's launch script> sh <x86_64 build> <aarch64 build> (the Linux builds from \
             C:\\dv)"
        );
        assert_eq!(
            launcher.host(),
            Host::Wsl {
                distro: "Ubuntu".into()
            }
        );
        assert!(launcher.reinstalls());
    }

    #[test]
    fn a_distro_without_a_linux_build_says_which_file_is_missing_without_starting_wsl() {
        let empty = tempfile::tempdir().unwrap();
        let launcher = Launcher::Wsl {
            distro: "Ubuntu".into(),
            builds: empty.path().to_path_buf(),
            home: Some("/tmp/dv-never-used".into()),
        };
        let Err(error) = launcher.launch(&StartOptions::none()) else {
            panic!("started without a build");
        };
        let file = empty.path().join(Arch::host().file_name());
        assert!(error.permanent);
        assert_eq!(
            error.message,
            format!(
                "no Linux build of dv for {}: {} is missing (script/build-linux{} makes it)",
                Arch::host(),
                file.display(),
                if Arch::host() == Arch::Aarch64 {
                    " --aarch64"
                } else {
                    ""
                }
            )
        );
    }

    #[test]
    fn only_a_distro_or_its_stand_in_has_a_host_to_ask_after() {
        for asked in [Asked::Fresh, Asked::Listing] {
            assert_eq!(Launcher::InProcess.host_running(asked), None);
            assert_eq!(Launcher::loopback("dv").host_running(asked), None);
        }
        let up = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let heard = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let launcher = Launcher::InProcess.with_host_check({
            let up = Arc::clone(&up);
            let heard = Arc::clone(&heard);
            move |asked| {
                heard.lock().push(asked);
                up.load(std::sync::atomic::Ordering::SeqCst)
            }
        });
        assert_eq!(launcher.host_running(Asked::Fresh), Some(false));
        up.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(launcher.host_running(Asked::Listing), Some(true));
        assert_eq!(*heard.lock(), [Asked::Fresh, Asked::Listing]);
        // It starts its servers as before, on this machine.
        assert_eq!(launcher.host(), Host::Local);
        assert_eq!(
            launcher.describe(),
            "dv's server on a thread of this process"
        );
        drop(launcher.start().expect("an in-process server"));
    }

    #[test]
    fn a_sandbox_home_that_isnt_an_absolute_posix_path_is_refused() {
        let launcher = Launcher::Wsl {
            distro: "Ubuntu".into(),
            builds: PathBuf::from("."),
            home: Some("relative/home".into()),
        };
        let Err(error) = launcher.launch(&StartOptions::none()) else {
            panic!("started with a bad home");
        };
        assert!(error.permanent);
        assert_eq!(
            error.message,
            "DV_WSL_HOME must be an absolute path inside the distro, not \"relative/home\""
        );
    }
}
