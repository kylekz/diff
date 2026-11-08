//! dv's scenario extensions (tests/e2e/README.md): a header's `gh` file, which `mock_gh` answers
//! dv's `gh` calls from, and its `lsp` files, which `mock_lsp` plays for each language server;
//! the `gh_log` and `lsp_log` actions read what they logged.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use gpui_automation_runner::Fixture;
use serde::Deserialize;
use serde_json::{Value, json};

/// What dv runs in place of `gh` (PLAN D7). Every launch points it at `mock_gh`, so no scenario
/// can reach GitHub.
pub const GH_VAR: &str = "DV_GH";

/// The `mock_gh` scenario file (PLAN §14.2): the header's `gh`, unset without one.
pub const MOCK_GH_VAR: &str = "DV_MOCK_GH";

/// Where `mock_gh` appends one JSON line per call; `gh_log` steps read it.
pub const MOCK_GH_LOG_VAR: &str = "DV_MOCK_GH_LOG";

/**
 * git stops looking for a repository at these directories. Every launch sets it to the
 * scenario's output dir: the data dir (the app's working directory) sits inside this checkout's
 * `target/`, and a repository lookup from there must never find dv's own checkout.
 */
pub const GIT_CEILING_VAR: &str = "GIT_CEILING_DIRECTORIES";

/**
 * What dv runs in place of a language server's resolvers (PLAN §18.4): `DV_LSP_<ID>`, with
 * `DV_LSP_<ID>_ARGS`. The runner removes every inherited `DV_` variable, so a scenario reaches
 * no real language server unless its header's `lsp` (or `env`) names one.
 */
const LSP_VAR_PREFIX: &str = "DV_LSP_";

/// The file in a scenario's output dir that `DV_MOCK_GH_LOG` names.
pub const GH_LOG_FILE: &str = "gh_calls.jsonl";

/// The file in a scenario's output dir every `mock_lsp` the scenario starts logs to.
pub const LSP_LOG_FILE: &str = "lsp_messages.jsonl";

/// The `${...}` variables the fixture defines; `dv_mock_gh` only with a header's `gh`.
pub const VARS: &[&str] = &[
    "dv_gh",
    "dv_mock_gh",
    "dv_mock_gh_log",
    "dv_mock_lsp",
    "dv_mock_lsp_log",
];

/// The environment the fixture sets, which a header's `env` may not.
pub const RESERVED_ENV: &[&str] = &[GH_VAR, MOCK_GH_VAR, MOCK_GH_LOG_VAR, GIT_CEILING_VAR];

/// dv's header keys: `{"scenario": {"gh": "fixtures/gh/pr.json", "lsp": {"typescript": …}}}`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DvHeader {
    /// A `mock_gh` scenario file, relative to the scenario file: `DV_MOCK_GH` names it, so
    /// `mock_gh` answers the app's `gh` calls from it.
    gh: Option<String>,
    /**
     * `mock_lsp` scenario files by language server id (`{"typescript":
     * "fixtures/lsp/hover.json"}`), relative to the scenario file: `DV_LSP_<ID>` points at
     * `mock_lsp` playing that file, logging to `${dv_mock_lsp_log}`.
     */
    #[serde(default)]
    lsp: BTreeMap<String, String>,
}

impl DvHeader {
    /// The load-time checks: the files the header names exist next to the scenario (`dir`).
    pub fn check(&self, dir: &Path) -> Result<()> {
        if let Some(gh) = &self.gh {
            if gh.trim().is_empty() {
                bail!("the scenario header's gh must name a mock_gh scenario file");
            }
            let path = dir.join(gh);
            if !path.is_file() {
                bail!(
                    "the scenario's gh file {} does not exist (header \"gh\" is relative to the \
                     scenario file)",
                    path.display()
                );
            }
        }
        for (id, file) in &self.lsp {
            if file.trim().is_empty()
                || id.is_empty()
                || !id
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
            {
                bail!(
                    "the scenario header's lsp maps a language server id (lower case) to a \
                     mock_lsp scenario file; {id:?} doesn't"
                );
            }
            let path = dir.join(file);
            if !path.is_file() {
                bail!(
                    "the scenario's lsp file for {id}, {}, does not exist (header lsp files are \
                     relative to the scenario file)",
                    path.display()
                );
            }
        }
        Ok(())
    }
}

/// dv's runner actions, each run in the runner's process.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DvAction {
    /// `{"gh_log": {}}`: the calls `mock_gh` has logged so far in the scenario.
    GhLog {},
    /**
     * `{"lsp_log": {"wait_ms"?: n}}`: what every `mock_lsp` the scenario started has logged so
     * far (the messages it read, its starts and crashes). With `wait_ms`, the log is read again
     * until the line's `expect_result` holds, for that long at most: a message the app sends
     * off its UI thread, which no `state` shows.
     */
    LspLog { wait_ms: Option<u64> },
}

/// One scenario's `mock_gh` and `mock_lsp`, the same for every launch of its app.
#[derive(Debug)]
pub struct DvFixture {
    mock_gh: PathBuf,
    mock_lsp: PathBuf,
    /// The header's `gh` file, for `DV_MOCK_GH`.
    gh_scenario: Option<PathBuf>,
    /// `DV_MOCK_GH_LOG`: `<out_dir>/gh_calls.jsonl`, shared by every launch.
    gh_log: PathBuf,
    /// The header's `lsp` files by language server id.
    lsp_scenarios: Vec<(String, PathBuf)>,
    lsp_log: PathBuf,
    out_dir: PathBuf,
}

impl DvFixture {
    /**
     * The fixture for `header`'s scenario in `dir`, with its logs in `out_dir`: the files as
     * absolute paths, after checking that `mock_gh` and `mock_lsp` are built when the header
     * scripts them, since such a scenario is meaningless without them.
     */
    pub fn new(
        header: &DvHeader,
        dir: &Path,
        out_dir: &Path,
        mock_gh: PathBuf,
        mock_lsp: PathBuf,
    ) -> Result<Self> {
        let absolute = |path: PathBuf| std::path::absolute(&path).unwrap_or(path);
        let gh_scenario = header.gh.as_ref().map(|gh| absolute(dir.join(gh)));
        if gh_scenario.is_some() && !mock_gh.is_file() {
            bail!(
                "mock_gh {} not found; build it (`cargo build -p mock_gh`) or pass --mock-gh",
                mock_gh.display()
            );
        }
        let lsp_scenarios: Vec<_> = header
            .lsp
            .iter()
            .map(|(id, file)| (id.clone(), absolute(dir.join(file))))
            .collect();
        if !lsp_scenarios.is_empty() && !mock_lsp.is_file() {
            bail!(
                "mock_lsp {} not found; build it (`cargo build -p mock_lsp`) or pass --mock-lsp",
                mock_lsp.display()
            );
        }
        Ok(Self {
            mock_gh,
            mock_lsp,
            gh_scenario,
            gh_log: out_dir.join(GH_LOG_FILE),
            lsp_scenarios,
            lsp_log: out_dir.join(LSP_LOG_FILE),
            out_dir: out_dir.to_path_buf(),
        })
    }
}

impl Fixture<DvAction> for DvFixture {
    fn vars(&self) -> Vec<(&'static str, String)> {
        let mut vars = vec![
            ("dv_gh", self.mock_gh.display().to_string()),
            ("dv_mock_gh_log", self.gh_log.display().to_string()),
            ("dv_mock_lsp", self.mock_lsp.display().to_string()),
            ("dv_mock_lsp_log", self.lsp_log.display().to_string()),
        ];
        if let Some(path) = &self.gh_scenario {
            vars.push(("dv_mock_gh", path.display().to_string()));
        }
        vars
    }

    fn env(&self) -> Vec<(OsString, Option<OsString>)> {
        let mut env: Vec<(OsString, Option<OsString>)> = vec![
            (GH_VAR.into(), Some(self.mock_gh.clone().into())),
            (MOCK_GH_LOG_VAR.into(), Some(self.gh_log.clone().into())),
            // Without a scenario file mock_gh answers nothing.
            (
                MOCK_GH_VAR.into(),
                self.gh_scenario.clone().map(OsString::from),
            ),
            (GIT_CEILING_VAR.into(), Some(self.out_dir.clone().into())),
        ];
        for (id, path) in &self.lsp_scenarios {
            let var = format!(
                "{LSP_VAR_PREFIX}{}",
                id.to_ascii_uppercase().replace('-', "_")
            );
            let args = json!([
                "--scenario",
                path.display().to_string(),
                "--log",
                self.lsp_log.display().to_string(),
            ]);
            env.push((format!("{var}_ARGS").into(), Some(args.to_string().into())));
            env.push((var.into(), Some(self.mock_lsp.clone().into())));
        }
        env
    }

    fn run(&mut self, action: &DvAction) -> Result<Value> {
        match action {
            DvAction::GhLog {} => read_log(&self.gh_log),
            DvAction::LspLog { .. } => read_log(&self.lsp_log),
        }
    }

    fn retry_for(&self, action: &DvAction) -> Option<Duration> {
        match action {
            DvAction::GhLog {} => None,
            DvAction::LspLog { wait_ms } => wait_ms.map(Duration::from_millis),
        }
    }
}

/**
 * The records in a `mock_gh` or `mock_lsp` log: one JSON object per line (PLAN §14.2), as an
 * array in the order they were written. A log that doesn't exist yet means nothing has been
 * logged.
 */
pub fn read_log(path: &Path) -> Result<Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(json!([])),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).with_context(|| {
            format!(
                "{}:{}: not a JSON log record: {line}",
                path.display(),
                index + 1
            )
        })?;
        records.push(record);
    }
    Ok(Value::Array(records))
}
