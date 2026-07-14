#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// `Workspace::automation_state`'s single `json!` call has grown large enough
// (Phase 7 D4 added two more fields) to blow past `serde_json`'s default
// macro recursion limit of 128 nested `json_internal!` expansions. Bumping
// the limit is the standard fix for this exact `json!` failure mode — the
// macro itself isn't recursive at runtime, only in how far rustc unrolls it
// at compile time.
#![recursion_limit = "256"]

mod author;
#[cfg(feature = "automation")]
mod automation;
mod cli;
mod fuzzy;
mod highlight;
mod pr;
mod recent;
mod settings;
mod shell;
mod submit;
mod themes;
mod workspace;

use std::borrow::Cow;

use dv_core::{DiffSource, GitRepo, RepoLocation, RepoSlug};
use gpui::*;
use gpui_component::{Root, TitleBar};

use crate::settings::Settings;
use crate::shell::AppShell;

// Two cfg-gated copies rather than one string with the automation lines
// spliced in at runtime: `--automation` itself only parses under the
// `automation` feature (see the parse arms below), so a `--no-default-features`
// build's own `--help`/usage text must not advertise a flag it then rejects.
#[cfg(feature = "automation")]
const USAGE: &str = "\
usage: dv [<repo-path>] [options]

  <repo-path>          local path or \\\\wsl.localhost\\<distro>\\<path> (default: .)

options:
  --wsl <distro>:<posix-path>   open a repo inside a WSL distro
  --staged                      diff index vs HEAD
  --commit <rev>                diff one commit against its parent
  --range <a>..<b> | <a>...<b>  diff two revisions (... = merge base)
  --automation                  JSON-over-stdio control channel for agents
  (default)                     working tree vs HEAD

usage: dv pr <number|url> [--repo <path>|--wsl <distro>:<posix-path>] [--automation]

  <number>             a bare PR number, opened against <repo-path>/cwd
  <url>                a PR URL — must match the repo's origin remote";

#[cfg(not(feature = "automation"))]
const USAGE: &str = "\
usage: dv [<repo-path>] [options]

  <repo-path>          local path or \\\\wsl.localhost\\<distro>\\<path> (default: .)

options:
  --wsl <distro>:<posix-path>   open a repo inside a WSL distro
  --staged                      diff index vs HEAD
  --commit <rev>                diff one commit against its parent
  --range <a>..<b> | <a>...<b>  diff two revisions (... = merge base)
  (default)                     working tree vs HEAD

usage: dv pr <number|url> [--repo <path>|--wsl <distro>:<posix-path>]

  <number>             a bare PR number, opened against <repo-path>/cwd
  <url>                a PR URL — must match the repo's origin remote";

struct Cli {
    /// `None` for a bare launch — the app opens to the shell's empty state.
    seed: Option<(RepoLocation, DiffSource)>,
    automation: bool,
    /// Set only by `dv pr <number|url>` — the workspace opens this PR as
    /// soon as its initial repo load lands (docs/phase-3-github.md
    /// deliverable 1).
    pending_pr: Option<u64>,
}

fn parse_args() -> Result<Cli, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();

    let mut location: Option<RepoLocation> = None;
    let mut source = DiffSource::WorkingTree;
    // Only mutated by the `"--automation"` arm below, which is itself
    // gated out under `--no-default-features` (falls through to the
    // unknown-option error instead) — `mut` goes unused in that build.
    #[cfg_attr(not(feature = "automation"), allow(unused_mut))]
    let mut automation = false;
    // `--automation` alone must behave like a bare launch, so track whether
    // any argument actually described a repo/diff.
    let mut seen_repo_arg = false;

    let mut args = raw.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Err(USAGE.to_string()),
            #[cfg(feature = "automation")]
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
    Ok(Cli {
        seed,
        automation,
        pending_pr: None,
    })
}

/// The first non-flag token in `dv pr <...>` (everything after `"pr"`),
/// skipping `--repo <path>` / `--wsl <spec>` / `--json` exactly like
/// `cli::extract_location_globals` does — so `dv pr --repo X list` and
/// `dv pr list --repo X` both see `"list"` here, matching whatever
/// `cli::run` will itself dispatch on. `None` means every token was
/// consumed as a flag (or there were none): `dv pr` alone stays headless so
/// `cli::run` prints its own "missing subcommand" usage error.
fn pr_first_positional(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--repo" | "--wsl" => {
                iter.next();
            }
            "--json" => {}
            other => return Some(other),
        }
    }
    None
}

/// `dv pr <target>`'s failure modes, split by exit code the same way
/// `cli::CliError` splits `Usage`/`Op`: a malformed target (not a number,
/// not a recognizable PR URL) is a parse error (exit 2, printed before
/// touching any repo); a target that parsed fine but doesn't match this
/// repo is caught only once a repo is actually opened (exit 1).
#[derive(Debug)]
enum PrArgError {
    Parse(String),
    Mismatch(String),
}

/// `dv pr <number|url> [--repo <path>|--wsl <distro>:<posix-path>]
/// [--automation]` — the GUI launch shape (`args` is everything after
/// `"pr"`, already established by the caller not to be one of the headless
/// subcommands). `--json` is accepted and silently ignored: it's a global
/// flag on every other `pr`/`review`/`comment` invocation, and erroring on
/// a flag that simply has nothing to do once a window opens would be
/// needlessly hostile to a copy-pasted command line.
fn parse_pr_gui_args(args: &[String]) -> Result<Cli, PrArgError> {
    let mut location: Option<RepoLocation> = None;
    let mut target: Option<&str> = None;
    // Only mutated by the `"--automation"` arm below, which is itself
    // gated out under `--no-default-features` (falls through to the
    // unknown-option error instead) — `mut` goes unused in that build.
    #[cfg_attr(not(feature = "automation"), allow(unused_mut))]
    let mut automation = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--repo" => {
                let value = iter
                    .next()
                    .ok_or_else(|| PrArgError::Parse("--repo requires a path".to_string()))?;
                location = Some(
                    RepoLocation::from_path_arg(value)
                        .map_err(|e| PrArgError::Parse(format!("{e:#}")))?,
                );
            }
            "--wsl" => {
                let value = iter.next().ok_or_else(|| {
                    PrArgError::Parse("--wsl requires <distro>:<posix-path>".to_string())
                })?;
                location = Some(
                    RepoLocation::from_wsl_arg(value)
                        .map_err(|e| PrArgError::Parse(format!("{e:#}")))?,
                );
            }
            #[cfg(feature = "automation")]
            "--automation" => automation = true,
            "--json" => {}
            other if other.starts_with('-') => {
                return Err(PrArgError::Parse(format!("unknown option: {other}")));
            }
            other => {
                if target.is_some() {
                    return Err(PrArgError::Parse(format!(
                        "unexpected extra argument: {other}"
                    )));
                }
                target = Some(other);
            }
        }
    }

    let target = target
        .ok_or_else(|| PrArgError::Parse("dv pr requires a <number> or a PR URL".to_string()))?;
    let location = match location {
        Some(l) => l,
        None => RepoLocation::Local(
            std::env::current_dir()
                .map_err(|e| PrArgError::Mismatch(format!("cannot read current directory: {e}")))?,
        ),
    };

    let number = resolve_pr_target(target, &location)?;
    Ok(Cli {
        seed: Some((location, DiffSource::WorkingTree)),
        automation,
        pending_pr: Some(number),
    })
}

/// A bare `<number>` needs no repo context at all. A PR URL is parsed and
/// then REQUIRED to match `location`'s `origin` remote (docs/phase-3-
/// github.md deliverable 1) — opening some other repo's PR #3 from a
/// checkout of a *different* repo would silently diff the wrong thing.
fn resolve_pr_target(target: &str, location: &RepoLocation) -> Result<u64, PrArgError> {
    if let Ok(number) = target.parse::<u64>() {
        return Ok(number);
    }

    let url = parse_pr_url(target).map_err(PrArgError::Parse)?;

    let repo =
        GitRepo::open(location.clone()).map_err(|e| PrArgError::Mismatch(format!("{e:#}")))?;
    let origin = repo
        .remote_url("origin")
        .map_err(|e| PrArgError::Mismatch(format!("could not read origin remote: {e:#}")))?;
    let origin_slug =
        RepoSlug::parse_remote_url(&origin).map_err(|e| PrArgError::Mismatch(format!("{e}")))?;

    let matches = origin_slug.host.eq_ignore_ascii_case(&url.host)
        && origin_slug.owner.eq_ignore_ascii_case(&url.owner)
        && origin_slug.repo.eq_ignore_ascii_case(&url.repo);
    if !matches {
        return Err(PrArgError::Mismatch(format!(
            "PR URL {target} points at {}/{}/{} but this repo's origin is {origin_slug} — \
             open it with --repo/--wsl pointed at that checkout, or run `dv pr {}` (bare \
             number) from within it",
            url.host, url.owner, url.repo, url.number
        )));
    }
    Ok(url.number)
}

struct PrUrlTarget {
    host: String,
    owner: String,
    repo: String,
    number: u64,
}

/// Parse `https://<host>/<owner>/<repo>/pull/<number>` — any trailing path
/// segment or query string after the number (`/files`, `?diff=split`, ...)
/// is ignored.
fn parse_pr_url(url: &str) -> Result<PrUrlTarget, String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| format!("PR URL must start with http(s)://: {url}"))?;
    let mut parts = rest.split('/');
    let host = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("could not parse a host from: {url}"))?;
    let owner = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("could not parse an owner from: {url}"))?;
    let repo = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("could not parse a repo from: {url}"))?;
    if parts.next() != Some("pull") {
        return Err(format!("PR URL must contain /pull/<number>: {url}"));
    }
    let number_field = parts
        .next()
        .ok_or_else(|| format!("PR URL is missing a PR number: {url}"))?;
    let number_str = number_field
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(number_field);
    let number = number_str
        .parse::<u64>()
        .map_err(|_| format!("could not parse a PR number from: {url}"))?;
    Ok(PrUrlTarget {
        host: host.to_lowercase(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    })
}

/// `pub(crate)` so `cli.rs`'s `review create --range` can reuse the exact
/// same `a..b` / `a...b` parsing instead of drifting a second copy.
pub(crate) fn parse_range(value: &str) -> Result<DiffSource, String> {
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

/// Embeds the bundled JetBrains Mono TTFs and registers them with gpui's
/// text system so `mono_font_family: "JetBrains Mono"` (set uniformly by
/// `themes::apply_theme`) resolves even on a machine that doesn't have the
/// font installed system-wide. Must run before the first theme is applied —
/// once something has rendered mono text with a *missing* font, gpui has
/// already picked a fallback for that font id.
///
/// `-Regular`/`-Bold`/`-Italic`/`-BoldItalic` cover every style the app
/// actually uses (diff/code text is never anything more exotic than bold or
/// italic); the many other static weights JetBrains ships aren't bundled.
fn register_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/jetbrains-mono/JetBrainsMono-Bold.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/jetbrains-mono/JetBrainsMono-Italic.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/jetbrains-mono/JetBrainsMono-BoldItalic.ttf"
        )),
    ];
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        // Not fatal: the app still runs, just falls back to whatever
        // `mono_font_family` resolves to on this machine (or the platform
        // default monospace font if JetBrains Mono isn't installed either).
        eprintln!("warning: failed to register bundled JetBrains Mono fonts: {err:#}");
    }
}

fn main() {
    // Pure headless path: `dv review ...` / `dv comment ...` / `dv pr
    // <list|view|create|fetch> ...` are the agent-facing CLI
    // (docs/phase-2-review-layer.md § Agent CLI, docs/phase-3-github.md)
    // and must never touch gpui — no window, no platform app, no theme
    // init. Handled before anything else in `main` so a CI/agent
    // invocation never pays for (or risks failing on) GPUI startup.
    //
    // `dv pr <number|url>` is the one exception under the `pr` subcommand:
    // it's a GUI launch (open the app with a pending PR-open), not a
    // headless query — distinguished from the four real `pr` subcommands
    // by `pr_first_positional` before anything commits to either path.
    let raw_args: Vec<String> = std::env::args().collect();
    if let Some(sub) = raw_args.get(1) {
        if sub == "review" || sub == "comment" {
            let code = cli::run(&raw_args[1..]);
            std::process::exit(code);
        }
        if sub == "pr" {
            let rest = &raw_args[2..];
            let headless = matches!(
                pr_first_positional(rest),
                None | Some("list") | Some("view") | Some("create") | Some("fetch")
            );
            if headless {
                let code = cli::run(&raw_args[1..]);
                std::process::exit(code);
            }
            match parse_pr_gui_args(rest) {
                Ok(cli) => {
                    run_gui(cli);
                    return;
                }
                Err(PrArgError::Parse(message)) => {
                    eprintln!("{message}\n\n{USAGE}");
                    std::process::exit(2);
                }
                Err(PrArgError::Mismatch(message)) => {
                    eprintln!("{message}");
                    std::process::exit(1);
                }
            }
        }
    }

    let cli = match parse_args() {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    run_gui(cli);
}

/// The GPUI launch path, shared by a plain `dv [<repo-path>]` invocation and
/// `dv pr <number|url>`'s GUI launch.
fn run_gui(cli: Cli) {
    // Route WSL repo operations through a persistent `dv-host` process when
    // one is available (docs/phase-5-implementation-plan.md §3). The
    // headless CLI dispatch in `main` above returns before this function is
    // ever reached, so `dv review`/`dv comment`/`dv pr <list|view|create|
    // fetch>` stay on today's per-command `wsl.exe` spawns untouched.
    dv_core::remote::enable_hosts();

    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(move |cx| {
        gpui_component::init(cx);
        workspace::init(cx);
        shell::init(cx);
        register_fonts(cx);
        // Persisted-or-default theme (settings.json missing/corrupt falls
        // back to `themes::DEFAULT_THEME` silently — see `Settings::load`).
        // Applied before the window opens so `Root`'s first render already
        // sees the right theme (see the `Root::new` call below for why it
        // gets no explicit `.bg()` of its own). `follow_os_appearance`
        // resolves right here too — `cx.window_appearance()` is the
        // platform's live light/dark state, readable at the `App` level
        // before any window exists yet (docs/phase-4-settings-and-theming.md
        // deliverable 2). Live changes while the app is running are handled
        // separately, by `AppShell`'s own `Window::observe_window_appearance`
        // subscription (see `shell.rs`'s `AppShell::new`) — that one can't
        // cover startup itself (no window yet to observe from).
        let settings = Settings::load();
        let os_is_dark = matches!(
            cx.window_appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        );
        let theme_name = settings.effective_theme(os_is_dark).to_string();
        themes::apply_theme(&theme_name, &settings.mono_font, None, cx);

        let Cli {
            seed,
            automation,
            pending_pr,
        } = cli;
        cx.spawn(async move |cx| {
            let options = WindowOptions {
                titlebar: Some(TitleBar::title_bar_options()),
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(120.), px(120.)),
                    size: size(px(1440.), px(920.)),
                })),
                ..Default::default()
            };

            // Only read back under the `automation` feature (to hand the
            // freshly built shell to `automation::start` below) — allow the
            // otherwise-unused write/binding under `--no-default-features`
            // rather than cfg-splitting the closure body itself.
            #[cfg_attr(
                not(feature = "automation"),
                allow(unused_variables, unused_assignments)
            )]
            let mut shell_slot = None;
            #[cfg_attr(not(feature = "automation"), allow(unused_variables))]
            let window = cx
                .open_window(options, |window, cx| {
                    let shell = cx.new(|cx| {
                        AppShell::new(seed, automation, pending_pr, settings, window, cx)
                    });
                    shell_slot = Some(shell.clone());
                    // No `.bg(...)` override here: `Root::render()` already
                    // paints `cx.theme().tokens.background` fresh every
                    // frame, before applying `self.style` on top via
                    // `refine_style`. An explicit `.bg()` at construction
                    // time would set that `self.style`'s background once
                    // and have it win on every subsequent render — freezing
                    // the window at whichever theme was active at startup
                    // and breaking the live theme picker (found by looking
                    // at an actual Claude Light screenshot: the chrome had
                    // switched but the diff pane was still compositing
                    // Aura Dark's frozen root background under the new
                    // theme's translucent row tints).
                    cx.new(|cx| Root::new(shell, window, cx))
                })
                .expect("failed to open window");

            #[cfg(feature = "automation")]
            if automation {
                let shell = shell_slot.expect("window builder ran");
                cx.update(|cx| automation::start(window, shell, cx));
            }
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    // Deliberately NOT `use super::*;`: that also re-globs `gpui::*`
    // (imported at this file's top level) into this module — which blows
    // the compiler's macro-expansion recursion limit once any `#[test]` fn
    // needs expanding here (main.rs is the only module in the crate with
    // both a `gpui::*` glob import AND its own `#[cfg(test)] mod tests`;
    // every other module either has one or the other). Narrow, explicit
    // imports sidestep it entirely.
    use super::{
        PrArgError, parse_pr_gui_args, parse_pr_url, pr_first_positional, resolve_pr_target,
    };
    use dv_core::RepoLocation;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // --- pr_first_positional: the headless-vs-GUI router --------------

    #[test]
    fn pr_first_positional_finds_bare_subcommand() {
        assert_eq!(pr_first_positional(&args(&["list"])), Some("list"));
        assert_eq!(pr_first_positional(&args(&["view", "5"])), Some("view"));
    }

    #[test]
    fn pr_first_positional_skips_leading_flags() {
        let a = args(&["--repo", "D:/x", "list"]);
        assert_eq!(pr_first_positional(&a), Some("list"));
        let b = args(&["--json", "--wsl", "Ubuntu:/x", "fetch", "5"]);
        assert_eq!(pr_first_positional(&b), Some("fetch"));
    }

    #[test]
    fn pr_first_positional_sees_a_bare_number_as_not_headless() {
        assert_eq!(pr_first_positional(&args(&["123"])), Some("123"));
        assert_eq!(
            pr_first_positional(&args(&["--repo", "D:/x", "123"])),
            Some("123")
        );
    }

    #[test]
    fn pr_first_positional_sees_a_url_as_not_headless() {
        let a = args(&["https://github.com/o/r/pull/9"]);
        assert_eq!(
            pr_first_positional(&a),
            Some("https://github.com/o/r/pull/9")
        );
    }

    #[test]
    fn pr_first_positional_none_when_only_flags_or_empty() {
        assert_eq!(pr_first_positional(&args(&[])), None);
        assert_eq!(pr_first_positional(&args(&["--json"])), None);
    }

    #[test]
    fn dv_pr_headless_subcommands_are_recognized() {
        for sub in ["list", "view", "create", "fetch"] {
            let rest = args(&[sub]);
            let headless = matches!(
                pr_first_positional(&rest),
                None | Some("list") | Some("view") | Some("create") | Some("fetch")
            );
            assert!(headless, "{sub} should stay on the headless CLI path");
        }
        for target in ["123", "https://github.com/o/r/pull/9"] {
            let rest = args(&[target]);
            let headless = matches!(
                pr_first_positional(&rest),
                None | Some("list") | Some("view") | Some("create") | Some("fetch")
            );
            assert!(!headless, "{target} should route to the GUI launch path");
        }
    }

    // --- parse_pr_url ---------------------------------------------------

    #[test]
    fn parse_pr_url_basic() {
        let url = parse_pr_url("https://github.com/kylekz/difftest/pull/9").unwrap();
        assert_eq!(url.host, "github.com");
        assert_eq!(url.owner, "kylekz");
        assert_eq!(url.repo, "difftest");
        assert_eq!(url.number, 9);
    }

    #[test]
    fn parse_pr_url_lowercases_host_only() {
        let url = parse_pr_url("https://GitHub.com/Owner/Repo/pull/3").unwrap();
        assert_eq!(url.host, "github.com");
        // Owner/repo casing is preserved — matched case-insensitively later.
        assert_eq!(url.owner, "Owner");
        assert_eq!(url.repo, "Repo");
    }

    #[test]
    fn parse_pr_url_ignores_trailing_path_and_query() {
        let url = parse_pr_url("https://github.com/o/r/pull/42/files").unwrap();
        assert_eq!(url.number, 42);
        let url = parse_pr_url("https://github.com/o/r/pull/42?diff=split").unwrap();
        assert_eq!(url.number, 42);
    }

    #[test]
    fn parse_pr_url_rejects_non_pull_paths() {
        assert!(parse_pr_url("https://github.com/o/r/issues/9").is_err());
        assert!(parse_pr_url("https://github.com/o/r").is_err());
        assert!(parse_pr_url("not a url at all").is_err());
        assert!(parse_pr_url("https://github.com/o/r/pull/notanumber").is_err());
    }

    // --- resolve_pr_target: the bare-number fast path -------------------

    #[test]
    fn resolve_pr_target_bare_number_skips_repo_entirely() {
        // A nonsense location: if the number path touched the repo at all,
        // this would error (no such directory / not a git repo).
        let bogus = RepoLocation::Local("Z:/definitely/does/not/exist".into());
        assert_eq!(resolve_pr_target("123", &bogus).unwrap(), 123);
    }

    #[test]
    fn resolve_pr_target_url_requires_a_real_repo() {
        let bogus = RepoLocation::Local("Z:/definitely/does/not/exist".into());
        let err = resolve_pr_target("https://github.com/o/r/pull/9", &bogus).unwrap_err();
        assert!(matches!(err, PrArgError::Mismatch(_)));
    }

    // --- parse_pr_gui_args ----------------------------------------------

    #[test]
    fn parse_pr_gui_args_bare_number_and_repo_flag() {
        let a = args(&["123", "--repo", "."]);
        let cli = parse_pr_gui_args(&a).expect("should parse");
        assert_eq!(cli.pending_pr, Some(123));
        assert!(!cli.automation);
        assert!(cli.seed.is_some());
    }

    #[cfg(feature = "automation")]
    #[test]
    fn parse_pr_gui_args_honors_automation_flag() {
        let a = args(&["123", "--automation"]);
        let cli = parse_pr_gui_args(&a).expect("should parse");
        assert!(cli.automation);
    }

    // Mirrors `parse_pr_gui_args_honors_automation_flag` but for the
    // `--no-default-features` build: `--automation` must not exist as a
    // known flag here either, matching `parse_args`'s top-level gate.
    #[cfg(not(feature = "automation"))]
    #[test]
    fn parse_pr_gui_args_rejects_automation_flag_when_feature_disabled() {
        let a = args(&["123", "--automation"]);
        assert!(matches!(parse_pr_gui_args(&a), Err(PrArgError::Parse(_))));
    }

    #[test]
    fn parse_pr_gui_args_requires_a_target() {
        let a = args(&["--repo", "."]);
        assert!(matches!(parse_pr_gui_args(&a), Err(PrArgError::Parse(_))));
    }

    #[test]
    fn parse_pr_gui_args_rejects_two_targets() {
        let a = args(&["123", "456"]);
        assert!(matches!(parse_pr_gui_args(&a), Err(PrArgError::Parse(_))));
    }
}
