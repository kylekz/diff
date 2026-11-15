//! `dv`: the entry point for the GUI, the CLI and `dv backend --stdio` (docs/PLAN.md §4, §10,
//! §13.1), shared by the crate's two binaries ([`main`]).
//!
//! gpui: **with the `gui` feature** (Cargo.toml lists the three shapes). One clap tree parses
//! every command line ([`cli`]); a headless command goes to `dv_cli` before any gpui code runs
//! (PLAN §4), while `dv` alone and `dv open` start the app, detached unless asked otherwise
//! ([`launch`]). Built without `gui` (the Linux build installed into WSL distros) they say to
//! open the repository from Windows instead ([`windowless`]).
//!
//! **The binaries** (PLAN §13.1): `dv` (`src/main.rs`, `dv.exe`) is a console program in every
//! build, so the CLI prints and returns its exit code in a terminal; `dv-gui`
//! (`src/bin/dv-gui.rs`, with `gui`) runs the app in its own process: on Windows the
//! windows-subsystem binary with no console, on macOS the app bundle's executable; what the
//! Start menu's shortcut, the Dock and Velopack start, and what a packaged `dv` starts for `dv`
//! and `dv open` when it is beside it. On Windows and macOS both first hand their command line
//! to Velopack ([`installer`], PLAN D9), whose install, update and uninstall hooks run there and
//! exit on Windows.

#[cfg(feature = "gui")]
mod app;
#[cfg(feature = "automation")]
mod automation;
mod cli;
#[cfg(feature = "gui")]
mod crash;
#[cfg(feature = "gui")]
mod data_dir;
#[cfg(all(target_os = "macos", feature = "gui"))]
mod dock_icon;
#[cfg(feature = "gui")]
mod fatal;
#[cfg(all(any(windows, target_os = "macos"), feature = "gui"))]
mod installer;
#[cfg(feature = "gui")]
mod launch;
#[cfg(all(any(windows, target_os = "macos"), feature = "gui"))]
mod processes;
#[cfg(all(any(windows, target_os = "macos"), feature = "gui"))]
mod updates;
#[cfg(any(not(feature = "gui"), test))]
mod windowless;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser as _;
use dv_cli::Context;

use crate::cli::{Cli, Command};

/// Which binary is running ([`launch::Entry`]); without the app there is only `dv`.
#[cfg(feature = "gui")]
pub use launch::Entry;

/// Which binary is running; without the app there is only `dv`.
#[cfg(not(feature = "gui"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Console,
}

/// Runs this process's command line as the binary `entry`.
pub fn main(entry: Entry) -> ExitCode {
    #[cfg(all(any(windows, target_os = "macos"), feature = "gui"))]
    installer::run();
    let args = without_process_serial(std::env::args_os().collect());
    let mut cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => return parse_error(entry, &error, &args),
    };
    match cli.command.take() {
        Some(Command::Headless(command)) => headless(entry, &cli, command, &args),
        Some(Command::Open(open)) => gui(cli, Some(open), &args, entry),
        None => gui(cli, None, &args, entry),
    }
}

/// A command line that doesn't parse: `dv` says why in the terminal (PLAN §10.3); `dv-gui`,
/// which has none, in a message box (the log, off Windows: `fatal`).
fn parse_error(entry: Entry, error: &clap::Error, args: &[OsString]) -> ExitCode {
    #[cfg(feature = "gui")]
    if entry == Entry::Gui {
        // `--help` and `--version` aren't errors: their text, and exit 0.
        if matches!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
        ) {
            fatal::show_info(&error.render().to_string());
            return ExitCode::SUCCESS;
        }
        fatal::show_message(&format!("dv couldn't start.\n\n{}", error.render()));
        return ExitCode::from(2);
    }
    let _ = entry;
    let mut ctx = Context::process();
    dv_cli::output::parse_error(error, args, &mut ctx).into()
}

/// A headless command: run here by `dv`; refused by `dv-gui`, whose output would go nowhere.
fn headless(entry: Entry, cli: &Cli, command: dv_cli::Command, args: &[OsString]) -> ExitCode {
    #[cfg(feature = "gui")]
    if entry == Entry::Gui {
        fatal::show_message(
            "dv-gui opens the app. The commands run from dv in a terminal: `dv --help` lists \
             them.",
        );
        return ExitCode::from(2);
    }
    let _ = entry;
    init_logging("warn");
    let serving = matches!(command, dv_cli::Command::Backend(_));
    let mut ctx = Context::process();
    // What a command run inside a WSL distro passes on (PLAN §10.4).
    ctx.args = cli::headless_args(args);
    let exit = dv_cli::run(&cli.global, command, &mut ctx);
    if serving {
        // The backend's input ended: requests still in git end with this process,
        // which is how a server dies with its client (PLAN §5.1).
        std::process::exit(i32::from(exit.code()));
    }
    exit.into()
}

/// `dv` or `dv open`: the app, detached or in this process ([`launch`]).
#[cfg(feature = "gui")]
fn gui(cli: Cli, open: Option<cli::OpenArgs>, args: &[OsString], entry: Entry) -> ExitCode {
    use dv_cli::{CliError, Exit};

    use crate::launch::Launch;

    let json = cli.global.json;
    let report = |error: CliError| -> ExitCode {
        if entry == Entry::Gui {
            fatal::show_message(&format!("dv couldn't start.\n\n{}", error.message));
            return ExitCode::from(error.exit.code());
        }
        dv_cli::output::error(&mut Context::process(), json, &error).into()
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let here = dv_cli::this_distro(&dv_cli::Env::Process);
    let startup = match launch::startup(&cli, open.as_ref(), &cwd, here.as_deref(), &|path| {
        path.exists()
    }) {
        Ok(startup) => startup,
        Err(error) => return report(error),
    };
    match launch::launch(&cli, entry) {
        Launch::Detach => {
            init_logging("warn");
            if let Err(error) = launch::check(&startup, Default::default()) {
                return report(error);
            }
            match launch::detach(args, &startup) {
                Ok(()) => Exit::Ok.into(),
                Err(error) => report(CliError::new(
                    Exit::Failure,
                    dv_cli::error::code::IO,
                    format!("couldn't start the app: {error}"),
                )),
            }
        }
        Launch::InProcess => {
            init_logging("info");
            // A driver is waiting on a scripted run; a modal error box would only hang it.
            let show_dialog = !cli.automation_enabled();
            match app::run(cli, startup, show_dialog) {
                Ok(code) => code,
                Err(error) => {
                    fatal::report(&error, show_dialog);
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// `dv` or `dv open` in a build without a window: exit 2, saying where to open the repository
/// instead ([`windowless`]).
#[cfg(not(feature = "gui"))]
fn gui(cli: Cli, open: Option<cli::OpenArgs>, _args: &[OsString], _entry: Entry) -> ExitCode {
    let mut ctx = Context::process();
    let error = windowless::refusal(
        &cli.global,
        open.as_ref(),
        &ctx.cwd,
        std::env::var("WSL_DISTRO_NAME").ok().as_deref(),
        &|path| path.exists(),
    );
    dv_cli::output::error(&mut ctx, cli.global.json, &error).into()
}

/// Logs to stderr at `level` unless `RUST_LOG` says otherwise: `warn` for the CLI, whose
/// stderr a person or an agent reads, `info` for the app.
fn init_logging(level: &str) {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level))
        .target(env_logger::Target::Stderr)
        .init();
}

/// `args` without the `-psn_…` process serial number macOS has passed an app that Finder or
/// the Dock starts (`dv-gui`, PLAN §13.1), which no command line of dv's means. Elsewhere
/// `args` as they are.
fn without_process_serial(args: Vec<OsString>) -> Vec<OsString> {
    if !cfg!(target_os = "macos") {
        return args;
    }
    args.into_iter()
        .enumerate()
        .filter(|(at, arg)| *at == 0 || !arg.to_str().is_some_and(|arg| arg.starts_with("-psn_")))
        .map(|(_, arg)| arg)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    #[test]
    fn a_process_serial_number_is_dropped_on_macos() {
        let line = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
        let started = line(&["dv-gui", "-psn_0_12345"]);
        let expected = if cfg!(target_os = "macos") {
            line(&["dv-gui"])
        } else {
            started.clone()
        };
        assert_eq!(super::without_process_serial(started), expected);
        let open = line(&["dv-gui", "open", "/code/api"]);
        assert_eq!(super::without_process_serial(open.clone()), open);
    }
}
