use std::time::{Duration, Instant};

use dv_test_support::FixtureRepo;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

/// The base a relative `cwd` resolves against in these tests.
fn base() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\data")
    } else {
        PathBuf::from("/data")
    }
}

fn parse(args: Value) -> Result<Request> {
    let args = args.as_object().cloned().unwrap_or_default();
    Request::parse("cli", &args, &base())
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// `request` started with `exe` and the environment a child gets, no seeded repo involved.
fn start(exe: &Path, request: &Request) -> Running {
    let env = child_env(None, &base(), &request.env, false);
    Running::start(exe, request, &env).expect("the child starts")
}

/// Waits (for real) until `running` exits and returns what it printed.
fn finish(running: &mut Running) -> Output {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = running.exited().unwrap() {
            return running.output(status);
        }
        assert!(Instant::now() < deadline, "the child didn't exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn git(repo: &FixtureRepo, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Output {
    let request = Request {
        args: strings(args),
        cwd: repo.path().to_path_buf(),
        stdin: stdin.map(str::to_string),
        env: env
            .iter()
            .map(|(name, value)| (name.to_string(), Some(value.to_string())))
            .collect(),
    };
    let env = child_env(Some(repo.env_overlay()), &base(), &request.env, false);
    let mut running = Running::start(Path::new("git"), &request, &env).expect("git starts");
    finish(&mut running)
}

/// Not a test of its own: the child the registry tests start, this test binary run on this
/// test alone with `DV_TEST_SLEEPING_CHILD` set, which sleeps until it is killed (or a minute
/// passes). Without the variable it returns at once.
#[test]
fn sleeping_child() {
    if std::env::var_os("DV_TEST_SLEEPING_CHILD").is_some() {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// A request that starts [`sleeping_child`].
fn sleeper() -> (PathBuf, Request) {
    let exe = std::env::current_exe().expect("the test binary");
    let request = Request {
        args: strings(&[
            "--exact",
            "automation::agent_cli::tests::sleeping_child",
            "--nocapture",
        ]),
        cwd: std::env::current_dir().expect("a working directory"),
        stdin: None,
        env: vec![("DV_TEST_SLEEPING_CHILD".into(), Some("1".into()))],
    };
    (exe, request)
}

#[test]
fn requests_parse_strictly() {
    let request = parse(json!({
        "args": ["comment", "reply", "t-1", "--body-file", "-"],
        "cwd": "repos/api-server",
        "stdin": "Fixed.\n",
        "env": {"CLAUDECODE": "1", "DV_REVIEW": null},
    }))
    .unwrap();
    assert_eq!(
        request,
        Request {
            args: strings(&["comment", "reply", "t-1", "--body-file", "-"]),
            cwd: base().join("repos/api-server"),
            stdin: Some("Fixed.\n".into()),
            env: vec![
                ("CLAUDECODE".into(), Some("1".into())),
                ("DV_REVIEW".into(), None)
            ],
        }
    );
    assert_eq!(
        request.line(),
        "dv comment reply t-1 --body-file -",
        "messages name the command"
    );
    // The defaults: the app's working directory, no stdin, no changes.
    let request = parse(json!({"args": ["status", "--json"]})).unwrap();
    assert_eq!(request.cwd, base());
    assert_eq!(request.stdin, None);
    assert_eq!(request.env, vec![]);
    // An absolute cwd stays as it is.
    let absolute = base().join("elsewhere");
    let request = parse(json!({"args": ["status"], "cwd": absolute})).unwrap();
    assert_eq!(request.cwd, absolute);

    for (args, error) in [
        (json!({}), "needs \"args\""),
        (json!({"args": "status"}), "needs \"args\""),
        (json!({"args": ["status", 1]}), "are strings"),
        (json!({"args": []}), "names no command"),
        (
            json!({"args": ["open", "repos/api"]}),
            "would open a window",
        ),
        (json!({"args": ["status"], "cwd": 3}), "\"cwd\" is a path"),
        (
            json!({"args": ["status"], "stdin": ["a"]}),
            "\"stdin\" is a string",
        ),
        (
            json!({"args": ["status"], "env": ["A=1"]}),
            "\"env\" is an object",
        ),
        (
            json!({"args": ["status"], "env": {"A": 1}}),
            "strings or null",
        ),
        (
            json!({"args": ["status"], "env": {"DV_DATA_DIR": "C:/elsewhere"}}),
            "can't change DV_DATA_DIR",
        ),
        (
            json!({"args": ["status"], "env": {"DV_GH": "gh"}}),
            "can't change DV_GH",
        ),
        (
            json!({"args": ["status"], "env": {"DV_LINUX_DIR": "D:/builds"}}),
            "can't change DV_LINUX_DIR",
        ),
        (
            json!({"args": ["status"], "env": {"DV_WSL_HOME": null}}),
            "can't change DV_WSL_HOME",
        ),
        (
            json!({"args": ["status"], "env": {"DV_ONLY_DISTROS": "Ubuntu"}}),
            "can't change DV_ONLY_DISTROS",
        ),
        (
            json!({"args": ["status"], "env": {"A=B": "1"}}),
            "isn't an environment",
        ),
    ] {
        let message = parse(args.clone()).unwrap_err().to_string();
        assert!(message.contains(error), "{args}: {message}");
    }
}

#[test]
fn only_headless_commands_start() {
    for args in [
        &["status", "--json"][..],
        &["review", "wait", "--timeout", "120", "--json"],
        &[
            "comment",
            "address",
            "t-1",
            "--note",
            "Done",
            "--author",
            "claude-code",
        ],
        &["--json", "review", "list", "--all"],
        &["doctor"],
        // Printed and exited by the child itself, without a window.
        &["--help"],
        &["--version"],
        &["review", "bogus", "--json"],
    ] {
        check_headless(&strings(args)).unwrap_or_else(|error| panic!("{args:?}: {error}"));
    }
    for (args, error) in [
        (&[][..], "names no command"),
        (&["--json"], "names no command"),
        (&["--data-dir", "x"], "names no command"),
        (&["open"], "would open a window"),
        (
            &["open", "repos/api", "--review", "r-1"],
            "would open a window",
        ),
    ] {
        let message = check_headless(&strings(args)).unwrap_err().to_string();
        assert!(message.contains(error), "{args:?}: {message}");
    }
}

#[test]
fn timeouts_have_a_default_and_a_cap() {
    let args = |value: Value| value.as_object().cloned().unwrap();
    let default = Duration::from_secs(7);
    assert_eq!(timeout("cli", &args(json!({})), default).unwrap(), default);
    assert_eq!(
        timeout("cli", &args(json!({"timeout_ms": 1500})), default).unwrap(),
        Duration::from_millis(1500)
    );
    assert!(
        timeout("cli", &args(json!({"timeout_ms": 120_001})), default)
            .unwrap_err()
            .to_string()
            .contains("at most 120000")
    );
    assert!(
        timeout("cli", &args(json!({"timeout_ms": "5"})), default)
            .unwrap_err()
            .to_string()
            .contains("whole number")
    );
    assert!(check_name("first-wait").is_ok());
    assert!(check_name("").is_err());
    assert!(check_name("a wait").is_err());
}

#[test]
fn the_child_environment_is_the_fixtures_without_the_agents_variables() {
    let overlay: EnvOverlay = vec![
        ("HOME".into(), Some("/fixture/home".into())),
        ("GIT_DIR".into(), None),
    ];
    let env = child_env(
        Some(overlay.clone()),
        Path::new("/data"),
        &[
            ("CLAUDECODE".into(), Some("1".into())),
            ("RUST_LOG".into(), None),
        ],
        false,
    );
    let set = |name: &str, value: &str| (OsString::from(name), Some(OsString::from(value)));
    let unset = |name: &str| (OsString::from(name), None);
    let home = Path::new("/data").join("home");
    let home = home.to_str().unwrap();
    assert_eq!(
        env,
        vec![
            // A home of its own under the data dir, which a seeded repo's overlay may replace.
            set("HOME", home),
            set("USERPROFILE", home),
            set(
                "CODEX_HOME",
                Path::new(home).join(".codex").to_str().unwrap()
            ),
            set("HOME", "/fixture/home"),
            unset("GIT_DIR"),
            set("DV_DATA_DIR", "/data"),
            // The user Path's stand-in, the app's own.
            set(
                "DV_USER_PATH_FILE",
                Path::new("/data").join("user-path.txt").to_str().unwrap()
            ),
            unset("CLAUDECODE"),
            unset("DV_AUTHOR"),
            unset("DV_MODEL"),
            unset("DV_REVIEW"),
            // No Linux build to run inside a distro: a WSL repository is exit 4.
            set(
                "DV_LINUX_DIR",
                Path::new("/data").join("no-linux-build").to_str().unwrap()
            ),
            // The setup checks reach no distro.
            set("DV_ONLY_DISTROS", ""),
            // `env` comes last, so a scenario can make the child Claude Code on purpose.
            set("CLAUDECODE", "1"),
            unset("RUST_LOG"),
        ]
    );
    // An app sandboxed by its own `DV_WSL_HOME` passes it, and the Linux build, on.
    let sandboxed = child_env(Some(overlay), Path::new("/data"), &[], true);
    assert!(
        !sandboxed.iter().any(|(name, _)| name == "DV_LINUX_DIR"),
        "{sandboxed:?}"
    );
}

#[test]
fn the_overlay_is_the_deepest_seeded_repo_holding_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let api = dir.path().join("repos").join("api");
    let nested = api.join("vendor").join("lib");
    std::fs::create_dir_all(api.join("src")).unwrap();
    std::fs::create_dir_all(nested.join("src")).unwrap();
    let overlay = |name: &str| -> EnvOverlay { vec![(name.into(), Some("1".into()))] };
    let roots = || {
        vec![
            (api.clone(), overlay("API")),
            (nested.clone(), overlay("NESTED")),
        ]
    };

    assert_eq!(overlay_for(&api, roots()), Some(overlay("API")));
    assert_eq!(overlay_for(&api.join("src"), roots()), Some(overlay("API")));
    assert_eq!(
        overlay_for(&nested.join("src"), roots()),
        Some(overlay("NESTED"))
    );
    // Spelled another way: a `..` and the other separator.
    let spelled = PathBuf::from(format!(
        "{}/src/../src",
        api.display().to_string().replace('\\', "/")
    ));
    assert_eq!(overlay_for(&spelled, roots()), Some(overlay("API")));
    assert_eq!(overlay_for(dir.path(), roots()), None);
    assert_eq!(overlay_for(&dir.path().join("missing"), roots()), None);
}

#[test]
fn a_child_runs_with_its_stdin_environment_and_exit_code() {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
    repo.config("dv.answer", r#"{"schema": 1, "a": [1, 2]}"#);

    // stdout that parses is JSON too.
    let output = git(&repo, &["config", "--get", "dv.answer"], None, &[]);
    assert_eq!(output.exit, Some(0), "{output:?}");
    assert_eq!(
        output.to_json(),
        json!({
            "exit": 0,
            "stdout": "{\"schema\": 1, \"a\": [1, 2]}\n",
            "stderr": "",
            "json": {"schema": 1, "a": [1, 2]},
        })
    );

    // stdin reaches the child and is closed after.
    let output = git(&repo, &["hash-object", "--stdin"], Some("hello\n"), &[]);
    assert_eq!(output.stdout, "ce013625030ba8dba906f756967f9e9ca394464a\n");
    assert_eq!(output.to_json()["json"], Value::Null);

    // The fixture's environment, then `env` over it.
    let output = git(&repo, &["var", "GIT_AUTHOR_IDENT"], None, &[]);
    assert!(output.stdout.starts_with("dv fixture <"), "{output:?}");
    let output = git(
        &repo,
        &["var", "GIT_AUTHOR_IDENT"],
        None,
        &[("GIT_AUTHOR_NAME", "claude-code")],
    );
    assert!(output.stdout.starts_with("claude-code <"), "{output:?}");

    // A failure: its exit code and stderr.
    let output = git(&repo, &["rev-parse", "--verify", "no-such-rev"], None, &[]);
    assert_eq!(output.exit, Some(128));
    assert!(output.stderr.contains("fatal"), "{output:?}");
    assert_eq!(output.to_json()["json"], Value::Null);

    // A program that isn't there fails to start, naming the command.
    let request = Request {
        args: strings(&["status"]),
        cwd: repo.path().to_path_buf(),
        stdin: None,
        env: vec![],
    };
    let error = Running::start(
        Path::new("dv-no-such-program"),
        &request,
        &EnvOverlay::new(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("couldn't start `dv status`"), "{error}");
}

#[test]
fn children_are_collected_by_key_and_killed_at_quit() {
    let (exe, request) = sleeper();
    let mut children = Children::default();
    let wait = Key::Spawned("wait".into());
    children
        .insert(wait.clone(), start(&exe, &request))
        .unwrap();
    assert!(children.contains(&wait));
    // A second child under the same name is refused (and killed as it is dropped).
    let error = children
        .insert(wait.clone(), start(&exe, &request))
        .unwrap_err()
        .to_string();
    assert!(error.contains("already running"), "{error}");

    // It runs: nothing to collect, and `state.cli` says so.
    assert!(children.take_exited(&wait).unwrap().is_none());
    let state = children.spawned_state();
    assert_eq!(state.len(), 1);
    assert_eq!(state[0]["name"], "wait");
    assert_eq!(state[0]["running"], true);
    assert_eq!(state[0]["args"][0], "--exact");
    let error = children
        .take_exited(&Key::Spawned("other".into()))
        .unwrap_err()
        .to_string();
    assert!(error.contains("no cli_spawn named \"other\""), "{error}");
    assert!(error.contains("unjoined: wait"), "{error}");

    // A `cli` run is collected once it exits and isn't listed in `state.cli`.
    let repo = FixtureRepo::new();
    let run = children.next_run();
    let version = Request {
        args: strings(&["--version"]),
        cwd: repo.path().to_path_buf(),
        stdin: None,
        env: vec![],
    };
    children
        .insert(run.clone(), start(Path::new("git"), &version))
        .unwrap();
    assert_eq!(children.spawned_state().len(), 1);
    let deadline = Instant::now() + Duration::from_secs(20);
    let (running, status) = loop {
        if let Some(exited) = children.take_exited(&run).unwrap() {
            break exited;
        }
        assert!(Instant::now() < deadline, "git --version didn't exit");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(running.output(status).stdout.starts_with("git version"));
    assert!(!children.contains(&run));

    // Quitting kills what still runs, at once (the child would sleep a minute).
    let started = Instant::now();
    assert_eq!(children.kill_all(), 1);
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(children.spawned_state(), Vec::<Value>::new());
    assert_eq!(children.kill_all(), 0);

    // Killing one by hand ends it too: it reports an exit status.
    let mut running = start(&exe, &request);
    assert_eq!(running.exited().unwrap(), None);
    running.kill();
    let status = running.exited().unwrap().expect("killed");
    assert!(!status.success());
}
