//! `dv doctor`, `dv setup path`, `dv setup all` (PLAN §12): this machine's checks against a
//! scratch home, the user Path's stand-in file (`DV_USER_PATH_FILE`, never the registry) and
//! `mock_gh`; no Linux build, so no distro is listed and no `wsl.exe` starts.

use std::path::Path;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::*;
use crate::skill::SKILL_MD;

/// The fixture with a scratch home (`HOME`, `USERPROFILE`) and `mock_gh` answering from
/// `gh` (a scenario file's `calls`, or none).
fn with_home(home: &Path, gh: Option<Value>) -> Fixture {
    let mut fixture = Fixture::new();
    for name in ["HOME", "USERPROFILE"] {
        fixture
            .env
            .insert(name.to_string(), home.display().to_string());
    }
    if let Some(calls) = gh {
        let file = fixture.outside("gh").join("gh.json");
        std::fs::write(&file, json!({ "calls": calls }).to_string()).unwrap();
        fixture
            .env
            .insert("DV_MOCK_GH".to_string(), file.display().to_string());
    }
    fixture
}

/// `mock_gh` signed out, as gh prints it.
fn signed_out() -> Value {
    json!([{
        "argv": ["auth", "status"],
        "stderr": "You are not logged into any GitHub hosts. To log in, run: gh auth login\n",
        "exit": 1,
    }])
}

/// `mock_gh` signed in as `kylekz`, as gh 2.40 and later print it.
fn signed_in() -> Value {
    json!([{
        "argv": ["auth", "status"],
        "stderr": "github.com\n  ✓ Logged in to github.com account kylekz (keyring)\n",
    }])
}

/// The checks of the one environment `dv doctor --json` listed, by id: `(state, detail)`.
fn checks(json: &Value) -> BTreeMap<String, (String, String)> {
    let environments = json["environments"].as_array().unwrap();
    assert_eq!(environments.len(), 1, "{json}");
    environments[0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| {
            (
                check["id"].as_str().unwrap().to_string(),
                (
                    check["state"].as_str().unwrap().to_string(),
                    check["detail"].as_str().unwrap().to_string(),
                ),
            )
        })
        .collect()
}

fn state(json: &Value, id: &str) -> String {
    checks(json)
        .get(id)
        .unwrap_or_else(|| panic!("no check {id} in {json}"))
        .0
        .clone()
}

fn doctor(fixture: &Fixture) -> (Exit, Value) {
    let ran = fixture.run(&["doctor", "--json"]);
    assert!(
        matches!(ran.exit, Exit::Ok | Exit::Failure),
        "{}",
        ran.stderr
    );
    let json = ran.json();
    assert_eq!(json["schema"], 1);
    assert_eq!(json["ok"], ran.exit == Exit::Ok);
    (ran.exit, json)
}

#[test]
fn the_skills_read_installed_missing_out_of_date_or_skipped() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let fixture = with_home(home, None);
    std::fs::create_dir(home.join(".claude")).unwrap();
    let (exit, json) = doctor(&fixture);
    assert_eq!(exit, Exit::Failure);
    let skill = home
        .join(".claude")
        .join("skills")
        .join("dv-review")
        .join("SKILL.md");
    assert_eq!(
        checks(&json)["skill-claude-code"],
        (
            "missing".to_string(),
            format!("not installed: {}", skill.display())
        )
    );
    let claude = json["environments"][0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "skill-claude-code")
        .unwrap()
        .clone();
    assert_eq!(
        claude["fix"],
        json!({"action": "Install", "command": "dv setup agents"})
    );
    assert_eq!(claude["title"], "dv-review skill for Claude Code");
    for agent in ["codex", "cursor"] {
        assert_eq!(state(&json, &format!("skill-{agent}")), "skipped");
    }

    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, "an older dv's skill").unwrap();
    let (_, json) = doctor(&fixture);
    assert_eq!(state(&json, "skill-claude-code"), "out_of_date");

    std::fs::write(&skill, SKILL_MD).unwrap();
    let (_, json) = doctor(&fixture);
    assert_eq!(
        checks(&json)["skill-claude-code"],
        ("ok".to_string(), format!("installed: {}", skill.display()))
    );
}

#[test]
fn git_and_gh_say_what_to_run() {
    let home = tempfile::tempdir().unwrap();
    let fixture = with_home(home.path(), Some(signed_out()));
    let (_, json) = doctor(&fixture);
    // The fixture's global configuration has no identity, and mock_gh says nobody signed in.
    assert_eq!(
        checks(&json)["git"],
        (
            "missing".to_string(),
            "user.name and user.email aren't set, so git can't say who wrote a commit or a \
             comment"
                .to_string()
        )
    );
    assert_eq!(
        checks(&json)["gh"],
        (
            "missing".to_string(),
            "gh isn't signed in to GitHub".to_string()
        )
    );
    let text = fixture.run(&["doctor"]);
    assert_eq!(text.exit, Exit::Failure);
    assert!(
        text.stdout.contains(
            "  missing  GitHub CLI  gh isn't signed in to GitHub\n    fix: gh auth login\n"
        ),
        "{}",
        text.stdout
    );
    assert!(
        text.stdout.lines().all(|line| line == line.trim_end()),
        "{}",
        text.stdout
    );

    let fixture = with_home(home.path(), Some(signed_in()));
    let (_, json) = doctor(&fixture);
    // A new fixture: its own home's configuration.
    std::fs::write(
        fixture.repo.home().join(".gitconfig"),
        "[user]\n\tname = Kyle\n\temail = kyle@example.com\n",
    )
    .unwrap();
    assert_eq!(
        checks(&json)["gh"],
        (
            "ok".to_string(),
            "signed in to github.com as kylekz".to_string()
        )
    );
    let (_, json) = doctor(&fixture);
    assert_eq!(
        checks(&json)["git"],
        ("ok".to_string(), "Kyle <kyle@example.com>".to_string())
    );
}

#[test]
fn a_gh_that_fails_otherwise_says_what_gh_said() {
    let home = tempfile::tempdir().unwrap();
    let fixture = with_home(
        home.path(),
        Some(json!([{
            "argv": ["auth", "status"],
            "stderr": "error connecting to api.github.com\ncheck your internet connection\n",
            "exit": 1,
        }])),
    );
    let (_, json) = doctor(&fixture);
    assert_eq!(
        checks(&json)["gh"],
        (
            "failed".to_string(),
            "gh auth status failed: error connecting to api.github.com".to_string()
        )
    );
}

#[test]
fn a_gh_older_than_dv_needs_is_out_of_date() {
    let home = tempfile::tempdir().unwrap();
    let version = |said: &str| {
        let mut calls = vec![json!({"argv": ["--version"], "stdout": said})];
        calls.extend(signed_in().as_array().unwrap().iter().cloned());
        let fixture = with_home(home.path(), Some(Value::Array(calls)));
        checks(&doctor(&fixture).1)["gh"].clone()
    };
    // `gh api --slurp`, which dv reads a pull request's files with, came with gh 2.48.
    assert_eq!(
        version(
            "gh version 2.47.0 (2024-03-19)\nhttps://github.com/cli/cli/releases/tag/v2.47.0\n"
        ),
        (
            "out_of_date".to_string(),
            "gh 2.47.0 is older than dv needs (gh 2.48.0 or later): update it \
             (https://cli.github.com)"
                .to_string()
        )
    );
    let signed_in = (
        "ok".to_string(),
        "signed in to github.com as kylekz".to_string(),
    );
    for said in [
        "gh version 2.48.0 (2024-04-03)\n",
        "gh version 2.103.3 (2026-10-26)\nhttps://github.com/cli/cli/releases/tag/v2.103.3\n",
        "gh version 3.0.0\n",
        // A gh that doesn't say is asked the rest as it is.
        "a development build\n",
    ] {
        assert_eq!(version(said), signed_in, "{said}");
    }
    let text = with_home(
        home.path(),
        Some(json!([{"argv": ["--version"], "stdout": "gh version 2.9.0 (2022-04-26)\n"}])),
    )
    .run(&["doctor"]);
    assert_eq!(text.exit, Exit::Failure);
    assert!(
        text.stdout.contains(
            "  out of date  GitHub CLI  gh 2.9.0 is older than dv needs (gh 2.48.0 or later): \
             update it (https://cli.github.com)\n"
        ),
        "{}",
        text.stdout
    );
}

#[test]
fn a_gh_that_isnt_there_is_missing() {
    let home = tempfile::tempdir().unwrap();
    let mut fixture = with_home(home.path(), None);
    fixture.env.insert(
        "DV_GH".to_string(),
        home.path().join("no-such-gh.exe").display().to_string(),
    );
    let (_, json) = doctor(&fixture);
    assert_eq!(
        checks(&json)["gh"],
        (
            "missing".to_string(),
            "gh isn't installed: dv reaches GitHub through it (https://cli.github.com)".to_string()
        )
    );
}

#[cfg(unix)]
#[test]
fn setup_path_links_this_dv_and_puts_local_bin_on_the_shells_path() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let fixture = with_home(home, None);
    let json = fixture.run(&["setup", "path", "--yes", "--json"]).ok_json();
    let link = home.join(".local/bin/dv");
    assert_eq!(json["action"], "added");
    assert_eq!(json["dir"], home.join(".local/bin").display().to_string());
    assert_eq!(json["link"], link.display().to_string());
    assert_eq!(
        std::fs::canonicalize(&link).unwrap(),
        std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap()
    );
    // The scratch home's shell has no ~/.local/bin on its PATH: a line in its startup file.
    let rc = json["rc_file"]
        .as_str()
        .expect("a startup file was written")
        .to_string();
    let text = std::fs::read_to_string(&rc).unwrap();
    assert!(text.contains(crate::checks::path::RC_MARK), "{text}");
    let (_, doctor) = doctor(&fixture);
    assert_eq!(state(&doctor, "path"), "ok", "{doctor}");
    // Again: nothing to do.
    let again = fixture.run(&["setup", "path", "--json"]).ok_json();
    assert_eq!(again["action"], "unchanged");

    let removed = fixture
        .run(&["setup", "path", "--remove", "--yes", "--json"])
        .ok_json();
    assert_eq!(removed["action"], "removed");
    assert!(std::fs::symlink_metadata(&link).is_err());
    assert!(
        std::fs::read_to_string(&rc)
            .map_or(true, |text| !text.contains(crate::checks::path::RC_MARK))
    );
}

#[test]
fn a_restricted_run_reaches_no_other_distro() {
    use crate::checks::{Seams, distro};

    let home = tempfile::tempdir().unwrap();
    let fixture = with_home(home.path(), None);
    let (ctx, _, _) = fixture.context(fixture.path(), &Input::default());
    let mut seams = Seams::from_context(&ctx);
    seams.only_distros = Some(vec!["dv-allowed-distro".to_string()]);
    // A distro that can't exist: should the guard regress, nothing real is reached.
    let other = "dv-no-such-distro";
    let refused = |result: Result<String, String>| {
        let error = result.unwrap_err();
        assert!(
            error.starts_with("this run doesn't reach dv-no-such-distro"),
            "{error}"
        );
    };
    refused(distro::start(&seams, other).map(|()| String::new()));
    refused(distro::apply(&seams, other, "dv", &|_, _| {}));
    let env = distro::environment(
        &seams,
        &dv_backend::wsl::Distro {
            name: other.to_string(),
            running: true,
        },
    );
    assert!(env.checks.is_empty(), "{env:?}");
    assert!(env.note.unwrap().starts_with("this run doesn't reach"));
    // `DV_ONLY_DISTROS` says the same to a CLI: set and empty, none.
    let mut fixture = fixture;
    fixture
        .env
        .insert("DV_ONLY_DISTROS".to_string(), String::new());
    let (ctx, _, _) = fixture.context(fixture.path(), &Input::default());
    assert_eq!(Seams::from_context(&ctx).only_distros, Some(Vec::new()));
}

/// A distro's checks run with what a command run there would take across (PLAN §10.4).
#[test]
fn a_distros_checks_take_across_what_a_command_would() {
    use crate::checks::Seams;

    let home = tempfile::tempdir().unwrap();
    let mut fixture = with_home(home.path(), None);
    let distro_env = |fixture: &Fixture| {
        let (ctx, _, _) = fixture.context(fixture.path(), &Input::default());
        Seams::from_context(&ctx).distro_env
    };
    let pair = |name: &str, value: &str| (name.to_string(), value.to_string());
    fixture.env.remove("DV_WSL_HOME");
    fixture.env.remove("GIT_ALLOW_PROTOCOL");
    fixture
        .env
        .insert("DV_GH".to_string(), "/t/no-gh".to_string());
    // Outside a sandbox the distro's own environment decides which `gh` runs.
    assert_eq!(distro_env(&fixture), []);
    fixture
        .env
        .insert("GIT_ALLOW_PROTOCOL".to_string(), "file".to_string());
    assert_eq!(distro_env(&fixture), [pair("GIT_ALLOW_PROTOCOL", "file")]);
    // A value that is no sandbox (PLAN §5.2) is an error where it is used, never a sandbox.
    fixture
        .env
        .insert("DV_WSL_HOME".to_string(), "/".to_string());
    assert_eq!(distro_env(&fixture), [pair("GIT_ALLOW_PROTOCOL", "file")]);
    fixture
        .env
        .insert("DV_WSL_HOME".to_string(), "/tmp/dv-home".to_string());
    assert_eq!(
        distro_env(&fixture),
        [
            pair("GIT_ALLOW_PROTOCOL", "file"),
            pair("DV_GH", "/t/no-gh")
        ]
    );
}

#[test]
fn container_tools_vm_distros_arent_the_users() {
    use crate::checks::distro::is_vm;
    for name in [
        "docker-desktop",
        "docker-desktop-data",
        "rancher-desktop",
        "rancher-desktop-data",
        "podman-machine-default",
    ] {
        assert!(is_vm(name), "{name}");
    }
    assert!(!is_vm("Ubuntu"));
    assert!(!is_vm("Debian"));
}

#[cfg(windows)]
mod windows {
    use pretty_assertions::assert_eq;

    use super::*;

    fn user_path(fixture: &Fixture) -> Option<String> {
        std::fs::read_to_string(&fixture.env["DV_USER_PATH_FILE"])
            .ok()
            .map(|text| text.trim_end().to_string())
    }

    #[test]
    fn without_a_linux_build_no_distro_is_listed() {
        let home = tempfile::tempdir().unwrap();
        let fixture = with_home(home.path(), None);
        let (_, json) = doctor(&fixture);
        assert_eq!(json["environments"][0]["name"], "Windows");
        assert_eq!(json["environments"][0]["kind"], "windows");
        let (state, detail) = checks(&json)["wsl"].clone();
        assert_eq!(state, "skipped");
        assert!(
            detail.starts_with("this dv can't check WSL distros: no Linux build of dv"),
            "{detail}"
        );
    }

    #[test]
    fn setup_path_adds_this_folder_to_the_user_path_once_and_takes_it_back() {
        let home = tempfile::tempdir().unwrap();
        let fixture = with_home(home.path(), None);
        let dir = r"D:\Programs\dv";
        std::fs::write(&fixture.env["DV_USER_PATH_FILE"], "C:\\Windows;C:\\tools\n").unwrap();

        // Not asked: the plan, and exit 2; nothing written.
        let ran = fixture.run(&["setup", "path", "--dir", dir]);
        assert_eq!(ran.exit, Exit::Usage, "{}", ran.stderr);
        assert_eq!(
            ran.lines(),
            [format!(
                "Windows  put {dir} in the user Path ({}), ahead of any other dv",
                fixture.env["DV_USER_PATH_FILE"]
            )]
        );
        assert_eq!(
            ran.stderr,
            "dv: dv setup path changes your PATH setup: pass --yes to go ahead\n"
        );
        assert_eq!(user_path(&fixture).as_deref(), Some(r"C:\Windows;C:\tools"));

        let json = fixture
            .run(&["setup", "path", "--dir", dir, "--yes", "--json"])
            .ok_json();
        assert_eq!(
            json,
            json!({"schema": 1, "dir": dir, "action": "added", "link": null, "rc_file": null})
        );
        assert_eq!(
            user_path(&fixture).as_deref(),
            Some(r"C:\Windows;C:\tools;D:\Programs\dv")
        );
        // Again: nothing to do, nothing to ask.
        let again = fixture.run(&["setup", "path", "--dir", dir]);
        assert_eq!(again.exit, Exit::Ok, "{}", again.stderr);
        assert_eq!(
            again.stdout,
            format!("Windows  unchanged  {dir} is on PATH already\n")
        );

        // --remove takes out exactly that folder, however it is spelled there.
        std::fs::write(
            &fixture.env["DV_USER_PATH_FILE"],
            "C:\\Windows;d:\\programs\\DV\\;C:\\tools;D:\\Programs\\dv2\n",
        )
        .unwrap();
        let json = fixture
            .run(&["setup", "path", "--remove", "--dir", dir, "--yes", "--json"])
            .ok_json();
        assert_eq!(json["action"], "removed");
        assert_eq!(
            user_path(&fixture).as_deref(),
            Some(r"C:\Windows;C:\tools;D:\Programs\dv2")
        );
        let json = fixture
            .run(&["setup", "path", "--remove", "--dir", dir, "--json"])
            .ok_json();
        assert_eq!(json["action"], "absent");
    }

    #[test]
    fn the_path_check_names_this_executables_folder() {
        let home = tempfile::tempdir().unwrap();
        let fixture = with_home(home.path(), None);
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap();
        let (_, json) = doctor(&fixture);
        let place = format!("the user Path ({})", fixture.env["DV_USER_PATH_FILE"]);
        assert_eq!(
            checks(&json)["path"],
            (
                "missing".to_string(),
                format!(
                    "not in {place}, so a terminal doesn't find this dv: {}",
                    dir.display()
                )
            )
        );
        fixture.run(&["setup", "path", "--yes"]);
        let (_, json) = doctor(&fixture);
        assert_eq!(
            checks(&json)["path"],
            (
                "ok".to_string(),
                format!("first on PATH, in {place}: {}", dir.display())
            )
        );
    }

    /// A folder holding a `dv.exe` of another install.
    fn other_dv() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dv.exe"), "an older dv").unwrap();
        dir
    }

    #[test]
    fn another_dv_first_on_path_fails_and_setup_path_puts_this_one_ahead() {
        let home = tempfile::tempdir().unwrap();
        let fixture = with_home(home.path(), None);
        let old = other_dv();
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap().to_path_buf();
        let user_path = fixture.env["DV_USER_PATH_FILE"].clone();
        std::fs::write(
            &user_path,
            format!("C:\\Windows;{};{}\n", old.path().display(), dir.display()),
        )
        .unwrap();
        let (exit, json) = doctor(&fixture);
        assert_eq!(exit, Exit::Failure);
        let (state, detail) = checks(&json)["path"].clone();
        assert_eq!(state, "failed");
        assert_eq!(
            detail,
            format!(
                "another dv comes first on PATH, so a terminal runs it, not this one: {}",
                old.path().join("dv.exe").display()
            )
        );
        let json = fixture.run(&["setup", "path", "--yes", "--json"]).ok_json();
        assert_eq!(json["action"], "added");
        assert_eq!(
            std::fs::read_to_string(&user_path).unwrap().trim_end(),
            format!("C:\\Windows;{};{}", dir.display(), old.path().display())
        );
        let (_, json) = doctor(&fixture);
        assert_eq!(checks(&json)["path"].0, "ok");
    }

    #[test]
    fn the_installed_copy_first_on_path_is_named_so_for_a_development_build() {
        // Velopack's layout (PLAN §13.3): `<root>\Update.exe` and `<root>\current\dv.exe`.
        let home = tempfile::tempdir().unwrap();
        let fixture = with_home(home.path(), None);
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("current");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(current.join("dv.exe"), "the installed dv").unwrap();
        std::fs::write(root.path().join("Update.exe"), "Velopack's").unwrap();
        assert!(crate::checks::path::is_installed_copy(&current));
        assert!(!crate::checks::path::is_installed_copy(root.path()));
        let dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::write(
            &fixture.env["DV_USER_PATH_FILE"],
            format!("{};{}\n", current.display(), dir.display()),
        )
        .unwrap();
        let (_, json) = doctor(&fixture);
        assert_eq!(
            checks(&json)["path"],
            (
                "failed".to_string(),
                format!(
                    "the installed dv comes first on PATH, so a terminal runs it, not this one: \
                     {}",
                    current.join("dv.exe").display()
                )
            )
        );
    }

    #[test]
    fn another_dv_in_the_system_path_stays_ahead_and_says_so() {
        let home = tempfile::tempdir().unwrap();
        let mut fixture = with_home(home.path(), None);
        let old = other_dv();
        let machine = fixture.outside("machine-path").join("Path.txt");
        std::fs::write(&machine, format!("C:\\Windows;{}\n", old.path().display())).unwrap();
        fixture.env.insert(
            "DV_MACHINE_PATH_FILE".to_string(),
            machine.display().to_string(),
        );
        let (_, json) = doctor(&fixture);
        let environments = json["environments"][0]["checks"].as_array().unwrap();
        let path = environments
            .iter()
            .find(|check| check["id"] == "path")
            .unwrap();
        assert_eq!(path["state"], "failed");
        assert!(
            path["detail"]
                .as_str()
                .unwrap()
                .starts_with("another dv in the system Path comes first"),
            "{path}"
        );
        // Only an administrator changes the system Path: no button.
        assert_eq!(path["fix"], Value::Null);
        let ran = fixture.run(&["setup", "path", "--yes"]);
        assert_eq!(ran.exit, Exit::Ok, "{}", ran.stderr);
        assert!(
            ran.stdout.contains(&format!(
                "but {} in the system Path still comes first",
                old.path().join("dv.exe").display()
            )),
            "{}",
            ran.stdout
        );
        let (_, json) = doctor(&fixture);
        assert_eq!(checks(&json)["path"].0, "failed");
    }

    #[test]
    fn setup_all_applies_what_dv_can_and_lists_the_rest() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let fixture = with_home(home, Some(signed_in()));
        std::fs::create_dir(home.join(".claude")).unwrap();
        std::fs::create_dir(home.join(".cursor")).unwrap();

        // Not asked: the plan, and exit 2.
        let ran = fixture.run(&["setup"]);
        assert_eq!(ran.exit, Exit::Usage, "{}", ran.stderr);
        assert_eq!(
            ran.lines(),
            [
                "Windows  path  Add to PATH",
                "Windows  skill-claude-code  Install",
                "Windows  skill-cursor  Install",
                "Windows  git  run `git config --global user.name \"Your Name\"; git config \
                 --global user.email \"you@example.com\"` yourself",
            ]
        );
        assert!(
            ran.stderr.ends_with("pass --yes to go ahead\n"),
            "{}",
            ran.stderr
        );

        let json = fixture.run(&["setup", "--yes", "--json"]).ok_json();
        let outcomes: Vec<(String, String)> = json["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| {
                (
                    step["check"].as_str().unwrap().to_string(),
                    step["outcome"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            outcomes,
            [
                ("path".to_string(), "applied".to_string()),
                ("skill-claude-code".to_string(), "applied".to_string()),
                ("skill-cursor".to_string(), "applied".to_string()),
                ("git".to_string(), "manual".to_string()),
            ]
        );
        assert_eq!(json["steps"][3]["environment"], "Windows");
        assert!(home.join(".claude/skills/dv-review/SKILL.md").is_file());
        assert!(home.join(".agents/skills/dv-review/SKILL.md").is_file());

        // Only git is left, which dv doesn't change: nothing to ask.
        let ran = fixture.run(&["setup", "all"]);
        assert_eq!(ran.exit, Exit::Ok, "{}", ran.stderr);
        assert_eq!(
            ran.lines(),
            [
                "Windows  git  run `git config --global user.name \"Your Name\"; git config \
              --global user.email \"you@example.com\"` yourself"
            ]
        );
        std::fs::write(
            fixture.repo.home().join(".gitconfig"),
            "[user]\n\tname = Kyle\n\temail = kyle@example.com\n",
        )
        .unwrap();
        let ran = fixture.run(&["setup"]);
        assert_eq!(ran.exit, Exit::Ok, "{}", ran.stderr);
        assert_eq!(ran.stdout, "nothing to set up\n");
        let (exit, _) = doctor(&fixture);
        assert_eq!(exit, Exit::Ok);
    }

    #[test]
    fn a_distros_checks_run_as_its_own_dv_in_a_scratch_home() {
        use crate::checks::{self, Seams, State, distro};
        use crate::tests::sandbox::{Sandbox, builds_dir};

        let Some(sandbox) = Sandbox::get("doctor-wsl") else {
            eprintln!("skipped: DV_TEST_WSL_DISTRO isn't set");
            return;
        };
        let home = tempfile::tempdir().unwrap();
        let mut fixture = with_home(home.path(), None);
        fixture.env.insert(
            "DV_LINUX_DIR".to_string(),
            builds_dir().display().to_string(),
        );
        fixture
            .env
            .insert("DV_WSL_HOME".to_string(), sandbox.home.clone());
        let (ctx, _, _) = fixture.context(fixture.path(), &Input::default());
        let mut seams = Seams::from_context(&ctx);
        // Only the test's distro, whatever else runs.
        seams.only_distros = Some(vec![sandbox.distro.clone()]);
        // The distro's dv asks no real gh.
        seams.distro_env = vec![("DV_GH".to_string(), "/dv-test-has-no-gh".to_string())];
        let running = dv_backend::wsl::Distro {
            name: sandbox.distro.clone(),
            running: true,
        };
        let env = distro::environment(&seams, &running);
        assert_eq!(env.kind, checks::Kind::Wsl);
        assert_eq!(env.check("dv").unwrap().state, State::Missing, "{env:?}");
        assert_eq!(env.checks.len(), 1, "{env:?}");
        assert!(env.note.is_some());

        distro::apply(&seams, &sandbox.distro, "dv", &|_, _| {}).unwrap();
        let env = distro::environment(&seams, &running);
        assert_eq!(env.check("dv").unwrap().state, State::Ok, "{env:?}");
        let ids: Vec<&str> = env.checks.iter().map(|check| check.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "dv",
                "path",
                "skill-claude-code",
                "skill-codex",
                "skill-cursor",
                "git",
                "gh"
            ],
            "{env:?}"
        );
        let gh = env.check("gh").unwrap();
        assert_eq!(gh.state, State::Missing, "{gh:?}");
        assert!(gh.detail.starts_with("gh isn't installed"), "{gh:?}");
        // The scratch home has no agent: their skills are skipped, read in the distro.
        let claude = env.check("skill-claude-code").unwrap();
        assert_eq!(claude.state, State::Skipped);
        assert!(
            claude.detail.contains(&format!("{}/.claude", sandbox.home)),
            "{claude:?}"
        );

        // Claude Code there: missing, installed by the distro's dv, then edited: out of date.
        sandbox.sh(r#"mkdir -p "$1/.claude""#);
        let env = distro::environment(&seams, &running);
        assert_eq!(
            env.check("skill-claude-code").unwrap().state,
            State::Missing
        );
        distro::apply(&seams, &sandbox.distro, "skill-claude-code", &|_, _| {}).unwrap();
        let env = distro::environment(&seams, &running);
        assert_eq!(env.check("skill-claude-code").unwrap().state, State::Ok);
        sandbox.sh(r#"echo edited > "$1/.claude/skills/dv-review/SKILL.md""#);
        let env = distro::environment(&seams, &running);
        assert_eq!(
            env.check("skill-claude-code").unwrap().state,
            State::OutOfDate
        );
        distro::apply(&seams, &sandbox.distro, "skill-claude-code", &|_, _| {}).unwrap();
        let env = distro::environment(&seams, &running);
        assert_eq!(env.check("skill-claude-code").unwrap().state, State::Ok);

        // PATH: the install's link runs this dv; a line puts ~/.local/bin on the shell's PATH.
        let path = env.check("path").unwrap().clone();
        if path.state == State::Missing {
            distro::apply(&seams, &sandbox.distro, "path", &|_, _| {}).unwrap();
            let env = distro::environment(&seams, &running);
            assert_eq!(env.check("path").unwrap().state, State::Ok, "{env:?}");
        } else {
            assert_eq!(path.state, State::Ok, "{path:?}");
        }
    }
}
