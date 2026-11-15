//! How `dv` (no subcommand) and `dv open` start the app (docs/PLAN.md §10.1, §13.1).
//!
//! `dv.exe` is a console program, so every headless command prints and returns its exit code
//! in a terminal. The app itself starts **detached**: `dv-gui` (on Windows the windows-subsystem
//! binary, on macOS the app bundle's, PLAN §13.1) when a packaged build (an installed copy) has
//! it beside this executable, else this same binary run again with the hidden `--no-detach`,
//! with no
//! console (`DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` on Windows, its own session,
//! `setsid`, elsewhere), its standard streams closed and none of the caller's handles inherited
//! ([`spawn_detached`]), while the first one returns 0 at once, also to a caller that captures
//! its output. `--automation` (a driver talks to it over stdio) and `--no-detach` run the app
//! in this process instead, as `dv-gui.exe` always does ([`launch`]).
//! The detached copy's line names what it opens absolutely (`open <path>`, [`detached_args`]),
//! so it opens the same repository whatever folder it starts in.
//!
//! What the app starts on ([`Startup`]) is worked out before anything starts; before
//! detaching, a path that isn't a repository and a review that doesn't exist are refused in
//! the terminal ([`check`]), since the detached app has no terminal to say so in. A pull
//! request (`dv open --pr 482`, its URL; PLAN §9.3) is opened by the app, where a `gh` that
//! fails shows as the main area's banner: nothing asks GitHub before the window is up. A
//! second app on the same data dir is allowed for now (PLAN §17).

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use dv_cli::{CliError, GlobalArgs};
use dv_core::backend::{Backend, LocalBackend, LocalBackendOptions};
use dv_core::github::PrRef;
use dv_core::location::Location;
use dv_core::review::{ReviewId, ReviewStore, StoreError};

use crate::cli::{Cli, OpenArgs};

/// Which binary is running: `dv`, the console program, or `dv-gui`, the app's own (PLAN
/// §13.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Console,
    Gui,
}

/// The app's binary's file name beside `dv` (PLAN §13.1).
pub const GUI_EXE: &str = if cfg!(windows) {
    "dv-gui.exe"
} else {
    "dv-gui"
};

/// Where the app runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launch {
    /// In this process: `--automation`, `--automation-pipe` or `--no-detach`.
    InProcess,
    /// In a detached copy of this binary, this one returning at once.
    Detach,
}

/// Where `cli` (a GUI command line: no subcommand, or `open`) runs the app when `entry` reads
/// it: `dv-gui` never detaches, being the detached app itself.
pub fn launch(cli: &Cli, entry: Entry) -> Launch {
    if entry == Entry::Gui || cli.automation_enabled() || cli.no_detach {
        Launch::InProcess
    } else {
        Launch::Detach
    }
}

/// What the app opens once its window is up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Startup {
    /// Nothing: an automation run started without `open` (scenarios open what they need).
    Nothing,
    /// Bare `dv`: the working directory's repository, the switcher open on it, when it is one;
    /// nothing otherwise (dv started from a shortcut, say).
    CurrentRepo(Location),
    /// `dv open [<path>]`: that repository, the switcher open on it (the automation `open`
    /// command's behaviour); one that doesn't open shows why in the main area's banner.
    Repo(Location),
    /// `dv open [<path>] --review <id>`: that review, through `Shell::open`.
    Review(Location, ReviewId),
    /// `dv open --pr <PR>` or `dv open <PR>`: that pull request's review in the repository
    /// the line names (`--pr`'s path, `--repo` or `--wsl`), else the working directory's,
    /// through `Shell::open` (PLAN §9.3).
    Pr(Location, PrRef),
}

/// What the app should open for `cli`, whose subcommand was `open` (`open`'s arguments) or
/// none, run in `cwd`, inside the WSL distro `here` when it runs in one (`dv_cli::this_distro`:
/// a Windows drive's path then names that drive's mount, as for the commands). The pull
/// request is `--pr`'s, else `open`'s argument when it reads as one and names no path that
/// `exists` ([`OpenArgs::pr`]); any other argument is a path. Two locations at once, a pull
/// request with `--review`, a `--pr` that names none, a malformed review id and the headless
/// commands' `--json` and `--author` (the app prints nothing and acts as the reviewer, PLAN
/// §10.1) are usage errors (exit 2), before any window opens.
pub fn startup(
    cli: &Cli,
    open: Option<&OpenArgs>,
    cwd: &Path,
    here: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<Startup, CliError> {
    if cli.global.json {
        return Err(CliError::usage(
            "`dv` and `dv open` start the app, which prints no JSON: --json is for the commands \
             that print (`dv status --json`)",
        ));
    }
    if cli.global.author.is_some() {
        return Err(CliError::usage(
            "the app always acts as the reviewer: --author is for the commands an agent runs \
             (`dv comment reply <id> --author <name>`)",
        ));
    }
    let target = open.and_then(|open| open.path(cwd, exists));
    let reference = open
        .and_then(|open| open.pr(cwd, exists))
        .map(|reference| {
            reference
                .parse::<PrRef>()
                .map_err(|error| CliError::usage(format!("--pr: {error}")))
        })
        .transpose()?;
    if reference.is_some() && cli.global.review.is_some() {
        return Err(CliError::usage(
            "`dv open` takes a pull request or --review, not both: a pull request opens its \
             own review",
        ));
    }
    let named = target.is_some() || cli.global.repo.is_some() || cli.global.wsl.is_some();
    if target.is_some() && (cli.global.repo.is_some() || cli.global.wsl.is_some()) {
        return Err(CliError::usage(
            "`dv open` takes one repository: a path, --repo or --wsl",
        ));
    }
    let global = GlobalArgs {
        repo: target
            .map(str::to_string)
            .or_else(|| cli.global.repo.clone()),
        ..cli.global.clone()
    };
    let location = dv_cli::location(&global, cwd, here)?;
    if let Some(reference) = reference {
        return Ok(Startup::Pr(location, reference));
    }
    if let Some(id) = &cli.global.review {
        let id = ReviewId::parse(id.trim())
            .map_err(|error| CliError::usage(format!("--review: {error}")))?;
        return Ok(Startup::Review(location, id));
    }
    Ok(if named || open.is_some() {
        Startup::Repo(location)
    } else if cli.automation_enabled() {
        Startup::Nothing
    } else {
        Startup::CurrentRepo(location)
    })
}

/// Checks, before detaching, that `startup` names a repository that opens (the app passes the
/// default `options`: the user's own git configuration, as the app will open it) and, for a
/// review, one the repository has. A WSL location is left to the app, which reaches it through
/// its distro's `dv backend --stdio` (PLAN §5.2): its path means nothing to this machine's file
/// system, and checking it here would start a server (and boot the distro) only for the app to
/// start them again. Bare `dv` outside a repository is fine (the app opens on nothing).
pub fn check(startup: &Startup, options: LocalBackendOptions) -> Result<(), CliError> {
    let (path, review) = match startup {
        Startup::Repo(Location::Local(path)) | Startup::Pr(Location::Local(path), _) => {
            (path, None)
        }
        Startup::Review(Location::Local(path), id) => (path, Some(id)),
        _ => return Ok(()),
    };
    let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_with(path, options)?);
    if let Some(id) = review {
        let root = backend.repo().toplevel.clone();
        match ReviewStore::new(backend).load(id) {
            Ok(_) => {}
            Err(StoreError::UnknownReview { .. }) => {
                return Err(CliError::not_found(format!("no review {id} in {root}")));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// The command line of the detached copy (`args` this one's, the program name first): this
/// line's own options for the app (`--data-dir`, `--window-size`), `--no-detach`, then what it
/// opens as [`open_args`] spell it, absolute, from `startup`: a path, `--repo` and `--wsl` all
/// become `open <absolute path>` (`--review` after it), a pull request `open <absolute path>
/// --pr <its reference>` ([`open_pr_args`]), and a bare `dv` in a repository
/// (`current_is_repo`) `open <its absolute path>`, so the app opens the same thing whatever
/// folder it starts in. A bare `dv` elsewhere opens nothing, as here.
pub fn detached_args(args: &[OsString], startup: &Startup, current_is_repo: bool) -> Vec<OsString> {
    let mut line = app_options(args);
    line.push(OsString::from("--no-detach"));
    match startup {
        Startup::Nothing => {}
        Startup::CurrentRepo(location) => {
            if current_is_repo && matches!(location, Location::Local(_)) {
                line.extend(open_args(location, None));
            }
        }
        Startup::Repo(location) => line.extend(open_args(location, None)),
        Startup::Review(location, id) => line.extend(open_args(location, Some(id))),
        Startup::Pr(location, reference) => line.extend(open_pr_args(location, reference)),
    }
    line
}

/// `open <location> --pr <reference>`: the pull request in the repository at `location`,
/// absolute as [`open_args`] spells it.
pub fn open_pr_args(location: &Location, reference: &PrRef) -> Vec<OsString> {
    let reference = match reference {
        PrRef::Number(number) | PrRef::Hash(number) => format!("#{number}"),
        PrRef::Short {
            owner,
            repo,
            number,
        } => format!("{owner}/{repo}#{number}"),
        PrRef::Url {
            host,
            owner,
            repo,
            number,
        } => format!("https://{host}/{owner}/{repo}/pull/{number}"),
    };
    let mut line = open_args(location, None);
    line.extend([OsString::from("--pr"), OsString::from(reference)]);
    line
}

/// `open <location> [--review <id>]`, the location absolute: a local path made absolute, a WSL
/// one as its `\\wsl.localhost\<distro>\<path>` share. Also what an update's restart reopens
/// (`crate::updates`).
pub fn open_args(location: &Location, review: Option<&ReviewId>) -> Vec<OsString> {
    let path = match location {
        Location::Local(path) => std::path::absolute(path)
            .unwrap_or_else(|_| path.clone())
            .into_os_string(),
        Location::Wsl { .. } => OsString::from(location.unc_path().unwrap_or_default()),
    };
    let mut line = vec![OsString::from("open"), path];
    if let Some(id) = review {
        line.push(OsString::from("--review"));
        line.push(OsString::from(id.as_str()));
    }
    line
}

/// The app's own options at the start of `args` (the program name first, skipped), with their
/// values: the tree's options that aren't global (the global ones, and everything from the
/// command on, are what [`detached_args`] spells as `open …`), but `--no-detach`.
fn app_options(args: &[OsString]) -> Vec<OsString> {
    use clap::CommandFactory as _;
    let tree = crate::cli::Cli::command();
    let mut out = Vec::new();
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        let Some(name) = arg
            .to_str()
            .and_then(|text| text.strip_prefix("--"))
            .filter(|name| !name.is_empty())
        else {
            break;
        };
        let (name, inline) = match name.split_once('=') {
            Some((name, _)) => (name, true),
            None => (name, false),
        };
        let Some(option) = tree
            .get_arguments()
            .find(|option| option.get_long() == Some(name))
        else {
            break;
        };
        let value = if !inline && option.get_action().takes_values() {
            rest.next()
        } else {
            None
        };
        if !option.is_global_set() && name != "no-detach" {
            out.push(arg.clone());
            out.extend(value.cloned());
        }
    }
    out
}

/// The program the detached app runs, for this executable `exe` (module docs): in a packaged
/// build (`packaged`) on Windows and macOS, `dv-gui` beside it when `exists` says it is there;
/// else `exe` itself, so a development build never starts a `dv-gui` an older build left.
pub fn detached_program(
    exe: &Path,
    packaged: bool,
    exists: impl Fn(&Path) -> bool,
) -> std::path::PathBuf {
    let gui = exe.with_file_name(GUI_EXE);
    if cfg!(any(windows, target_os = "macos")) && packaged && exists(&gui) {
        gui
    } else {
        exe.to_path_buf()
    }
}

/// Whether `startup`'s bare-`dv` folder is a repository, opened with `options` (the app's:
/// the user's own git configuration), for [`detached_args`].
pub fn current_is_repo(startup: &Startup, options: LocalBackendOptions) -> bool {
    match startup {
        Startup::CurrentRepo(Location::Local(path)) => {
            LocalBackend::open_with(path, options).is_ok()
        }
        _ => false,
    }
}

/// Starts the app with [`detached_args`] ([`detached_program`]), detached from this process's
/// console and process group (its session, off Windows), its standard streams closed (module
/// docs, [`spawn_detached`]).
pub fn detach(args: &[OsString], startup: &Startup) -> std::io::Result<()> {
    let exe = dv_cli::this_executable(&dv_cli::Env::Process)?;
    let program = detached_program(&exe, dv_cli::is_packaged(), Path::is_file);
    let mut command = std::process::Command::new(program);
    let here = current_is_repo(startup, LocalBackendOptions::default());
    command.args(detached_args(args, startup, here));
    spawn_detached(command)
}

/// Starts `command` detached: no console and a process group of its own (a session, off
/// Windows), its standard streams closed, and none of this process's standard handles handed
/// down. On Windows a child inherits every inheritable handle of its parent, and this
/// process's stdout and stderr are
/// whatever its caller gave it — a pipe, when the caller captures `dv open`'s output
/// (`$(dv open .)`, an agent's shell tool) — so the app would hold that pipe open for as long
/// as it runs, and the caller would wait for the window to close before seeing the end of the
/// output. They are made non-inheritable first; this process exits right after.
pub(crate) fn spawn_detached(mut command: std::process::Command) -> std::io::Result<()> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        keep_std_handles_from_children();
    }
    // A session of its own: no controlling terminal, so closing the terminal `dv open` ran in
    // sends the app no hangup. And none of the caller's other descriptors: what dv opens
    // itself is close-on-exec already, but whatever this process inherited (a pipe its
    // caller's shell left open as fd 3 and up) would otherwise stay open in the app.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let limit = descriptor_limit();
        // SAFETY: the closure runs in the child between fork and exec and calls only
        // `setsid`, `syscall(close_range)` and `fcntl`, which are async-signal-safe and touch
        // no memory of this process; `limit` was read before the fork.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                close_inherited_on_exec(limit);
                Ok(())
            });
        }
    }
    // The child is never waited for: it outlives this process.
    command.spawn().map(drop)
}

/// One past the highest descriptor a process may have (`RLIMIT_NOFILE`'s soft limit), capped
/// so the fallback loop of [`close_inherited_on_exec`] stays short.
#[cfg(unix)]
fn descriptor_limit() -> libc::c_int {
    // SAFETY: `sysconf` takes no pointers.
    let max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    libc::c_int::try_from(max.clamp(256, 65_536)).unwrap_or(65_536)
}

/// Marks every descriptor from 3 up close-on-exec, so the program `exec` starts holds only its
/// standard streams ([`spawn_detached`]). Marked rather than closed: the standard library's
/// own pipe, which tells the parent whether `exec` failed, must stay open until then (it is
/// close-on-exec already). `close_range` does it at once on Linux 5.11 and later; elsewhere
/// each descriptor below `limit`. Only between fork and exec: async-signal-safe calls alone.
#[cfg(unix)]
unsafe fn close_inherited_on_exec(limit: libc::c_int) {
    #[cfg(target_os = "linux")]
    // SAFETY: `close_range` takes no pointers; on a kernel without it the call fails (ENOSYS,
    // EINVAL) and the loop below does the same.
    unsafe {
        if libc::syscall(
            libc::SYS_close_range,
            3 as libc::c_uint,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        ) == 0
        {
            return;
        }
    }
    for fd in 3..limit {
        // SAFETY: `fcntl` with these commands takes no pointers; a descriptor that isn't open
        // fails with EBADF and is skipped.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// Makes this process's standard handles non-inheritable ([`spawn_detached`]). Best effort: a
/// handle that isn't there, or whose flag can't change, is left as it is.
#[cfg(windows)]
fn keep_std_handles_from_children() {
    use windows_sys::Win32::Foundation::{
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` takes no pointers; `SetHandleInformation` changes one flag of
        // a handle this process holds, and a null or invalid handle is skipped before it.
        unsafe {
            let handle = GetStdHandle(which);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests;
