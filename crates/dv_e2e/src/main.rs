//! `e2e`: dv's scenarios (`tests/e2e`, docs/PLAN.md §14) through toolbox-rs's runner, with dv's
//! extensions (`fixture`): the header's `gh` and `lsp`, played by `mock_gh` and `mock_lsp`, and
//! the `gh_log` and `lsp_log` actions that read their logs. toolbox-rs's docs/automation.md
//! documents the runner, the format and the protocol; tests/e2e/README.md dv's own parts.
//!
//! ```text
//! cargo build -p dv -p mock_gh -p mock_lsp
//! cargo run -p dv_e2e --bin e2e -- tests/e2e
//! ```

mod fixture;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use gpui_automation_runner::{Fixture, FixtureCx, Profile, RunnerApp};

use crate::fixture::{DvAction, DvFixture, DvHeader};

/// dv's half of the runner: where `mock_gh` and `mock_lsp` are, when the command line names them.
#[derive(Default)]
struct Dv {
    mock_gh: Option<PathBuf>,
    mock_lsp: Option<PathBuf>,
}

impl RunnerApp for Dv {
    type Header = DvHeader;
    type Action = DvAction;
    const HEADER_KEYS: &'static [&'static str] = &["gh", "lsp"];
    const ACTIONS: &'static [&'static str] = &["gh_log", "lsp_log"];
    const VARS: &'static [&'static str] = fixture::VARS;
    const RESERVED_ENV: &'static [&'static str] = fixture::RESERVED_ENV;

    fn profile(&self) -> Profile {
        Profile {
            manifest_dir: env!("CARGO_MANIFEST_DIR"),
            name: "dv",
            build_hint: "cargo build -p dv -p mock_gh -p mock_lsp",
            /* Every variable dv reads (PLAN §14.1): inherited ones never reach a run, so an
             * owner's DV_WSL_HOME or DV_LSP_<ID> can't either; only a header's env sets them. */
            env_prefix: "DV_",
            data_dir_var: "DV_DATA_DIR",
            /* dv's default window, the size the design boards are compared at (DESIGN §1.5). */
            default_window_size: "1440x900",
            platforms: &["windows", "linux", "macos"],
        }
    }

    fn usage(&self) -> &'static str {
        "\
dv's options:
  --mock-gh <exe>      the fake gh every run's DV_GH points at; scenarios
                       with a \"gh\" header need it built (default:
                       <target>/debug/mock_gh, .exe on Windows)
  --mock-lsp <exe>     the scripted language server a header's lsp files are
                       played by (default: <target>/debug/mock_lsp)
"
    }

    fn option(&mut self, flag: &str, value: &mut dyn FnMut() -> Result<String>) -> Result<bool> {
        let slot = match flag {
            "--mock-gh" => &mut self.mock_gh,
            "--mock-lsp" => &mut self.mock_lsp,
            _ => return Ok(false),
        };
        *slot = Some(absolute(PathBuf::from(value()?)));
        Ok(true)
    }

    fn check(&self, header: &DvHeader, scenario_path: &Path) -> Result<()> {
        header.check(scenario_dir(scenario_path))
    }

    fn fixture(&self, header: &DvHeader, cx: &FixtureCx) -> Result<Box<dyn Fixture<DvAction>>> {
        let helper = |chosen: &Option<PathBuf>, name: &str| {
            chosen.clone().unwrap_or_else(|| {
                cx.target_dir
                    .join("debug")
                    .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
            })
        };
        Ok(Box::new(DvFixture::new(
            header,
            scenario_dir(cx.scenario_path),
            cx.out_dir,
            helper(&self.mock_gh, "mock_gh"),
            helper(&self.mock_lsp, "mock_lsp"),
        )?))
    }
}

/// The folder a scenario file is in: the files its header names are relative to it.
fn scenario_dir(scenario_path: &Path) -> &Path {
    scenario_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

fn main() -> ExitCode {
    gpui_automation_runner::main(Dv::default())
}
