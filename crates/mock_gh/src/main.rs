//! `mock_gh`: a fake `gh` for dv's tests. dv runs whatever `DV_GH` names in place of `gh`, and
//! tests point it here, so no test ever talks to GitHub (docs/PLAN.md D7, §14.2).
//!
//! gpui: **never**; `script/check` fails when gpui appears in its dependency tree (PLAN §13.2).
//!
//! Each call:
//! 1. reads stdin only when its arguments ask gh to (an argument `-`, or one ending in `=-` or
//!    `@-`: `--input -`, `--body-file=-`, `-F body=@-`), as the real gh does. Anything else
//!    leaves stdin alone, so a caller that lets it inherit a pipe never has it drained or
//!    waited on;
//! 2. picks the first entry of the scenario file `DV_MOCK_GH` (`scenario.rs`) that matches and
//!    has calls left. Without `DV_MOCK_GH` there are no entries;
//! 3. appends `{"argv", "stdin", "matched"}` to the call log `DV_MOCK_GH_LOG` (`log.rs`), if
//!    set, before printing anything, so a caller that has the answer can already read the log;
//! 4. waits for the entry's `delay_ms`, writes its stdout and stderr and exits with its code; an
//!    unmatched call writes `mock_gh: no scenario entry for argv [...]` to stderr and exits 64.

mod log;
mod scenario;

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crate::log::{CallLog, Record};
use crate::scenario::Scenario;

/// The scenario file to answer from (PLAN §14.2).
const SCENARIO_VAR: &str = "DV_MOCK_GH";
/// Where every call is appended as one JSON line.
const LOG_VAR: &str = "DV_MOCK_GH_LOG";

/// Exit code of a call that no scenario entry answers (`EX_USAGE`).
const UNMATCHED: u8 = 64;
/// Exit code when the scenario file can't be read or is invalid (`EX_DATAERR`).
const BAD_SCENARIO: u8 = 65;
/// Exit code when the call log can't be read or written (`EX_IOERR`).
const LOG_FAILED: u8 = 74;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let stdin = reads_stdin(&argv).then(read_stdin);
    let log = env_path(LOG_VAR).map(CallLog::new);
    let scenario_path = env_path(SCENARIO_VAR);

    let scenario = match &scenario_path {
        None => Ok(Scenario::default()),
        Some(path) => Scenario::load(path),
    };
    let scenario = match scenario {
        Ok(scenario) => scenario,
        Err(err) => {
            // The call is still logged, so a test asserting on the log sees it was made.
            if let Some(log) = &log
                && let Err(log_err) = record(log, &argv, stdin.as_deref(), |_| None)
            {
                eprintln!("mock_gh: {log_err:#}");
            }
            eprintln!("mock_gh: {err:#}");
            return ExitCode::from(BAD_SCENARIO);
        }
    };

    let matched = match &log {
        Some(log) => match record(log, &argv, stdin.as_deref(), |uses| {
            scenario.pick(&argv, stdin.as_deref(), |index| {
                uses.get(&index).copied().unwrap_or(0)
            })
        }) {
            Ok(matched) => matched,
            Err(err) => {
                eprintln!("mock_gh: {err:#}");
                return ExitCode::from(LOG_FAILED);
            }
        },
        None => scenario.pick(&argv, stdin.as_deref(), |_| 0),
    };

    let Some(entry) = matched.and_then(|index| scenario.calls.get(index)) else {
        let source = match &scenario_path {
            Some(path) => format!("the scenario {}", path.display()),
            None => format!("no scenario ({SCENARIO_VAR} is not set)"),
        };
        eprintln!("mock_gh: no scenario entry for argv {argv:?} in {source}");
        return ExitCode::from(UNMATCHED);
    };
    std::thread::sleep(Duration::from_millis(entry.delay_ms));
    // A caller that stopped reading has made up its mind; the exit code still reports the
    // scripted outcome, so write errors are ignored.
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(&entry.stdout_bytes());
    let _ = stdout.flush();
    if let Some(text) = &entry.stderr {
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(text.as_bytes());
        let _ = stderr.flush();
    }
    ExitCode::from(entry.exit)
}

/// Under the log's lock: counts each entry's earlier answers, lets `pick` choose with them, and
/// appends the call with the choice. Returns the choice.
fn record(
    log: &CallLog,
    argv: &[String],
    stdin: Option<&str>,
    pick: impl FnOnce(&std::collections::HashMap<usize, u32>) -> Option<usize>,
) -> anyhow::Result<Option<usize>> {
    let _lock = log.lock()?;
    let matched = pick(&log.uses()?);
    log.append(&Record {
        argv,
        stdin,
        matched,
    })?;
    Ok(matched)
}

/// Whether gh would read stdin for these arguments: `-` as a value (`--input -`), or a value
/// ending in `=-` (`--body-file=-`) or `@-` (`-F body=@-`).
fn reads_stdin(argv: &[String]) -> bool {
    argv.iter()
        .any(|arg| arg == "-" || arg.ends_with("=-") || arg.ends_with("@-"))
}

/// All of stdin as text (lossy UTF-8); a read error is reported and counts as empty input.
fn read_stdin() -> String {
    let mut bytes = Vec::new();
    if let Err(err) = std::io::stdin().lock().read_to_end(&mut bytes) {
        eprintln!("mock_gh: reading stdin: {err}");
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A non-empty path from the environment.
fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn stdin_is_read_only_when_an_argument_names_it() {
        assert!(reads_stdin(&args(
            "api repos/o/r/pulls/1/reviews --input -"
        )));
        assert!(reads_stdin(&args("pr review 1 --body-file=-")));
        assert!(reads_stdin(&args("api graphql -F query=@-")));
        assert!(!reads_stdin(&args("pr view 1 --json number")));
        assert!(!reads_stdin(&args("api graphql -F query=@file.graphql")));
        assert!(!reads_stdin(&[]));
    }
}
