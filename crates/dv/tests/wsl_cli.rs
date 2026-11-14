//! The built `dv` crossing into a real WSL distro from Windows (docs/PLAN.md §10.4): a command
//! for a repository inside the distro runs there, as a terminal or an agent's shell runs it.
//! Windows only, and only when `DV_TEST_WSL_DISTRO` names a distro, sandboxed by `DV_WSL_HOME`
//! (a fresh folder under `/tmp` in it, which `script/check --linux` makes and removes), one
//! test at a time:
//!
//! ```text
//! script/build-linux
//! MSYS_NO_PATHCONV=1 DV_TEST_WSL_DISTRO=Ubuntu DV_WSL_HOME=/tmp/dv-check-1 \
//!     cargo test -p dv --test wsl_cli -- --test-threads=1
//! ```
//!
//! The crossing's output reaches the caller as a terminal, a pipe or a file each get it, both
//! streams sent to one file included (`> out 2>&1`, an agent's shell), and a `--json` error's
//! stdout is its error document alone, also when `wsl.exe` itself fails.
//!
//! Each test works in a home of its own under `DV_WSL_HOME`, removed at its end, and every `dv`
//! it runs gets that home as `DV_WSL_HOME`, so nothing reaches the distro user's own home
//! (CLAUDE.md). `dv` finds the Linux build beside itself in `target/debug`, where
//! `script/build-linux` puts it (or in `DV_LINUX_DIR`). Every child runs without this process's
//! `CLAUDECODE`, `DV_AUTHOR`, `DV_MODEL`, `DV_REVIEW` and `WSLENV`, set only on purpose.

#![cfg(windows)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use dv_backend::wsl::{self, Shell};
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

/// A home of the test's own under `DV_WSL_HOME` in the distro, removed when dropped.
struct Sandbox {
    distro: String,
    home: String,
    shell: Shell,
}

impl Sandbox {
    /// `None` unless `DV_TEST_WSL_DISTRO` is set; then `DV_WSL_HOME` must be a folder under
    /// `/tmp`, or the test fails rather than touch a real home.
    fn get(name: &str) -> Option<Self> {
        let distro = std::env::var("DV_TEST_WSL_DISTRO")
            .ok()
            .filter(|distro| !distro.trim().is_empty())?;
        let root = std::env::var("DV_WSL_HOME").unwrap_or_default();
        let root = wsl::sandbox(Some(root.clone()))
            .ok()
            .flatten()
            .filter(|root| root.starts_with("/tmp/"))
            .unwrap_or_else(|| {
                panic!(
                    "DV_TEST_WSL_DISTRO is set, so DV_WSL_HOME must be a scratch folder under \
                     /tmp in {distro}, not {root:?}"
                )
            });
        let home = format!("{root}/{name}-{}", std::process::id());
        let shell = Shell::wsl(distro.clone(), Some(home.clone())).unwrap();
        let sandbox = Self {
            distro,
            home,
            shell,
        };
        sandbox.sh(r#"rm -rf -- "$1" && mkdir -p -- "$1""#);
        Some(sandbox)
    }

    /// `script` in the distro with the home as `$HOME` and as `$1`; it must succeed.
    fn sh(&self, script: &str) {
        let output = self
            .shell
            .run(script, &[&self.home], Duration::from_secs(60))
            .unwrap();
        assert!(output.status.success(), "{script}: {output:?}");
    }

    /// A repository at `<home>/repo`: `a.txt` committed on `main`, changed in the work tree,
    /// and a folder `sub`; made with no configuration of the user's, its own identity.
    fn repo(&self) -> String {
        self.sh(r#"set -e
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
git init -q -b main "$1/repo"
cd "$1/repo"
git config user.name 'dv fixture'
git config user.email dv@example.com
printf 'a\n' > a.txt
git add a.txt
git commit -qm base
printf 'b\n' > a.txt
mkdir sub
"#);
        format!("{}/repo", self.home)
    }

    /// `path`, a folder in the distro, as Windows reaches it.
    fn unc(&self, path: &str) -> PathBuf {
        PathBuf::from(format!(
            r"\\wsl.localhost\{}{}",
            self.distro,
            path.replace('/', r"\")
        ))
    }

    /// `dv <args>` from this process's working directory (not in the distro).
    fn dv(&self, args: &[&str], env: &[(&str, &str)], stdin: Option<&str>) -> Output {
        self.dv_in(&std::env::current_dir().unwrap(), args, env, stdin)
    }

    /// `dv <args>` as a child of this process, with no agent identity and the home as
    /// `DV_WSL_HOME`.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dv"));
        command
            .args(args)
            .env_remove("CLAUDECODE")
            .env_remove("DV_AUTHOR")
            .env_remove("DV_MODEL")
            .env_remove("DV_REVIEW")
            .env_remove("WSLENV")
            .env_remove("RUST_LOG")
            .env("DV_WSL_HOME", &self.home);
        command
    }

    /// Whether a `dv … <what>` of this home's runs in the distro now.
    fn runs_in_the_distro(&self, what: &str) -> bool {
        self.shell
            .run(
                r#"pgrep -f -- "$1/.local/share/dv/bin/dv-.* $2" > /dev/null"#,
                &[&self.home, what],
                Duration::from_secs(60),
            )
            .unwrap()
            .status
            .success()
    }

    /// `dv <args>` in `cwd` with `env` and `stdin` (else none) through real pipes.
    fn dv_in(
        &self,
        cwd: &Path,
        args: &[&str],
        env: &[(&str, &str)],
        stdin: Option<&str>,
    ) -> Output {
        let mut command = self.command(args);
        command
            .current_dir(cwd)
            .envs(env.iter().copied())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("dv starts");
        if let Some(stdin) = stdin {
            let mut pipe = child.stdin.take().unwrap();
            pipe.write_all(stdin.as_bytes()).unwrap();
        }
        child.wait_with_output().expect("dv runs")
    }

    /// `dv <args>` with stdout and stderr both sent to one file, as `> out 2>&1` sends them
    /// (one handle, duplicated): its exit code and what the file holds.
    fn dv_to_one_file(&self, args: &[&str]) -> (Option<i32>, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let file = std::fs::File::create(&path).unwrap();
        let status = self
            .command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(file.try_clone().unwrap()))
            .stderr(Stdio::from(file))
            .status()
            .expect("dv runs");
        let bytes = std::fs::read(&path).unwrap();
        (
            status.code(),
            String::from_utf8(bytes.clone())
                .unwrap_or_else(|_| panic!("not UTF-8 (wsl.exe's own text?): {bytes:?}")),
        )
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let removed = self
            .shell
            .run(r#"rm -rf -- "$1""#, &[&self.home], Duration::from_secs(60));
        if !removed.is_ok_and(|output| output.status.success()) {
            eprintln!("couldn't remove {} in {}", self.home, self.distro);
        }
    }
}

macro_rules! sandbox {
    ($name:expr) => {
        match Sandbox::get($name) {
            Some(sandbox) => sandbox,
            None => {
                eprintln!("skipped: DV_TEST_WSL_DISTRO isn't set");
                return;
            }
        }
    };
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// stdout as the one JSON document it must be, the exit code `code`.
fn json(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {} stderr: {}",
        stdout(output),
        stderr(output)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {:?}", stdout(output)))
}

#[test]
fn a_command_runs_inside_the_distro_after_installing_dv_there() {
    let sandbox = sandbox!("cli-run");
    let repo = sandbox.repo();
    let wsl = format!("{}:{repo}", sandbox.distro);

    let first = sandbox.dv(&["--wsl", &wsl, "status", "--json"], &[], None);
    let doc = json(&first, 0);
    let installing = stderr(&first);
    assert!(
        installing.starts_with("dv: installing dv in ")
            && installing
                .to_lowercase()
                .ends_with(&format!("{}…\n", sandbox.distro.to_lowercase())),
        "{installing:?}"
    );
    assert_eq!(doc["repo"]["root"], repo);
    assert_eq!(doc["repo"]["branch"], "main");
    assert_eq!(
        doc["author"],
        json!({"kind": "reviewer", "name": "dv fixture", "model": null})
    );

    // Installed now: nothing but the command's own output.
    let again = sandbox.dv(&["--wsl", &wsl, "status", "--json"], &[], None);
    assert_eq!(json(&again, 0)["repo"]["root"], repo);
    assert_eq!(stderr(&again), "");

    // The share's spelling, and a working directory inside the distro, name it too.
    let unc = sandbox.unc(&repo);
    let listed = sandbox.dv(
        &["review", "list", "--repo", unc.to_str().unwrap(), "--json"],
        &[],
        None,
    );
    assert_eq!(json(&listed, 0)["reviews"], json!([]));
    let sub = unc.join("sub");
    for line in [
        &["status", "--json"][..],
        &["status", "--repo", "..", "--json"],
    ] {
        let here = sandbox.dv_in(&sub, line, &[], None);
        assert_eq!(json(&here, 0)["repo"]["root"], repo, "{line:?}");
    }

    // Text output passes through as it is.
    let text = sandbox.dv(&["--wsl", &wsl, "status"], &[], None);
    assert_eq!(text.status.code(), Some(0));
    assert!(
        stdout(&text).starts_with(&format!("repo: {repo} (on main)\n")),
        "{}",
        stdout(&text)
    );
}

#[test]
fn exit_codes_and_text_on_stdin_pass_through() {
    let sandbox = sandbox!("cli-exit");
    let repo = sandbox.repo();
    let wsl = format!("{}:{repo}", sandbox.distro);
    let review = json(
        &sandbox.dv(&["--wsl", &wsl, "review", "new", "--json"], &[], None),
        0,
    )["review"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // `review wait --timeout` runs out inside the distro: exit 3 here.
    let waited = sandbox.dv(
        &[
            "--wsl",
            &wsl,
            "review",
            "wait",
            &review,
            "--timeout",
            "1",
            "--json",
        ],
        &[],
        None,
    );
    assert_eq!(json(&waited, 3), json!({"schema": 1, "outcome": "timeout"}));

    // The text read from stdin here reaches the distro's dv.
    let added = sandbox.dv(
        &[
            "--wsl",
            &wsl,
            "comment",
            "add",
            "--file",
            "a.txt",
            "--lines",
            "1",
            "--body-file",
            "-",
            "--json",
        ],
        &[],
        Some("Why b?\r\n"),
    );
    let thread = json(&added, 0)["thread"].clone();
    assert_eq!(thread["body"], "Why b?");
    let listed = sandbox.dv(&["--wsl", &wsl, "comment", "list", "--json"], &[], None);
    assert_eq!(json(&listed, 0)["threads"][0]["id"], thread["id"]);
    // A `--file` value starting with `-` stays the option's value there.
    let dashed = sandbox.dv(
        &["--wsl", &wsl, "comment", "list", "--file=-a.txt", "--json"],
        &[],
        None,
    );
    assert_eq!(json(&dashed, 0)["threads"], json!([]));

    // An error inside the distro: its exit code and its JSON document.
    let nope = format!("t-01k6{}", "z".repeat(22));
    let missing = sandbox.dv(
        &["--wsl", &wsl, "comment", "show", &nope, "--json"],
        &[],
        None,
    );
    assert_eq!(json(&missing, 1)["error"]["code"], "not_found");
    assert!(stderr(&missing).starts_with("dv: "), "{}", stderr(&missing));
}

#[test]
fn who_the_command_acts_as_and_its_review_cross_with_it() {
    let sandbox = sandbox!("cli-env");
    let repo = sandbox.repo();
    let wsl = format!("{}:{repo}", sandbox.distro);
    let status = |env: &[(&str, &str)]| {
        json(
            &sandbox.dv(&["--wsl", &wsl, "status", "--json"], env, None),
            0,
        )
    };
    assert_eq!(
        status(&[("CLAUDECODE", "1")])["author"],
        json!({"kind": "agent", "name": "claude-code", "model": null})
    );
    // After what `WSLENV` lists already.
    assert_eq!(
        status(&[("DV_AUTHOR", "codex"), ("WSLENV", "USERPROFILE/p")])["author"],
        json!({"kind": "agent", "name": "codex", "model": null})
    );
    // An entry `WSLENV` has for the same name, with other flags or in another case, doesn't
    // keep it from crossing verbatim.
    assert_eq!(
        status(&[("CLAUDECODE", "1"), ("WSLENV", "CLAUDECODE/w")])["author"],
        json!({"kind": "agent", "name": "claude-code", "model": null})
    );
    for listed in ["DV_AUTHOR/p", "dv_author"] {
        assert_eq!(
            status(&[("DV_AUTHOR", "codex"), ("WSLENV", listed)])["author"],
            json!({"kind": "agent", "name": "codex", "model": null}),
            "{listed}"
        );
    }
    assert_eq!(
        status(&[])["author"],
        json!({"kind": "reviewer", "name": "dv fixture", "model": null})
    );

    let review = json(
        &sandbox.dv(&["--wsl", &wsl, "review", "new", "--json"], &[], None),
        0,
    )["review"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let shown = sandbox.dv(
        &["--wsl", &wsl, "review", "show", "--json"],
        &[("DV_REVIEW", &review)],
        None,
    );
    assert_eq!(json(&shown, 0)["review"]["id"], review.as_str());
}

#[test]
fn a_killed_dv_takes_the_distros_dv_with_it() {
    let sandbox = sandbox!("cli-kill");
    let repo = sandbox.repo();
    let wsl = format!("{}:{repo}", sandbox.distro);
    let review = json(
        &sandbox.dv(&["--wsl", &wsl, "review", "new", "--json"], &[], None),
        0,
    )["review"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    // A wait that would last a minute, killed as a tool's timeout kills it: no Ctrl-C, no
    // stdin closing, just the process gone.
    let mut waiting = sandbox
        .command(&["--wsl", &wsl, "review", "wait", &review, "--timeout", "60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("dv starts");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !sandbox.runs_in_the_distro("review wait") {
        assert!(Instant::now() < deadline, "the distro's dv never started");
        std::thread::sleep(Duration::from_millis(200));
    }
    waiting.kill().unwrap();
    waiting.wait().unwrap();
    // Its wsl.exe was in dv's kill-on-close job, and the distro's dv ends with it.
    let deadline = Instant::now() + Duration::from_secs(15);
    while sandbox.runs_in_the_distro("review wait") {
        assert!(
            Instant::now() < deadline,
            "the distro's dv outlived the dv that ran it"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_distro_that_isnt_there_is_exit_4_with_its_json_error_alone_on_stdout() {
    let sandbox = sandbox!("cli-missing");
    // `wsl.exe` can't start a distro that isn't there, so nothing boots. What it says
    // (UTF-16LE) is the error's detail, never stdout's: stdout is the error document alone,
    // which `wsl.exe --list` words.
    let line = ["--wsl", "dv-no-such-distro:/srv/api", "status", "--json"];
    let output = sandbox.dv(&line, &[], None);
    let error = json(&output, 4)["error"].clone();
    assert_eq!(error["code"], "backend_unavailable");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("there is no WSL distro named dv-no-such-distro ("),
        "{error}"
    );
    assert!(
        error["detail"]
            .as_str()
            .unwrap()
            .starts_with("wsl.exe said: "),
        "{error}"
    );
    assert!(
        stderr(&output).starts_with(&format!("dv: {}\n", error["message"].as_str().unwrap())),
        "{}",
        stderr(&output)
    );
    // Both streams into one file: dv's two lines, whole.
    let (code, text) = sandbox.dv_to_one_file(&line);
    assert_eq!(code, Some(4), "{text}");
    let last = text.lines().last().unwrap_or_default();
    let doc: Value = serde_json::from_str(last).unwrap_or_else(|e| panic!("{e}: {text:?}"));
    assert_eq!(doc["error"]["code"], "backend_unavailable");
}

#[test]
fn both_streams_into_one_file_come_out_whole() {
    let sandbox = sandbox!("cli-one-file");
    let repo = sandbox.repo();
    let wsl = format!("{}:{repo}", sandbox.distro);
    // Installed first, so each run below writes only the command's own lines.
    json(
        &sandbox.dv(&["--wsl", &wsl, "status", "--json"], &[], None),
        0,
    );
    // A `--json` error writes both streams: `dv: …` on stderr, the document on stdout.
    // wsl.exe relays them with a write position each, so shared with the file they would
    // overwrite each other; dv copies them itself.
    for run in 0..8 {
        let (code, text) =
            sandbox.dv_to_one_file(&["--wsl", &wsl, "review", "show", "r-nope", "--json"]);
        assert_eq!(code, Some(2), "run {run}: {text:?}");
        let lines: Vec<&str> = text.lines().collect();
        let [said, doc] = lines[..] else {
            panic!("run {run}: {text:?}");
        };
        assert!(said.starts_with("dv: "), "run {run}: {text:?}");
        let doc: Value =
            serde_json::from_str(doc).unwrap_or_else(|e| panic!("run {run}: {e}: {text:?}"));
        assert_eq!(doc["error"]["code"], "usage", "run {run}: {text:?}");
        assert_eq!(
            format!("dv: {}", doc["error"]["message"].as_str().unwrap()),
            said,
            "run {run}"
        );
    }
}

#[test]
fn a_link_folder_dv_cant_write_leaves_the_crossing_working() {
    let sandbox = sandbox!("cli-ro-bin");
    let repo = sandbox.repo();
    sandbox.sh(r#"mkdir -p "$1/.local/bin" && chmod 555 "$1/.local/bin""#);
    let wsl = format!("{}:{repo}", sandbox.distro);
    // The first crossing installs, warns if the link couldn't be made, and runs.
    let first = sandbox.dv(&["--wsl", &wsl, "status", "--json"], &[], None);
    sandbox.sh(r#"chmod 755 "$1/.local/bin""#);
    assert_eq!(json(&first, 0)["repo"]["root"], repo);
    let said = stderr(&first);
    assert!(said.starts_with("dv: installing dv in "), "{said}");
    // Root writes into the folder whatever its mode: then there is nothing to warn about.
    if said.lines().count() > 1 {
        assert!(
            said.lines()
                .nth(1)
                .is_some_and(|line| line.starts_with("dv: warning: couldn't point ")),
            "{said}"
        );
    }
}
