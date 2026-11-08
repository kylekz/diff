use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_automation_runner::{Fixture, RunnerApp};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use crate::Dv;
use crate::fixture::{DvAction, DvFixture, DvHeader, GH_LOG_FILE, LSP_LOG_FILE, VARS, read_log};

fn header(json: Value) -> DvHeader {
    serde_json::from_value(json).unwrap()
}

fn envs(fixture: &DvFixture) -> BTreeMap<String, Option<String>> {
    let text = |value: OsString| value.to_string_lossy().into_owned();
    fixture
        .env()
        .into_iter()
        .map(|(name, value)| (text(name), value.map(text)))
        .collect()
}

/// A fixture for `header` with stand-ins for the helpers that exist, so `new` accepts it.
fn fixture(header: &DvHeader, dir: &Path, out: &Path) -> DvFixture {
    let helper = out.join("helper");
    std::fs::create_dir_all(out).unwrap();
    std::fs::write(&helper, "").unwrap();
    DvFixture::new(header, dir, out, helper.clone(), helper).unwrap()
}

#[test]
fn headers_take_dv_keys_alone() {
    let parsed = serde_json::from_value::<DvHeader>(json!({"gh": "a.json", "lsp": {"css": "b"}}));
    assert!(parsed.is_ok());
    let typo = serde_json::from_value::<DvHeader>(json!({"gh_file": "a.json"})).unwrap_err();
    assert!(typo.to_string().contains("unknown field"), "{typo}");
    assert!(<Dv as RunnerApp>::HEADER_KEYS == ["gh", "lsp"]);
}

#[test]
fn header_files_must_exist_beside_the_scenario() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pr.json"), "{}").unwrap();
    std::fs::write(dir.path().join("hover.json"), "{}").unwrap();
    let check = |json: Value| {
        header(json)
            .check(dir.path())
            .map_err(|err| err.to_string())
    };

    assert_eq!(
        check(json!({"gh": "pr.json", "lsp": {"typescript": "hover.json"}})),
        Ok(())
    );
    assert_eq!(check(json!({})), Ok(()));
    for (json, error) in [
        (json!({"gh": " "}), "must name a mock_gh scenario file"),
        (json!({"gh": "missing.json"}), "does not exist"),
        (
            json!({"lsp": {"Type Script": "hover.json"}}),
            "(lower case)",
        ),
        (json!({"lsp": {"typescript": ""}}), "(lower case)"),
        (json!({"lsp": {"css": "missing.json"}}), "for css"),
    ] {
        let message = check(json.clone()).unwrap_err();
        assert!(message.contains(error), "{json}: {message}");
    }
}

#[test]
fn actions_parse_and_reject_unknown_options() {
    let parse = |json: Value| serde_json::from_value::<DvAction>(json);
    assert_eq!(parse(json!({"gh_log": {}})).unwrap(), DvAction::GhLog {});
    assert_eq!(
        parse(json!({"lsp_log": {}})).unwrap(),
        DvAction::LspLog { wait_ms: None }
    );
    assert_eq!(
        parse(json!({"lsp_log": {"wait_ms": 3000}})).unwrap(),
        DvAction::LspLog {
            wait_ms: Some(3000)
        }
    );
    for bad in [
        json!({"gh_log": {"path": "x"}}),
        json!({"lsp_log": {"path": "x"}}),
        json!({"lsp_log": {"wait_ms": "soon"}}),
        json!({"tool_log": {}}),
    ] {
        assert!(parse(bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn every_launch_reaches_mock_gh_and_stops_git_at_the_output_dir() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let plain = fixture(&DvHeader::default(), dir.path(), &out);
    let envs = envs(&plain);
    let helper = out.join("helper").display().to_string();
    assert_eq!(envs["DV_GH"].as_deref(), Some(helper.as_str()));
    assert_eq!(
        envs["DV_MOCK_GH"], None,
        "removed, so mock_gh answers nothing without a gh header"
    );
    assert_eq!(
        envs["DV_MOCK_GH_LOG"].as_deref().map(PathBuf::from),
        Some(out.join(GH_LOG_FILE))
    );
    assert_eq!(
        envs["GIT_CEILING_DIRECTORIES"]
            .as_deref()
            .map(PathBuf::from),
        Some(out.clone())
    );
    assert!(
        !envs.keys().any(|name| name.starts_with("DV_LSP_")),
        "no language server without an lsp header: {envs:?}"
    );
    assert!(
        !envs.contains_key("DV_WSL_HOME"),
        "the runner removes an inherited one and a header's env may set it, so the fixture \
         leaves it alone"
    );
}

#[test]
fn gh_headers_point_mock_gh_at_their_file() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let gh = fixture(&header(json!({"gh": "fixtures/pr.json"})), dir.path(), &out);
    let envs = envs(&gh);
    let file = std::path::absolute(dir.path().join("fixtures/pr.json")).unwrap();
    assert_eq!(
        envs["DV_MOCK_GH"].as_deref().map(PathBuf::from),
        Some(file.clone())
    );
    let vars: BTreeMap<_, _> = gh.vars().into_iter().collect();
    assert_eq!(vars["dv_mock_gh"], file.display().to_string());
    assert!(vars.keys().all(|name| VARS.contains(name)), "{vars:?}");
    assert_eq!(vars.len(), VARS.len());
}

#[test]
fn lsp_headers_point_a_language_server_at_mock_lsp() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let lsp = fixture(
        &header(json!({"lsp": {"tailwind-css": "fixtures/lsp/a.json"}})),
        dir.path(),
        &out,
    );
    let envs = envs(&lsp);
    let helper = out.join("helper").display().to_string();
    assert_eq!(
        envs["DV_LSP_TAILWIND_CSS"].as_deref(),
        Some(helper.as_str())
    );
    let args: Vec<String> =
        serde_json::from_str(envs["DV_LSP_TAILWIND_CSS_ARGS"].as_deref().unwrap()).unwrap();
    let file = std::path::absolute(dir.path().join("fixtures/lsp/a.json")).unwrap();
    assert_eq!(
        args,
        [
            "--scenario".to_string(),
            file.display().to_string(),
            "--log".to_string(),
            out.join(LSP_LOG_FILE).display().to_string(),
        ]
    );
    let vars: BTreeMap<_, _> = lsp.vars().into_iter().collect();
    assert!(!vars.contains_key("dv_mock_gh"), "no gh header: {vars:?}");
    assert_eq!(
        vars["dv_mock_lsp_log"],
        out.join(LSP_LOG_FILE).display().to_string()
    );
}

#[test]
fn scripted_scenarios_need_their_mock_built() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let new = |json: Value| {
        DvFixture::new(
            &header(json),
            dir.path(),
            dir.path(),
            missing.clone(),
            missing.clone(),
        )
        .map_err(|err| err.to_string())
    };
    assert!(new(json!({})).is_ok(), "nothing scripted, nothing needed");
    assert!(
        new(json!({"gh": "pr.json"}))
            .unwrap_err()
            .contains("cargo build -p mock_gh")
    );
    assert!(
        new(json!({"lsp": {"css": "a.json"}}))
            .unwrap_err()
            .contains("cargo build -p mock_lsp")
    );
}

#[test]
fn logs_read_as_arrays_and_lsp_log_waits() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let mut plain = fixture(&DvHeader::default(), dir.path(), &out);
    assert_eq!(plain.run(&DvAction::GhLog {}).unwrap(), json!([]));
    std::fs::write(
        out.join(GH_LOG_FILE),
        "{\"argv\": [\"pr\", \"view\"]}\n\n{\"argv\": [\"api\"]}\n",
    )
    .unwrap();
    assert_eq!(
        plain.run(&DvAction::GhLog {}).unwrap(),
        json!([{"argv": ["pr", "view"]}, {"argv": ["api"]}])
    );
    std::fs::write(out.join(LSP_LOG_FILE), "not json\n").unwrap();
    let err = read_log(&out.join(LSP_LOG_FILE)).unwrap_err();
    assert!(
        format!("{err:#}").contains(":1: not a JSON log record"),
        "{err:#}"
    );

    assert_eq!(plain.retry_for(&DvAction::GhLog {}), None);
    assert_eq!(
        plain.retry_for(&DvAction::LspLog {
            wait_ms: Some(3000)
        }),
        Some(Duration::from_secs(3))
    );
}

#[test]
fn mock_paths_come_from_the_command_line() {
    let mut dv = Dv::default();
    let mut value = || Ok("gh.exe".to_string());
    assert!(dv.option("--mock-gh", &mut value).unwrap());
    assert!(!dv.option("--bogus", &mut value).unwrap());
    assert_eq!(
        dv.mock_gh,
        Some(std::path::absolute("gh.exe").unwrap()),
        "absolute, since the app runs in its data dir"
    );
    assert_eq!(dv.mock_lsp, None);
}

#[test]
fn the_profile_is_dvs() {
    let profile = Dv::default().profile();
    assert_eq!(profile.name, "dv");
    assert_eq!(profile.env_prefix, "DV_");
    assert_eq!(profile.data_dir_var, "DV_DATA_DIR");
    assert_eq!(profile.platforms, ["windows", "linux", "macos"]);
    assert!(
        Path::new(profile.manifest_dir)
            .join("../../tests/e2e")
            .is_dir(),
        "the runner finds the workspace from here"
    );
}
