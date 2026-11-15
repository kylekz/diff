//! dv's one command tree (PLAN §10.1): the GUI's options and `dv open` here, the headless
//! commands and the global flags from `dv_cli`, composed so `dv --help` lists every command.

use std::ffi::OsString;
use std::path::Path;
#[cfg(feature = "gui")]
use std::{fmt, path::PathBuf, str::FromStr};

#[cfg(feature = "gui")]
use clap::ArgGroup;
use clap::{Args, CommandFactory as _, Parser, Subcommand};

/// Review diffs and GitHub PRs locally. `dv` alone opens the app on the current repository;
/// every command but `open` runs without a window.
// A repeated option takes its last value: the e2e runner passes the scenario's `--window-size`
// before the scenario's own `app_args`, which may give another (tests/e2e/README.md, `restart`).
#[derive(Parser, Debug, Clone, PartialEq)]
#[command(
    name = "dv",
    bin_name = "dv",
    version = dv_cli::version_text(),
    args_override_self = true
)]
#[cfg_attr(
    not(feature = "gui"),
    command(
        about = "Review diffs locally. This build of dv has no window: it runs the commands and \
                 `dv backend --stdio`, and the app opens its repositories from Windows.",
        long_about = None
    )
)]
#[cfg_attr(
    feature = "gui",
    command(group(ArgGroup::new("automation_mode").args(["automation", "automation_pipe"]).multiple(true)))
)]
pub struct Cli {
    // The app's options exist only in a build with the app (`gui`); the headless build's
    // `--help` lists the commands alone.
    #[cfg(feature = "gui")]
    /// Where dv keeps its settings [default: %APPDATA%\dv on Windows, the config dir elsewhere;
    /// under automation a fresh temporary folder].
    #[arg(long, env = "DV_DATA_DIR", value_name = "PATH")]
    pub data_dir: Option<PathBuf>,

    #[cfg(feature = "gui")]
    /// Accept automation commands as JSON lines on stdin (PLAN §14, tests/e2e/README.md).
    #[arg(long)]
    pub automation: bool,

    #[cfg(feature = "gui")]
    /// Serve automation on the Windows named pipe `\\.\pipe\<NAME>` instead of stdio, so a
    /// driver can attach to a running app. Implies `--automation`.
    #[arg(long, value_name = "NAME")]
    pub automation_pipe: Option<String>,

    #[cfg(feature = "gui")]
    /// Under automation, open every window unfocused, behind all other windows and out of the
    /// taskbar, so scripted runs never disturb the person using the machine. The e2e runner
    /// passes it unless given `--foreground`. Needs `--automation` or `--automation-pipe`.
    #[arg(long, requires = "automation_mode")]
    pub automation_background: bool,

    #[cfg(feature = "gui")]
    /// Window size in logical pixels, e.g. 1440x900 (minimum 1100x700) [default: 1440x900].
    #[arg(long, value_name = "WxH")]
    pub window_size: Option<WindowSize>,

    #[cfg(feature = "gui")]
    /// Run the app in this process rather than starting it detached: what a detaching `dv`
    /// passes the copy it starts (PLAN §13.1).
    #[arg(long, hide = true)]
    pub no_detach: bool,

    #[command(flatten)]
    pub global: dv_cli::GlobalArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[cfg(feature = "gui")]
impl Cli {
    /// Whether the automation driver runs, over stdio or a named pipe.
    pub fn automation_enabled(&self) -> bool {
        self.automation || self.automation_pipe.is_some()
    }
}

/// The subcommands: `open`, the GUI's, and the headless ones (`dv_cli`).
#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum Command {
    /// Open the app on a repository (the switcher on its changes), with --review on a
    /// review, or on a GitHub pull request's review
    Open(OpenArgs),

    #[command(flatten)]
    Headless(dv_cli::Command),
}

/// `dv open [<path> | <PR>] [--pr <PR>] [--review <id>]`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct OpenArgs {
    /// A path inside a repository's working tree, or \\wsl.localhost\<distro>\<path>
    /// [default: the current directory]; or, when no such path exists, a GitHub pull request
    /// of the repository ("#<n>" quoted, owner/repo#<n> or its URL), which opens as a review
    #[arg(value_name = "PATH|PR")]
    pub target: Option<String>,

    /// Open a GitHub pull request of the repository as a review: its number, #<n>,
    /// owner/repo#<n> or its URL
    #[arg(long, value_name = "PR")]
    pub pr: Option<String>,
}

impl OpenArgs {
    /// The pull request the line names, as written (PLAN §9.3): `--pr`'s value, whose every
    /// form `dv review new --pr` takes too; else the argument when it reads as one (`#482`,
    /// `owner/repo#482` or its URL) and names nothing that `exists` from `cwd`. A path that
    /// is there is always the path, and a bare number is a folder's name, as any other
    /// argument is.
    pub fn pr(&self, cwd: &Path, exists: &dyn Fn(&Path) -> bool) -> Option<&str> {
        if let Some(reference) = &self.pr {
            return Some(reference);
        }
        let target = self.target.as_deref()?;
        dv_core::github::PrRef::parse(target)
            .filter(|reference| !matches!(reference, dv_core::github::PrRef::Number(_)))
            .filter(|_| !exists(&cwd.join(target)))
            .map(|_| target)
    }

    /// The path the line names: the argument, unless it is the pull request
    /// ([`OpenArgs::pr`]).
    pub fn path(&self, cwd: &Path, exists: &dyn Fn(&Path) -> bool) -> Option<&str> {
        match (&self.pr, self.pr(cwd, exists)) {
            (None, Some(_)) => None,
            _ => self.target.as_deref(),
        }
    }
}

/// The command line `args` (the program name first, as parsed) as `dv_cli`'s own tree reads
/// it: without the app's options, which only the app reads, which come before the command's
/// name, and which a build without the app doesn't have. A command run inside a WSL distro
/// passes this line on to the distro's dv (PLAN §10.4). The options are clap's own: the
/// tree's arguments that aren't global.
pub fn headless_args(args: &[OsString]) -> Vec<OsString> {
    let tree = Cli::command();
    let mut out = Vec::with_capacity(args.len());
    let mut rest = args.iter();
    out.extend(rest.next().cloned());
    while let Some(arg) = rest.next() {
        let name = arg
            .to_str()
            .and_then(|text| text.strip_prefix("--"))
            .filter(|name| !name.is_empty());
        let Some(name) = name else {
            // The command's name (or a lone `--`): the rest is the command's.
            out.push(arg.clone());
            out.extend(rest.cloned());
            break;
        };
        let (name, inline) = match name.split_once('=') {
            Some((name, _)) => (name, true),
            None => (name, false),
        };
        let option = tree
            .get_arguments()
            .find(|option| option.get_long() == Some(name));
        let Some(option) = option else {
            out.push(arg.clone());
            continue;
        };
        let value = if !inline && option.get_action().takes_values() {
            rest.next()
        } else {
            None
        };
        if option.is_global_set() {
            out.push(arg.clone());
            out.extend(value.cloned());
        }
    }
    out
}

/// A window size in logical pixels, written `WIDTHxHEIGHT`.
#[cfg(feature = "gui")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowSize {
    pub width: u32,
    pub height: u32,
}

#[cfg(feature = "gui")]
impl WindowSize {
    /// DESIGN §2's reference size.
    pub const DEFAULT: WindowSize = WindowSize {
        width: 1440,
        height: 900,
    };
    /// Below 1100 wide the panel would have to collapse (DESIGN §2).
    pub const MIN: WindowSize = WindowSize {
        width: 1100,
        height: 700,
    };

    /// This size, grown to at least [`Self::MIN`] in each dimension.
    pub fn at_least_min(self) -> Self {
        Self {
            width: self.width.max(Self::MIN.width),
            height: self.height.max(Self::MIN.height),
        }
    }
}

#[cfg(feature = "gui")]
impl fmt::Display for WindowSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

#[cfg(feature = "gui")]
impl FromStr for WindowSize {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (width, height) = value
            .trim()
            .split_once(['x', 'X', '×'])
            .ok_or_else(|| format!("expected WIDTHxHEIGHT, got {value:?}"))?;
        let parse = |part: &str, name: &str| {
            part.trim()
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("{name} must be a positive whole number, got {part:?}"))
        };
        Ok(Self {
            width: parse(width, "width")?,
            height: parse(height, "height")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("dv").chain(args.iter().copied()))
    }

    #[cfg(feature = "gui")]
    #[test]
    fn automation_pipe_implies_automation() {
        assert!(!parse(&[]).unwrap().automation_enabled());
        let cli = parse(&["--automation-pipe", "dv-e2e"]).unwrap();
        assert!(!cli.automation);
        assert!(cli.automation_enabled());
        assert_eq!(cli.automation_pipe.as_deref(), Some("dv-e2e"));
        assert!(parse(&["--automation-pipe"]).is_err(), "needs a name");
        assert!(parse(&["--automation"]).unwrap().automation_enabled());
    }

    #[cfg(feature = "gui")]
    #[test]
    fn all_flags() {
        let cli = parse(&[
            "--data-dir",
            "target/tmp/data",
            "--automation",
            "--window-size",
            "1100x700",
            "--automation-background",
        ])
        .unwrap();
        assert_eq!(cli.data_dir, Some(PathBuf::from("target/tmp/data")));
        assert!(cli.automation);
        assert!(cli.automation_background);
        assert_eq!(
            cli.window_size,
            Some(WindowSize {
                width: 1100,
                height: 700
            })
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn window_size_syntax() {
        let size = |s: &str| s.parse::<WindowSize>();
        assert_eq!(
            size("2560x1440"),
            Ok(WindowSize {
                width: 2560,
                height: 1440
            })
        );
        assert_eq!(
            size(" 800 X 600 ").map(|s| s.to_string()),
            Ok("800x600".into())
        );
        assert_eq!(size("1440×900").map(|s| s.width), Ok(1440));
        assert!(size("1440").is_err());
        assert!(size("0x900").is_err());
        assert!(size("-5x900").is_err());
        assert!(size("wide x tall").is_err());
        assert!(parse(&["--window-size", "big"]).is_err());
    }

    #[cfg(feature = "gui")]
    #[test]
    fn window_size_respects_the_minimum() {
        let small = WindowSize {
            width: 800,
            height: 1000,
        };
        assert_eq!(
            small.at_least_min(),
            WindowSize {
                width: 1100,
                height: 1000
            }
        );
        assert_eq!(WindowSize::DEFAULT.at_least_min(), WindowSize::DEFAULT);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn a_repeated_option_takes_its_last_value() {
        let cli = parse(&["--window-size", "1440x900", "--window-size", "900x600"]).unwrap();
        assert_eq!(
            cli.window_size,
            Some(WindowSize {
                width: 900,
                height: 600
            })
        );
    }

    #[cfg(feature = "gui")]
    #[test]
    fn background_needs_automation() {
        assert!(parse(&["--automation-background"]).is_err());
        assert!(parse(&["--automation", "--automation-background"]).is_ok());
        assert!(parse(&["--automation-pipe", "p", "--automation-background"]).is_ok());
    }

    fn words(line: &[&str]) -> Vec<OsString> {
        line.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_headless_line_is_the_line_without_the_apps_options() {
        let plain = words(&["dv", "--repo", "x", "status", "--json", "--", "--data-dir"]);
        assert_eq!(headless_args(&plain), plain);
        assert_eq!(
            headless_args(&words(&["dv", "--wsl=Ubuntu:/a", "comment", "list"])),
            words(&["dv", "--wsl=Ubuntu:/a", "comment", "list"])
        );
        assert_eq!(headless_args(&words(&["dv"])), words(&["dv"]));
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_apps_options_are_left_out_of_the_headless_line() {
        let line = words(&[
            "dv",
            "--data-dir",
            "D:/data",
            "--automation",
            "--repo",
            "../api",
            "--window-size=1440x900",
            "--author",
            "codex",
            "review",
            "list",
            "--json",
        ]);
        assert!(
            parse(&["--data-dir", "D:/data", "--automation", "review", "list"]).is_ok(),
            "the app's options parse before a headless command"
        );
        assert_eq!(
            headless_args(&line),
            words(&[
                "dv", "--repo", "../api", "--author", "codex", "review", "list", "--json",
            ])
        );
    }

    #[test]
    fn one_tree_holds_open_and_the_headless_commands() {
        let cli = parse(&[]).unwrap();
        assert_eq!(cli.command, None);
        assert_eq!(
            parse(&["open", "../api"]).unwrap().command,
            Some(Command::Open(OpenArgs {
                target: Some("../api".into()),
                pr: None,
            }))
        );
        // A pull request by its flag, alone or beside the repository's path.
        assert_eq!(
            parse(&["open", "--pr", "482"]).unwrap().command,
            Some(Command::Open(OpenArgs {
                target: None,
                pr: Some("482".into()),
            }))
        );
        assert_eq!(
            parse(&["open", "../api", "--pr", "#482"]).unwrap().command,
            Some(Command::Open(OpenArgs {
                target: Some("../api".into()),
                pr: Some("#482".into()),
            }))
        );
        let cli = parse(&["backend", "--stdio"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Headless(dv_cli::Command::Backend(
                dv_cli::args::BackendArgs { stdio: true }
            )))
        );

        let cli = parse(&["status", "--json"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Headless(dv_cli::Command::Status))
        );
        assert!(cli.global.json);
        assert!(matches!(
            parse(&["review", "list", "--all"]).unwrap().command,
            Some(Command::Headless(dv_cli::Command::Review(
                dv_cli::args::ReviewCommand::List(_)
            )))
        ));
    }

    #[cfg(not(feature = "gui"))]
    #[test]
    fn a_build_without_the_app_has_none_of_its_options() {
        for flag in [
            "--automation",
            "--no-detach",
            "--automation-background",
            "--window-size=1440x900",
            "--data-dir=x",
        ] {
            assert!(parse(&[flag]).is_err(), "{flag}");
        }
    }

    #[cfg(feature = "gui")]
    #[test]
    fn the_apps_options_belong_to_the_app() {
        assert!(!parse(&[]).unwrap().no_detach);

        // The e2e runner's line: the app's flags first, then the scenario's `open`.
        let cli = parse(&[
            "--automation",
            "--automation-background",
            "--data-dir",
            "D:/data",
            "--window-size",
            "1440x900",
            "open",
            "repos/api-server",
            "--review",
            "r-01k6zzzzzzzzzzzzzzzzzzzzzz",
        ])
        .unwrap();
        assert!(cli.automation_enabled());
        assert_eq!(
            cli.command,
            Some(Command::Open(OpenArgs {
                target: Some("repos/api-server".into()),
                pr: None,
            }))
        );
        assert_eq!(
            cli.global.review.as_deref(),
            Some("r-01k6zzzzzzzzzzzzzzzzzzzzzz")
        );
        assert_eq!(
            parse(&["--no-detach", "open"]).unwrap().command,
            Some(Command::Open(OpenArgs {
                target: None,
                pr: None,
            }))
        );

        // The app's options belong to the app, not to the headless commands.
        assert!(parse(&["status", "--automation"]).is_err());
        assert!(parse(&["review", "list", "--window-size", "1440x900"]).is_err());
    }

    #[test]
    fn the_tree_is_consistent() {
        use clap::CommandFactory as _;
        Cli::command().debug_assert();
    }
}
