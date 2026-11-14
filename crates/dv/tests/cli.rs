//! The built `dv` as a terminal or an agent meets it (docs/PLAN.md §10, §13.1): one binary
//! whose headless commands print and exit without any window, with the documented exit codes,
//! stdout and stderr.
//!
//! Only headless commands (and `--help`, `--version`) are run here: `dv` alone or `dv open`
//! would start the app on the owner's desktop, except in the headless build
//! (`--no-default-features`), which has no window to open. These tests run in every shape. Every child gets the fixture's git environment
//! and none of this process's `CLAUDECODE`, `DV_AUTHOR`, `DV_MODEL` or `DV_REVIEW` (Claude Code exports
//! `CLAUDECODE` to the commands it runs). None crosses into a WSL distro: each child's
//! `DV_LINUX_DIR` names a folder without a Linux build, so a WSL repository stops at exit 4
//! before any `wsl.exe` starts (`tests/wsl_cli.rs` crosses for real, sandboxed).

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use dv_test_support::FixtureRepo;
use pretty_assertions::assert_eq;
use serde_json::Value;

/// `dv <args>` in `cwd`, with the fixture's git environment and `env` on top.
fn dv(repo: &FixtureRepo, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    command(repo, cwd, args, env)
        .stdin(Stdio::null())
        .output()
        .expect("dv runs")
}

/// `dv <args>` in the work tree with `stdin` written to it through a real pipe.
fn dv_piped(repo: &FixtureRepo, args: &[&str], stdin: &str) -> Output {
    let mut child = command(repo, repo.path(), args, &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("dv starts");
    let mut pipe = child.stdin.take().unwrap();
    pipe.write_all(stdin.as_bytes()).unwrap();
    drop(pipe);
    child.wait_with_output().expect("dv runs")
}

fn command(repo: &FixtureRepo, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dv"));
    command
        .args(args)
        .current_dir(cwd)
        .env_remove("CLAUDECODE")
        .env_remove("DV_AUTHOR")
        .env_remove("DV_MODEL")
        .env_remove("DV_REVIEW")
        .env_remove("DV_WSL_HOME")
        .env("DV_LINUX_DIR", no_linux_build())
        // The setup checks' seams (PLAN §12): never the registry, GitHub or the real home.
        .env("DV_USER_PATH_FILE", repo.home().join("user-path.txt"))
        .env("DV_GH", repo.home().join("no-such-gh"))
        .env("USERPROFILE", repo.home())
        .env_remove("CODEX_HOME")
        .env_remove("RUST_LOG");
    for (name, value) in repo.env_overlay() {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    command.envs(env.iter().copied());
    command
}

/// A folder without a Linux build of dv: what keeps these tests out of every WSL distro.
fn no_linux_build() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("no-linux-build")
}

fn fixture() -> FixtureRepo {
    let repo = FixtureRepo::new();
    repo.write("src/app.ts", "export const a = 1;\n");
    repo.commit("base");
    repo.write("src/app.ts", "export const a = 2;\n");
    repo
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {:?}", stdout(output)))
}

#[test]
fn help_lists_every_command_and_the_version_prints() {
    let repo = fixture();
    let help = dv(&repo, repo.path(), &["--help"], &[]);
    assert_eq!(help.status.code(), Some(0));
    let text = stdout(&help);
    assert!(text.contains("Usage: dv [OPTIONS] [COMMAND]"), "{text}");
    for command in [
        "open", "status", "review", "comment", "setup", "doctor", "backend",
    ] {
        assert!(
            text.lines()
                .any(|line| line.trim_start().starts_with(&format!("{command} "))),
            "`{command}` in dv --help:\n{text}"
        );
    }
    assert_eq!(stderr(&help), "");

    let version = dv(&repo, repo.path(), &["--version"], &[]);
    assert_eq!(version.status.code(), Some(0));
    assert_eq!(
        stdout(&version),
        format!("dv {}\n", dv_backend::build::version_text())
    );
}

#[test]
fn a_headless_command_prints_and_exits_0() {
    let repo = fixture();
    let output = dv(&repo, repo.path(), &["status", "--json"], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let status = json(&output);
    assert_eq!(status["schema"], 1);
    assert_eq!(status["author"]["kind"], "reviewer");
    assert_eq!(status["author"]["name"], "dv fixture");
    assert_eq!(status["current"], Value::Null);
    assert_eq!(stderr(&output), "", "nothing logged on a success");

    // The process's environment reaches the command: Claude Code's variable makes it an agent.
    let output = dv(&repo, repo.path(), &["status"], &[("CLAUDECODE", "1")]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stdout(&output).contains("acting as: agent claude-code"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn errors_go_to_stderr_and_set_the_exit_code() {
    let repo = fixture();

    // No review to show: exit 2, the error on stderr, and with --json on stdout too.
    let output = dv(&repo, repo.path(), &["review", "show", "--json"], &[]);
    assert_eq!(output.status.code(), Some(2));
    let error = json(&output);
    assert_eq!(error["schema"], 1);
    assert_eq!(error["error"]["code"], "no_review");
    assert!(stderr(&output).starts_with("dv: no open review in this repository"));

    let output = dv(&repo, repo.path(), &["review", "show"], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout(&output), "");

    // clap's own errors honour --json too.
    let output = dv(
        &repo,
        repo.path(),
        &["review", "list", "--bogus", "--json"],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json(&output)["error"]["code"], "usage");
    assert!(stderr(&output).contains("--bogus"));

    // A WSL repository dv can't run in (no Linux build to run there; on Linux, another
    // distro's): exit 4, nothing started.
    let output = dv(
        &repo,
        repo.path(),
        &[
            "status",
            "--repo",
            r"\\wsl.localhost\dv-no-such-distro\home\kyle\api",
            "--json",
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert_eq!(json(&output)["error"]["code"], "backend_unavailable");
    let message = json(&output)["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    if cfg!(windows) {
        assert!(
            message.starts_with("no Linux build of dv for x86_64: ")
                && message.contains("no-linux-build"),
            "{message}"
        );
    } else {
        assert!(
            message.starts_with("dv-no-such-distro:/home/kyle/api is in the WSL distro"),
            "{message}"
        );
    }

    // Outside a repository: exit 1.
    let outside = repo.outside_dir("plain");
    let output = dv(&repo, &outside, &["comment", "list"], &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).starts_with("dv: not a git repository"));

    // `dv doctor`: exit 1 while something needs fixing, the document on stdout all the same.
    let output = dv(&repo, repo.path(), &["doctor", "--json"], &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let doc = json(&output);
    assert_eq!(doc["ok"], false);
    let gh = doc["environments"][0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "gh")
        .unwrap()
        .clone();
    assert_eq!(gh["state"], "missing");
}

#[test]
fn an_agent_answers_through_the_binary_with_a_body_on_stdin() {
    let repo = fixture();
    // The reviewer starts a review and comments; then requests changes.
    let output = dv(&repo, repo.path(), &["review", "new", "--json"], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let review = json(&output)["review"]["id"].as_str().unwrap().to_string();
    let output = dv(
        &repo,
        repo.path(),
        &[
            "comment",
            "add",
            "--file",
            "src/app.ts",
            "--lines",
            "1",
            "--body",
            "Why 2?",
            "--json",
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let thread = json(&output)["thread"]["id"].as_str().unwrap().to_string();
    let output = dv(
        &repo,
        repo.path(),
        &["review", "submit", "--verdict", "request-changes"],
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    // The agent's reply comes over a real pipe, quotes and all.
    let body = "It's \"2\" now: `a = 2`.\n\nSee $HOME and `$(rm -rf /)` untouched.\n";
    let output = dv_piped(
        &repo,
        &[
            "comment",
            "reply",
            &thread,
            "--body-file",
            "-",
            "--author",
            "claude-code",
            "--json",
        ],
        body,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let reply = json(&output);
    assert_eq!(reply["reply"]["body"], body.trim_end());
    assert_eq!(reply["reply"]["author"]["name"], "claude-code");

    // `done` needs an agent; Claude Code's variable is one.
    let output = dv(&repo, repo.path(), &["review", "done"], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).starts_with("dv: `dv review done` is an agent's handoff"));
    let output = dv(
        &repo,
        repo.path(),
        &["review", "done", "--note", "Fixed."],
        &[("CLAUDECODE", "1")],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    // The reviewer hasn't answered: a short wait times out with exit 3.
    let output = dv(
        &repo,
        repo.path(),
        &["review", "wait", &review, "--timeout", "0.3", "--json"],
        &[],
    );
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_eq!(stdout(&output), "{\"schema\":1,\"outcome\":\"timeout\"}\n");
}

#[test]
fn a_snapshot_cache_that_cant_be_written_logs_nothing_on_a_success() {
    let repo = fixture();
    let output = dv(&repo, repo.path(), &["review", "new", "--json"], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let review = json(&output)["review"]["id"].as_str().unwrap().to_string();
    // A directory stands where the cache goes, so every load's write of it fails, as a replace
    // does on Windows while another reader (a waiting agent, the app) holds the file.
    let cache = repo
        .path()
        .join(".git/dv/reviews")
        .join(format!("{review}.state.json"));
    if cache.exists() {
        std::fs::remove_file(&cache).unwrap();
    }
    std::fs::create_dir_all(&cache).unwrap();
    for args in [
        &["review", "show", review.as_str(), "--json"][..],
        &["status", "--json"],
        &["comment", "list", "--json"],
    ] {
        let output = dv(&repo, repo.path(), args, &[]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: {}",
            stderr(&output)
        );
        assert_eq!(json(&output)["schema"], 1);
        assert_eq!(
            stderr(&output),
            "",
            "{args:?}: the cache is only an optimisation"
        );
    }
}

#[test]
fn backend_stdio_serves_until_its_stdin_ends_whatever_still_runs() {
    use std::io::{BufRead as _, BufReader};
    use std::time::{Duration, Instant};

    let repo = fixture();
    let mut child = command(&repo, repo.path(), &["backend", "--stdio"], &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("dv starts");
    let mut requests = child.stdin.take().unwrap();
    let mut replies = BufReader::new(child.stdout.take().unwrap());
    let mut call = |request: Value| -> Value {
        writeln!(requests, "{request}").unwrap();
        let mut line = String::new();
        replies.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line:?}"))
    };

    let proto = dv_backend::proto::PROTO;
    let hello = call(serde_json::json!({"id": 1, "op": "hello", "proto": proto}));
    assert_eq!(hello["id"], 1);
    assert_eq!(hello["ok"]["proto"], proto);
    assert_eq!(hello["ok"]["pid"], child.id());
    assert_eq!(hello["ok"]["build_ok"], Value::Null);
    let env: Vec<(String, Option<String>)> = repo
        .env_overlay()
        .into_iter()
        .map(|(name, value)| {
            let text = |text: std::ffi::OsString| text.into_string().unwrap();
            (text(name), value.map(text))
        })
        .collect();
    let opened = call(serde_json::json!({
        "id": 2, "op": "open", "path": repo.path(), "env": env,
    }));
    let handle = opened["ok"]["repo"].clone();
    assert!(handle.is_u64(), "{opened}");

    // A git that runs for half a minute doesn't keep the process once its input ends.
    writeln!(
        requests,
        "{}",
        serde_json::json!({
            "id": 3, "op": "git", "repo": handle, "args": ["-c", "alias.slow=!sleep 30", "slow"],
        })
    )
    .unwrap();
    drop(requests);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "dv backend --stdio outlived its input"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "it waited for git: {:?}",
        started.elapsed()
    );
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut replies, &mut rest).unwrap();
    assert_eq!(rest, "", "the abandoned call is never answered");
}

/// The headless build (`--no-default-features`) has no window: `dv` and `dv open` exit 2 with
/// where to open the repository instead. In a build with the app these would open a window,
/// so they run only here.
#[cfg(not(feature = "gui"))]
#[test]
fn without_the_app_dv_and_dv_open_say_where_to_open_the_repository() {
    let repo = fixture();
    for args in [&[][..], &["open"], &["open", "."]] {
        let output = dv(&repo, repo.path(), args, &[("WSL_DISTRO_NAME", "")]);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&output).starts_with("dv: this build of dv has no window"),
            "{args:?}: {}",
            stderr(&output)
        );
        assert_eq!(stdout(&output), "");
    }
    let output = dv(
        &repo,
        repo.path(),
        &["open", "--json"],
        &[("WSL_DISTRO_NAME", "Ubuntu")],
    );
    assert_eq!(output.status.code(), Some(2));
    let error = json(&output);
    assert_eq!(error["error"]["code"], "unsupported");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(r"open the repository from Windows: `dv open \\wsl.localhost\Ubuntu\"),
        "{message}"
    );
}
