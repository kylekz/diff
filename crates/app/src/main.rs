#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod automation;
mod highlight;
mod recent;
mod shell;
mod workspace;

use dv_core::{DiffSource, RepoLocation};
use gpui::*;
use gpui_component::{ActiveTheme as _, Root, TitleBar};

use crate::shell::AppShell;

const USAGE: &str = "\
usage: dv [<repo-path>] [options]

  <repo-path>          local path or \\\\wsl.localhost\\<distro>\\<path> (default: .)

options:
  --wsl <distro>:<posix-path>   open a repo inside a WSL distro
  --staged                      diff index vs HEAD
  --commit <rev>                diff one commit against its parent
  --range <a>..<b> | <a>...<b>  diff two revisions (... = merge base)
  --automation                  JSON-over-stdio control channel for agents
  (default)                     working tree vs HEAD";

struct Cli {
    /// `None` for a bare launch — the app opens to the shell's empty state.
    seed: Option<(RepoLocation, DiffSource)>,
    automation: bool,
}

fn parse_args() -> Result<Cli, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();

    let mut location: Option<RepoLocation> = None;
    let mut source = DiffSource::WorkingTree;
    let mut automation = false;
    // `--automation` alone must behave like a bare launch, so track whether
    // any argument actually described a repo/diff.
    let mut seen_repo_arg = false;

    let mut args = raw.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Err(USAGE.to_string()),
            "--automation" => automation = true,
            "--wsl" => {
                let value = args.next().ok_or("--wsl requires <distro>:<posix-path>")?;
                location = Some(RepoLocation::from_wsl_arg(&value).map_err(|e| format!("{e:#}"))?);
                seen_repo_arg = true;
            }
            "--staged" => {
                source = DiffSource::Staged;
                seen_repo_arg = true;
            }
            "--commit" => {
                let value = args.next().ok_or("--commit requires a revision")?;
                source = DiffSource::Commit(value);
                seen_repo_arg = true;
            }
            "--range" => {
                let value = args
                    .next()
                    .ok_or("--range requires <a>..<b> or <a>...<b>")?;
                source = parse_range(&value)?;
                seen_repo_arg = true;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {other}\n\n{USAGE}"));
            }
            path => {
                if location.is_some() {
                    return Err(format!("unexpected extra argument: {path}\n\n{USAGE}"));
                }
                location = Some(RepoLocation::from_path_arg(path).map_err(|e| format!("{e:#}"))?);
                seen_repo_arg = true;
            }
        }
    }

    let seed = if seen_repo_arg {
        let location = match location {
            Some(l) => l,
            None => RepoLocation::Local(std::env::current_dir().map_err(|e| e.to_string())?),
        };
        Some((location, source))
    } else {
        None
    };
    Ok(Cli { seed, automation })
}

fn parse_range(value: &str) -> Result<DiffSource, String> {
    let (base, head, merge_base) = if let Some((b, h)) = value.split_once("...") {
        (b, h, true)
    } else if let Some((b, h)) = value.split_once("..") {
        (b, h, false)
    } else {
        return Err(format!(
            "--range expects <a>..<b> or <a>...<b>, got: {value}"
        ));
    };
    if base.is_empty() || head.is_empty() {
        return Err(format!("--range endpoints must be non-empty: {value}"));
    }
    Ok(DiffSource::Range {
        base: base.to_string(),
        head: head.to_string(),
        merge_base,
    })
}

fn apply_aura_theme(cx: &mut App) {
    use gpui_component::{Theme, ThemeConfig, ThemeMode};

    let config: ThemeConfig = serde_json::from_str(include_str!("../../../themes/aura-dark.json"))
        .expect("themes/aura-dark.json must parse as a gpui-component ThemeConfig");
    Theme::change(ThemeMode::Dark, None, cx);
    Theme::global_mut(cx).apply_config(&std::rc::Rc::new(config));
}

fn main() {
    let cli = match parse_args() {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(move |cx| {
        gpui_component::init(cx);
        workspace::init(cx);
        shell::init(cx);
        apply_aura_theme(cx);

        let Cli { seed, automation } = cli;
        cx.spawn(async move |cx| {
            let options = WindowOptions {
                titlebar: Some(TitleBar::title_bar_options()),
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(120.), px(120.)),
                    size: size(px(1440.), px(920.)),
                })),
                ..Default::default()
            };

            let mut shell_slot = None;
            let window = cx
                .open_window(options, |window, cx| {
                    let shell = cx.new(|cx| AppShell::new(seed, automation, window, cx));
                    shell_slot = Some(shell.clone());
                    cx.new(|cx| Root::new(shell, window, cx).bg(cx.theme().background))
                })
                .expect("failed to open window");

            if automation {
                let shell = shell_slot.expect("window builder ran");
                cx.update(|cx| automation::start(window, shell, cx));
            }
        })
        .detach();
    });
}
