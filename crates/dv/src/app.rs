//! GUI startup: the data dir, then gpui with dv's assets, fonts, services and theme, the main
//! window, with `--automation` the automation driver, and what `dv open` asked for
//! ([`Startup`]).

use std::{cell::Cell, path::PathBuf, process::ExitCode, rc::Rc};

use anyhow::{Result, bail};
use dv_ui::appearance::MaterialSupport;
use dv_ui::services::reviews::OpenRequest;
use dv_ui::services::{ServicesConfig, SetupConfig, WslAccess};
use dv_ui::shell::{IfNotOpened, MainWindow};
use dv_ui::system::SystemShell;
use gpui::{App, QuitMode, px, size};

use crate::cli::{Cli, WindowSize};
use crate::data_dir::Source;
use crate::launch::Startup;

/// Starts the app, blocks until it quits and returns the process exit code.
///
/// Errors before the app starts are returned, for the caller to report. A failure during
/// startup inside the app (the window can't open, the fonts don't load) is reported where it
/// happens, through `fatal::report` with `show_dialog` (the detached app has no console, PLAN
/// §13.1), and the process then exits with [`ExitCode::FAILURE`]: `Application::run` can't
/// hand an error back, and on macOS it never returns after a quit.
pub fn run(cli: Cli, open: Startup, show_dialog: bool) -> Result<ExitCode> {
    let automation = cli.automation_enabled();
    if automation && !cfg!(feature = "automation") {
        bail!("--automation and --automation-pipe need a build with the `automation` feature");
    }

    let place = crate::data_dir::resolve(cli.data_dir.clone(), automation)?;
    let data_dir = crate::data_dir::prepare(&place)?;
    // A panic's report goes beside the app's other files; an automation run's folder is
    // removed when it ends, so there the default hook's stderr is the report.
    if !automation {
        crate::crash::install(&data_dir);
    }
    log::info!(
        "data directory: {} ({})",
        data_dir.display(),
        place.source.as_str()
    );
    let temporary = (place.source == Source::Automation).then(|| data_dir.clone());
    let requested = cli.window_size.unwrap_or(WindowSize::DEFAULT);
    let window_size = requested.at_least_min();
    if window_size != requested {
        log::warn!("--window-size {requested} is below the minimum; using {window_size}");
    }
    let startup = StartOptions {
        data_dir,
        automation,
        automation_pipe: cli.automation_pipe.clone(),
        background: cli.automation_background,
        window_size,
        open,
    };

    let startup_failed = Rc::new(Cell::new(false));
    gpui_platform::application()
        .with_assets(dv_ui::Assets)
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run({
            let startup_failed = startup_failed.clone();
            move |cx| {
                start_or_quit(
                    &startup_failed,
                    |error| crate::fatal::report(error, show_dialog),
                    cx,
                    |cx| start(startup, cx),
                )
            }
        });
    // An automation run's temporary data dir goes with it; everything in it was written before
    // the event loop ended. (On macOS `run` never returns and the OS's temp cleanup takes it.)
    if let Some(dir) = temporary
        && let Err(error) = std::fs::remove_dir_all(&dir)
    {
        log::warn!("couldn't remove {}: {error}", dir.display());
    }
    Ok(if startup_failed.get() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Runs `start`. On failure hands the error to `report`, sets `failed` and quits the app, so
/// [`run`] can exit non-zero once the event loop ends.
fn start_or_quit(
    failed: &Cell<bool>,
    report: impl FnOnce(&anyhow::Error),
    cx: &mut App,
    start: impl FnOnce(&mut App) -> Result<()>,
) {
    if let Err(error) = start(cx) {
        report(&error);
        failed.set(true);
        cx.quit();
    }
}

/// Everything [`start`] needs from the command line.
struct StartOptions {
    data_dir: PathBuf,
    automation: bool,
    automation_pipe: Option<String>,
    /// `--automation-background`.
    background: bool,
    window_size: WindowSize,
    /// What to open once the window is up.
    open: Startup,
}

fn start(startup: StartOptions, cx: &mut App) -> Result<()> {
    let StartOptions {
        data_dir,
        automation,
        automation_pipe,
        background,
        window_size,
        open,
    } = startup;
    dv_ui::init(cx)?;
    // A scripted run opens no browser, Explorer window or folder picker: it records them.
    dv_ui::system::install(
        if automation {
            SystemShell::scripted()
        } else {
            SystemShell::Real
        },
        cx,
    );
    // An automation run reaches WSL only through the sandbox seam (`WslAccess::for_run`).
    let wsl = WslAccess::for_run(automation);
    let setup = setup_config(&data_dir, automation);
    dv_ui::services::init(
        ServicesConfig {
            data_dir,
            wsl,
            setup,
        },
        cx,
    );
    // The settings' material and mode, before the first frame.
    dv_ui::appearance::init(MaterialSupport::probe(), cx);

    dv_ui::automation::set_background(background);
    let main = dv_ui::shell::open_main_window(
        size(px(window_size.width as f32), px(window_size.height as f32)),
        cx,
    )?;
    // Automation first: its host registers the seeded repos' routes (a `<name>.loopback`
    // repo goes through the loopback), which what `dv open` names must see.
    if automation {
        start_automation(&main, automation_pipe, cx)?;
    }
    open_startup(open, &main, cx)?;
    // An installed copy's updates, in the background (PLAN D9); none under automation.
    #[cfg(any(windows, target_os = "macos"))]
    crate::updates::start(automation, main.shell.clone(), cx);
    // A development build has no bundle to take its Dock icon from.
    #[cfg(target_os = "macos")]
    crate::dock_icon::set_unless_bundled();
    // A background run must never take the foreground from the person at the machine.
    if !dv_ui::automation::is_background() {
        cx.activate(true);
    }
    Ok(())
}

/// Where the setup checks look and write (PLAN §12): the real machine for the app; for an
/// automation run a scratch home and a stand-in user `Path` in its data dir (the `cli`
/// children's own, `automation::agent_cli`), never the owner's (CLAUDE.md), and the first-run
/// screen only when the run asks for it (`DV_FIRST_RUN=1`: every scenario starts on a fresh
/// data dir).
fn setup_config(data_dir: &std::path::Path, automation: bool) -> SetupConfig {
    if automation {
        SetupConfig::sandboxed(
            data_dir.join(AUTOMATION_HOME),
            data_dir.join(AUTOMATION_USER_PATH),
            std::env::var("DV_FIRST_RUN").is_ok_and(|value| value.trim() == "1"),
        )
    } else {
        SetupConfig::real()
    }
}

/// An automation run's home directory, in its data dir.
pub const AUTOMATION_HOME: &str = "home";

/// The file standing in for the user `Path` in an automation run, in its data dir.
pub const AUTOMATION_USER_PATH: &str = "user-path.txt";

/// Opens what `dv` or `dv open` asked for in the main window: the working directory's or the
/// named repository with the switcher on it, or a review (PLAN §10.1). It runs in the
/// background like any open; a failure shows in the main area's banner, except bare `dv`
/// outside any repository, which opens nothing.
fn open_startup(open: Startup, main: &MainWindow, cx: &mut App) -> Result<()> {
    let shell = main.shell.clone();
    main.handle.update(cx, |_, window, cx| {
        shell.update(cx, |shell, cx| match open {
            Startup::Nothing => {}
            Startup::CurrentRepo(location) => shell
                .open_repo(location, IfNotOpened::Ignore, window, cx)
                .detach(),
            Startup::Repo(location) => shell
                .open_repo(location, IfNotOpened::Banner, window, cx)
                .detach(),
            Startup::Review(location, id) => shell
                .open(location, OpenRequest::Review(id), window, cx)
                .detach(),
        })
    })?;
    Ok(())
}

#[cfg(feature = "automation")]
fn start_automation(main: &MainWindow, pipe: Option<String>, cx: &mut App) -> Result<()> {
    crate::automation::start(main.handle.into(), main.shell.clone(), pipe, cx)
}

#[cfg(not(feature = "automation"))]
fn start_automation(_main: &MainWindow, _pipe: Option<String>, _cx: &mut App) -> Result<()> {
    // `run` refuses `--automation` in this build before the app starts.
    Ok(())
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    #[gpui::test]
    fn a_failed_start_is_reported_and_recorded(cx: &mut TestAppContext) {
        // The detached app has no console: the error must reach `fatal::report` (its message
        // box), not just a log line nobody sees.
        let failed = Cell::new(false);
        let reported = std::cell::RefCell::new(None);
        cx.update(|cx| {
            start_or_quit(
                &failed,
                |error| *reported.borrow_mut() = Some(format!("{error:#}")),
                cx,
                |_| Err(anyhow::anyhow!("no fonts").context("opening the main window")),
            )
        });
        assert!(failed.get());
        assert_eq!(
            reported.into_inner().as_deref(),
            Some("opening the main window: no fonts")
        );
    }

    #[gpui::test]
    fn a_clean_start_records_no_failure(cx: &mut TestAppContext) {
        let failed = Cell::new(false);
        let mut ran = false;
        cx.update(|cx| {
            start_or_quit(
                &failed,
                |error| panic!("nothing to report: {error}"),
                cx,
                |_| {
                    ran = true;
                    Ok(())
                },
            )
        });
        assert!(ran);
        assert!(!failed.get());
    }
}
