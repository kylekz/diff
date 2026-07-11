#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
  (default)                     working tree vs HEAD";

/// Returns `None` for a bare launch (`dv` with no arguments) — the app opens
/// to the shell's empty state. Any argument seeds an initial review.
fn parse_args() -> Result<Option<(RepoLocation, DiffSource)>, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() {
        return Ok(None);
    }

    let mut location: Option<RepoLocation> = None;
    let mut source = DiffSource::WorkingTree;

    let mut args = raw.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Err(USAGE.to_string()),
            "--wsl" => {
                let value = args.next().ok_or("--wsl requires <distro>:<posix-path>")?;
                location = Some(RepoLocation::from_wsl_arg(&value).map_err(|e| format!("{e:#}"))?);
            }
            "--staged" => source = DiffSource::Staged,
            "--commit" => {
                let value = args.next().ok_or("--commit requires a revision")?;
                source = DiffSource::Commit(value);
            }
            "--range" => {
                let value = args
                    .next()
                    .ok_or("--range requires <a>..<b> or <a>...<b>")?;
                source = parse_range(&value)?;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {other}\n\n{USAGE}"));
            }
            path => {
                if location.is_some() {
                    return Err(format!("unexpected extra argument: {path}\n\n{USAGE}"));
                }
                location = Some(RepoLocation::from_path_arg(path).map_err(|e| format!("{e:#}"))?);
            }
        }
    }

    let location = match location {
        Some(l) => l,
        None => RepoLocation::Local(std::env::current_dir().map_err(|e| e.to_string())?),
    };
    Ok(Some((location, source)))
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
    let seed = match parse_args() {
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

        cx.spawn(async move |cx| {
            let options = WindowOptions {
                titlebar: Some(TitleBar::title_bar_options()),
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(120.), px(120.)),
                    size: size(px(1440.), px(920.)),
                })),
                ..Default::default()
            };

            cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| AppShell::new(seed, window, cx));
                cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
