//! A command for a repository inside a WSL distro (docs/PLAN.md §10.4): which commands cross
//! into the distro, the line the distro's dv gets, the folder it starts in and the variables
//! that cross with it, and the exit codes passed through. On Windows, the exit-4 paths and a
//! text file that isn't UTF-8, which start no `wsl.exe` (or none that boots anything: a distro
//! that isn't there, and the listing that says so); elsewhere a WSL location
//! dv can't reach is exit 4, and inside a distro that distro's locations are local paths; on
//! Linux, the run itself through this machine's `/bin/sh` with a stand-in build. Only when
//! `DV_TEST_WSL_DISTRO` names a distro, the real crossing, in a scratch home under
//! `DV_WSL_HOME` (`crates/dv/tests/wsl_cli.rs` crosses with the built `dv` too).

use std::collections::BTreeSet;
use std::ffi::OsString;

use clap::{CommandFactory as _, Parser as _};
use pretty_assertions::assert_eq;

use super::*;
use crate::reexec::{
    Forwarded, environment, forwarded, reads_repository, shares_console, workdir, wsl_failed,
};

fn words(line: &[&str]) -> Vec<OsString> {
    line.iter().map(OsString::from).collect()
}

/// `dv <line>` as parsed: every line here is one clap takes.
fn parsed(line: &[&str]) -> crate::Cli {
    crate::Cli::try_parse_from(std::iter::once("dv").chain(line.iter().copied()))
        .unwrap_or_else(|error| panic!("{line:?}: {error}"))
}

#[test]
fn the_commands_that_read_a_repository_cross_and_the_others_dont() {
    for line in [
        &["status"][..],
        &["review", "list", "--all"],
        &["review", "show"],
        &["review", "new", "--staged"],
        &["review", "wait", "--timeout", "1"],
        &["review", "done", "--note", "x"],
        &["review", "submit", "--verdict", "approve"],
        &["review", "close", "r-1"],
        &["review", "delete", "r-1"],
        &["comment", "list"],
        &["comment", "show", "t-1"],
        &[
            "comment", "add", "--file", "a", "--lines", "1", "--body", "x",
        ],
        &["comment", "reply", "t-1", "--body", "x"],
        &["comment", "address", "t-1"],
        &["comment", "resolve", "t-1"],
        &["comment", "reopen", "t-1"],
    ] {
        assert!(reads_repository(&parsed(line).command), "{line:?}");
    }
    for line in [
        &["setup", "agents"][..],
        &["setup", "wsl", "Ubuntu"],
        &["setup"],
        &["doctor"],
        &["backend", "--stdio"],
        // Refused before any repository is read.
        &["review", "new", "--pr", "482"],
    ] {
        assert!(!reads_repository(&parsed(line).command), "{line:?}");
    }
}

/// [`forwarded`] of a line clap takes.
fn forward(line: &[&str], distro: &str, repo: &str) -> Forwarded {
    parsed(line);
    forwarded(
        &words(
            &std::iter::once("dv")
                .chain(line.iter().copied())
                .collect::<Vec<_>>(),
        ),
        distro,
        repo,
    )
}

#[test]
fn the_distros_dv_gets_the_line_with_the_repository_by_its_path_there() {
    let api = "/home/kyle/api";
    for line in [
        &["--wsl", "Ubuntu:/home/kyle/api", "status", "--json"][..],
        &["status", "--wsl=Ubuntu:/home/kyle/api", "--json"],
        &[
            "status",
            "--repo",
            r"\\wsl.localhost\Ubuntu\home\kyle\api",
            "--json",
        ],
        &["--repo=api", "status", "--json"],
        &["status", "--json"],
    ] {
        assert_eq!(
            forward(line, "Ubuntu", api),
            Forwarded {
                args: words(&["--repo", api, "status", "--json"]),
                text_file: None,
            },
            "{line:?}"
        );
    }

    // A `--file` is one word; through the distro's share it is its path there, any other
    // stays as given.
    assert_eq!(
        forward(
            &[
                "comment",
                "list",
                "--file",
                r"\\wsl.localhost\ubuntu\home\kyle\api\src\x.rs",
            ],
            "Ubuntu",
            api
        )
        .args,
        words(&[
            "--repo",
            api,
            "comment",
            "list",
            "--file=/home/kyle/api/src/x.rs"
        ])
    );
    assert_eq!(
        forward(
            &["comment", "list", "--file=src/x.rs", "--status", "open"],
            "Ubuntu",
            api
        )
        .args,
        words(&[
            "--repo",
            api,
            "comment",
            "list",
            "--file=src/x.rs",
            "--status",
            "open"
        ])
    );
    assert_eq!(
        forward(
            &["comment", "list", "--file", r"\\wsl$\Debian\srv\x.rs"],
            "Ubuntu",
            api
        )
        .args,
        words(&[
            "--repo",
            api,
            "comment",
            "list",
            r"--file=\\wsl$\Debian\srv\x.rs"
        ])
    );
}

#[test]
fn a_file_starting_with_a_dash_stays_the_options_value_in_the_distro() {
    let api = "/home/kyle/api";
    // Given inline, as clap takes it: two words would make it a flag in the distro.
    for (given, path) in [("--file=-x.rs", "-x.rs"), ("--file=--repo", "--repo")] {
        let crossed = forward(&["comment", "list", given, "--json"], "Ubuntu", api).args;
        assert_eq!(
            crossed,
            words(&["--repo", api, "comment", "list", given, "--json"]),
            "{given}"
        );
        // The distro's dv reads it as the option's value.
        let parsed = crate::Cli::try_parse_from(std::iter::once("dv".into()).chain(crossed))
            .unwrap_or_else(|error| panic!("{given}: {error}"));
        let crate::args::Command::Comment(crate::args::CommentCommand::List(list)) = parsed.command
        else {
            panic!("{given}: not `comment list`");
        };
        assert_eq!(list.file.as_deref(), Some(path), "{given}");
    }
}

#[test]
fn a_text_file_is_read_here_and_handed_over_on_stdin() {
    let api = "/home/kyle/api";
    let reply = forward(
        &[
            "comment",
            "reply",
            "t-1",
            "--body-file",
            r"C:\notes\reply.md",
            "--author",
            "codex",
        ],
        "Ubuntu",
        api,
    );
    assert_eq!(
        reply,
        Forwarded {
            args: words(&[
                "--repo",
                api,
                "comment",
                "reply",
                "t-1",
                "--body-file",
                "-",
                "--author",
                "codex"
            ]),
            text_file: Some(r"C:\notes\reply.md".into()),
        }
    );
    let done = forward(
        &["review", "done", "r-1", "--round", "2", "--note-file=-"],
        "Ubuntu",
        api,
    );
    assert_eq!(done.text_file, Some("-".into()));
    assert_eq!(
        done.args,
        words(&[
            "--repo",
            api,
            "review",
            "done",
            "r-1",
            "--round",
            "2",
            "--note-file",
            "-"
        ])
    );
}

#[test]
fn text_that_looks_like_an_option_and_what_follows_a_lone_double_dash_stay_as_they_are() {
    let api = "/home/kyle/api";
    let line = [
        "comment",
        "add",
        "--file",
        "a",
        "--lines",
        "1",
        "--body",
        "--wsl Ubuntu:/x and --repo y",
    ];
    assert_eq!(
        forward(&line, "Ubuntu", api).args,
        words(&[
            "--repo",
            api,
            "comment",
            "add",
            "--file=a",
            "--lines",
            "1",
            "--body",
            "--wsl Ubuntu:/x and --repo y"
        ])
    );
    assert_eq!(
        forward(
            &["review", "done", "--note=--repo=a b", "--repo", "x"],
            "Ubuntu",
            api
        )
        .args,
        words(&["--repo", api, "review", "done", "--note=--repo=a b"])
    );
    assert_eq!(
        forward(&["comment", "show", "--", "--repo"], "Ubuntu", api).args,
        words(&["--repo", api, "comment", "show", "--", "--repo"])
    );
}

/// The forwarding skips the word after `--body` and `--note` because no other option takes a
/// value that looks like one: clap refuses `--author --repo`, so `--repo` there would be an
/// option. Should another option take such values, the forwarding must learn it.
#[test]
fn only_body_and_note_take_values_that_look_like_options() {
    fn walk(command: &clap::Command, names: &mut BTreeSet<String>) {
        for arg in command.get_arguments() {
            if arg.is_allow_hyphen_values_set() {
                names.insert(arg.get_long().unwrap_or_default().to_string());
            }
        }
        for sub in command.get_subcommands() {
            walk(sub, names);
        }
    }
    let mut names = BTreeSet::new();
    walk(&crate::Cli::command(), &mut names);
    assert_eq!(names, BTreeSet::from(["body".into(), "note".into()]));
}

#[test]
fn the_distros_dv_starts_in_the_working_directory_when_it_is_in_that_distro() {
    let api = "/home/kyle/api";
    let inside = Path::new(r"\\wsl.localhost\Ubuntu\home\kyle\api\src");
    assert_eq!(workdir(inside, "Ubuntu", api), "/home/kyle/api/src");
    assert_eq!(workdir(inside, "ubuntu", api), "/home/kyle/api/src");
    assert_eq!(
        workdir(Path::new(r"\\wsl$\Ubuntu\srv"), "Ubuntu", api),
        "/srv"
    );
    assert_eq!(workdir(Path::new(r"C:\Users\kyle"), "Ubuntu", api), api);
    assert_eq!(
        workdir(Path::new(r"\\wsl.localhost\Debian\srv"), "Ubuntu", api),
        api
    );
}

fn vars(pairs: &[(&str, &str)]) -> Env {
    Env::Vars(
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
    )
}

fn change(name: &str, value: Option<&str>) -> (String, Option<String>) {
    (name.to_string(), value.map(str::to_string))
}

#[test]
fn who_the_command_acts_as_crosses_through_wslenv() {
    assert_eq!(
        environment(&Env::empty()),
        [
            change("CLAUDECODE", None),
            change("DV_AUTHOR", None),
            change("DV_MODEL", None),
            change("DV_REVIEW", None),
            change("WSLENV", None),
        ]
    );
    assert_eq!(
        environment(&vars(&[("CLAUDECODE", "1"), ("DV_REVIEW", "r-1")])),
        [
            change("CLAUDECODE", Some("1")),
            change("DV_AUTHOR", None),
            change("DV_MODEL", None),
            change("DV_REVIEW", Some("r-1")),
            change("WSLENV", Some("CLAUDECODE/u:DV_REVIEW/u")),
        ]
    );
    // The model an agent names crosses with its name.
    assert_eq!(
        environment(&vars(&[
            ("DV_AUTHOR", "codex"),
            ("DV_MODEL", "GPT 6.1 Sol")
        ]))
        .last(),
        Some(&change("WSLENV", Some("DV_AUTHOR/u:DV_MODEL/u")))
    );
    // After what it lists already, which keeps its order; an entry for a name that crosses, in
    // any case and with any flags, gives way to `NAME/u`, so the value crosses verbatim under
    // that name (`/w` wouldn't pass it in, `/p` would translate it, `dv_author` would be
    // another variable there).
    assert_eq!(
        environment(&vars(&[
            (
                "WSLENV",
                "USERPROFILE/p:CLAUDECODE/w:FOO/u:BAR:DV_AUTHOR/p:dv_author"
            ),
            ("CLAUDECODE", "1"),
            ("DV_AUTHOR", "codex"),
        ]))
        .last(),
        Some(&change(
            "WSLENV",
            Some("USERPROFILE/p:FOO/u:BAR:CLAUDECODE/u:DV_AUTHOR/u")
        ))
    );
    assert_eq!(
        environment(&vars(&[
            ("WSLENV", "claudecode/up:DV_REVIEW"),
            ("CLAUDECODE", "1")
        ]))
        .last(),
        Some(&change("WSLENV", Some("DV_REVIEW:CLAUDECODE/u")))
    );
    // A name that doesn't cross (it isn't set) keeps its entry.
    assert_eq!(
        environment(&vars(&[("WSLENV", "USERPROFILE/p:DV_AUTHOR/p")])).last(),
        Some(&change("WSLENV", Some("USERPROFILE/p:DV_AUTHOR/p")))
    );
}

#[test]
fn the_child_shares_the_console_only_when_stdout_and_stderr_are_terminals() {
    let fixture = Fixture::new();
    let input = Input {
        stdin: String::new(),
        terminal: false,
    };
    let (mut ctx, _, _) = fixture.context(fixture.path(), &input);
    for (process, stdout, stderr, shared) in [
        (true, true, true, true),
        // A file or a pipe on either, as `> out 2>&1` and an agent's shell have: dv copies.
        (true, true, false, false),
        (true, false, true, false),
        (true, false, false, false),
        // Captured streams (tests): dv copies.
        (false, true, true, false),
    ] {
        ctx.process_stdio = process;
        ctx.stdout_is_terminal = stdout;
        ctx.stderr_is_terminal = stderr;
        assert_eq!(shares_console(&ctx), shared, "{process} {stdout} {stderr}");
    }
}

#[test]
fn wsl_exes_own_failure_is_the_errors_detail_decoded_never_its_output() {
    let shell = dv_backend::wsl::Shell::local(None).unwrap();
    let said: Vec<u8> = "There is no distribution with the supplied name.\r\nError code: \
                         Wsl/Service/WSL_E_DISTRO_NOT_FOUND\r\n"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let error = wsl_failed(&shell, -1, Some(&said));
    assert_eq!(error.exit, Exit::Unavailable);
    assert_eq!(error.code, "backend_unavailable");
    assert_eq!(
        error.message,
        "wsl.exe couldn't run dv in this machine (exit code -1)"
    );
    assert_eq!(
        error.detail.as_deref(),
        Some(
            "wsl.exe said: There is no distribution with the supplied name. Error code: \
             Wsl/Service/WSL_E_DISTRO_NOT_FOUND"
        )
    );
    // Shared with the console, nothing was kept: no detail.
    assert_eq!(wsl_failed(&shell, -1, None).detail, None);
    assert_eq!(wsl_failed(&shell, -1, Some(b"")).detail, None);
}

#[test]
fn exit_codes_pass_through() {
    for code in 0..=255u8 {
        assert_eq!(Exit::from_code(code).code(), code);
    }
    assert_eq!(Exit::from_code(3), Exit::Timeout);
    assert_eq!(Exit::from_code(4), Exit::Unavailable);
    assert_eq!(Exit::from_code(9), Exit::Other(9));
}

#[test]
fn a_relative_repository_from_a_working_directory_inside_wsl_is_a_wsl_location() {
    // PowerShell can stand in a distro's folder.
    let inside = Path::new(r"\\wsl.localhost\Ubuntu\home\kyle");
    let wsl = |path: &str| dv_core::location::Location::Wsl {
        distro: "Ubuntu".to_string(),
        path: path.to_string(),
    };
    for (repo, path) in [
        (None, "/home/kyle"),
        (Some("."), "/home/kyle"),
        (Some("api"), "/home/kyle/api"),
        (Some("../kyle/api"), "/home/kyle/../kyle/api"),
    ] {
        let global = crate::GlobalArgs {
            repo: repo.map(str::to_string),
            ..crate::GlobalArgs::default()
        };
        assert_eq!(crate::location(&global, inside, None).unwrap(), wsl(path));
    }
}

#[test]
fn inside_a_distro_a_windows_drive_is_where_wsl_mounts_it() {
    let local = |path: &str| dv_core::location::Location::Local(path.into());
    let cwd = Path::new("/tmp/dv-repo/src");
    let at = |repo: &str, here: Option<&str>| {
        let global = crate::GlobalArgs {
            repo: Some(repo.to_string()),
            ..crate::GlobalArgs::default()
        };
        crate::location(&global, cwd, here).unwrap()
    };
    for (repo, mounted) in [
        (r"D:\Software\diff", "/mnt/d/Software/diff"),
        ("D:/Software/diff", "/mnt/d/Software/diff"),
        (r"d:\code\api\", "/mnt/d/code/api"),
        ("C:", "/mnt/c"),
        (r"C:\", "/mnt/c"),
    ] {
        assert_eq!(at(repo, Some("Ubuntu")), local(mounted), "{repo}");
        // Outside a distro a drive's path is no mount: as given on Windows, a relative path
        // on Linux, which has no drives.
        assert_ne!(at(repo, None), local(mounted), "{repo}");
    }
    // A distro's own paths, and its share, are what they were.
    assert_eq!(
        at("/srv/api", Some("Ubuntu")),
        local("/srv/api"),
        "an absolute POSIX path"
    );
    assert_eq!(
        at(r"\\wsl.localhost\Ubuntu\srv\api", Some("Ubuntu")),
        dv_core::location::Location::Wsl {
            distro: "Ubuntu".into(),
            path: "/srv/api".into(),
        }
    );
}

#[test]
fn a_command_to_paste_on_windows_quotes_only_what_it_must() {
    use crate::output::windows_arg;

    for (text, quoted) in [
        ("Ubuntu:/home/kyle/api", "Ubuntu:/home/kyle/api"),
        (r"D:\code\my-api_2.0+x~", r"D:\code\my-api_2.0+x~"),
        // Spaces and the like: double quotes, which PowerShell and cmd both read.
        ("Ubuntu:/home/kyle/my api", r#""Ubuntu:/home/kyle/my api""#),
        (
            r"\\wsl.localhost\Ubuntu\it's here",
            r#""\\wsl.localhost\Ubuntu\it's here""#,
        ),
        ("", r#""""#),
        // `$`, a backtick or a double quote: PowerShell's literal single quotes, a `'` doubled.
        ("Other:/tmp/a$x b", "'Other:/tmp/a$x b'"),
        ("Other:/tmp/a`x", "'Other:/tmp/a`x'"),
        (r#"Other:/tmp/a"b"#, r#"'Other:/tmp/a"b'"#),
        ("/tmp/it's `x`", "'/tmp/it''s `x`'"),
    ] {
        assert_eq!(windows_arg(text), quoted, "{text}");
    }
}

#[test]
fn a_wsl_location_without_a_path_is_a_usage_error() {
    let fixture = Fixture::new();
    fixture
        .run(&["comment", "list", "--wsl", "Ubuntu:relative", "--json"])
        .error(Exit::Usage, "usage");
}

/// On Windows: what can't cross is exit 4 before any `wsl.exe` runs dv.
#[cfg(windows)]
mod windows {
    use pretty_assertions::assert_eq;

    use super::*;

    /// A folder holding a stand-in Linux build for x86_64.
    fn stand_in_builds() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dv-linux-x86_64"), "a stand-in").unwrap();
        dir
    }

    #[test]
    fn without_a_linux_build_a_wsl_repository_is_exit_4_before_wsl_starts() {
        let fixture = Fixture::new();
        let message = format!(
            "no Linux build of dv for x86_64: {} is missing (script/build-linux makes it)",
            no_linux_build().join("dv-linux-x86_64").display()
        );
        for line in [
            &["status", "--repo", r"\\wsl.localhost\Ubuntu\home\kyle\api"][..],
            &["status", "--repo", r"\\wsl$\Ubuntu\home\kyle\api"],
            &["review", "list", "--wsl", "Ubuntu:/home/kyle/api"],
            &["comment", "show", "t-1", "--wsl", "Ubuntu:/home/kyle/api"],
        ] {
            let mut line = line.to_vec();
            line.push("--json");
            let error = fixture
                .run(&line)
                .error(Exit::Unavailable, "backend_unavailable");
            assert_eq!(error["message"], message, "{line:?}");
        }
        // From a working directory inside the distro too.
        let inside = Path::new(r"\\wsl.localhost\Ubuntu\home\kyle");
        for line in [
            &["status", "--json"][..],
            &["review", "list", "--repo", "api", "--json"],
        ] {
            let error = fixture
                .run_in(inside, line)
                .error(Exit::Unavailable, "backend_unavailable");
            assert_eq!(error["message"], message, "{line:?}");
        }
        // What reads no repository runs here as ever.
        let refused = fixture
            .run(&[
                "review",
                "new",
                "--pr",
                "482",
                "--wsl",
                "Ubuntu:/home/kyle/api",
                "--json",
            ])
            .error(Exit::Usage, "unsupported");
        assert!(
            refused["message"]
                .as_str()
                .unwrap()
                .starts_with("GitHub PRs arrive in a later version")
        );
    }

    #[test]
    fn a_sandbox_home_that_isnt_a_posix_path_is_exit_4_before_wsl_starts() {
        // A distro that can't exist: should the guard ever let this through, no real distro
        // gets the stand-in build.
        let builds = stand_in_builds();
        let mut fixture = Fixture::new();
        fixture.env.insert(
            "DV_LINUX_DIR".into(),
            builds.path().to_str().unwrap().into(),
        );
        // What Git Bash makes of `/tmp/x` in a variable passed to a Windows program.
        fixture.env.insert(
            "DV_WSL_HOME".into(),
            "C:/Users/x/AppData/Local/Temp/x".into(),
        );
        let error = fixture
            .run(&["status", "--wsl", "dv-no-such-distro:/srv/api", "--json"])
            .error(Exit::Unavailable, "backend_unavailable");
        assert_eq!(
            error["message"],
            "DV_WSL_HOME must be an absolute path inside the distro, not \
             \"C:/Users/x/AppData/Local/Temp/x\""
        );
    }

    #[test]
    fn a_text_file_that_isnt_utf8_is_named_before_wsl_starts() {
        let builds = stand_in_builds();
        let mut fixture = Fixture::new();
        fixture.env.insert(
            "DV_LINUX_DIR".into(),
            builds.path().to_str().unwrap().into(),
        );
        fixture
            .env
            .insert("DV_WSL_HOME".into(), "/tmp/dv-never-used".into());
        // Windows-1252, as an editor may save it.
        std::fs::write(fixture.path().join("big.md"), b"line \xe9t\xe9\n").unwrap();
        let error = fixture
            .run(&[
                "comment",
                "add",
                "--wsl",
                "dv-no-such-distro:/srv/api",
                "--file",
                "src/a.rs",
                "--lines",
                "3",
                "--body-file",
                "big.md",
                "--json",
            ])
            .error(Exit::Failure, "io");
        assert_eq!(
            error["message"],
            format!(
                "the text from {} isn't UTF-8",
                fixture.path().join("big.md").display()
            )
        );
    }

    #[test]
    fn a_distro_that_isnt_there_is_exit_4() {
        // `wsl.exe` can't start a distro that isn't there, so nothing boots. What it said
        // (UTF-16LE, on its stdout) is the error's detail; `wsl.exe --list` words the error;
        // stdout is the error document alone.
        let builds = stand_in_builds();
        let mut fixture = Fixture::new();
        fixture.env.insert(
            "DV_LINUX_DIR".into(),
            builds.path().to_str().unwrap().into(),
        );
        let ran = fixture.run(&["status", "--wsl", "dv-no-such-distro:/srv/api", "--json"]);
        let error = ran.error(Exit::Unavailable, "backend_unavailable");
        let message = error["message"].as_str().unwrap();
        assert!(
            message.starts_with("there is no WSL distro named dv-no-such-distro (")
                || message.starts_with("WSL isn't installed here"),
            "{message}"
        );
        let detail = error["detail"].as_str().unwrap();
        assert!(
            detail.starts_with("wsl.exe said: ") && !detail.contains('\0'),
            "{detail:?}"
        );
        assert_eq!(ran.stderr, format!("dv: {message}\n  {detail}\n"));
    }

    #[test]
    fn a_command_crosses_into_the_distro_after_installing_dv_there() {
        use crate::tests::sandbox::{Sandbox, builds_dir};

        let Some(sandbox) = Sandbox::get("reexec") else {
            eprintln!("skipped: DV_TEST_WSL_DISTRO isn't set");
            return;
        };
        sandbox.sh(r#"set -e
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
git init -q -b main "$1/repo"
cd "$1/repo"
git config user.name 'dv fixture'
git config user.email dv@example.com
printf 'a\n' > a.txt
git add a.txt
git commit -qm base
"#);
        let repo = format!("{}/repo", sandbox.home);
        let mut fixture = Fixture::new();
        fixture
            .env
            .insert("DV_LINUX_DIR".into(), builds_dir().to_str().unwrap().into());
        fixture
            .env
            .insert("DV_WSL_HOME".into(), sandbox.home.clone());
        let line = [
            "--wsl",
            &format!("{}:{repo}", sandbox.distro),
            "status",
            "--json",
        ];

        // The first command installs this build, then runs; its output is copied into the
        // captured streams.
        let first = fixture.run(&line);
        let json = first.ok_json();
        assert_eq!(
            first.stderr,
            format!("dv: installing dv in {}…\n", sandbox.distro),
            "the distro as named"
        );
        assert_eq!(json["repo"]["root"], repo);
        assert_eq!(json["repo"]["branch"], "main");
        assert_eq!(
            json["author"],
            serde_json::json!({"kind": "reviewer", "name": "dv fixture", "model": null})
        );

        // Installed now; and who the command acts as crosses with it.
        fixture.env.insert("CLAUDECODE".into(), "1".into());
        let second = fixture.run(&line);
        assert_eq!(second.stderr, "");
        assert_eq!(
            second.ok_json()["author"],
            serde_json::json!({"kind": "agent", "name": "claude-code", "model": null})
        );
    }
}

/// Elsewhere dv crosses nothing: a WSL location is exit 4, except inside that distro.
#[cfg(not(windows))]
mod elsewhere {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_wsl_repository_dv_cant_reach_is_exit_4() {
        let fixture = Fixture::new();
        for line in [
            &["status", "--wsl", "Ubuntu:/home/kyle/api", "--json"][..],
            &[
                "status",
                "--repo",
                r"\\wsl.localhost\Ubuntu\home\kyle\api",
                "--json",
            ],
        ] {
            let error = fixture
                .run(line)
                .error(Exit::Unavailable, "backend_unavailable");
            // No WSL here: only dv on Windows reaches it.
            assert_eq!(
                error["message"],
                "Ubuntu:/home/kyle/api is in the WSL distro Ubuntu, which only dv on Windows \
                 reaches: run the command from Windows (`dv.exe --wsl Ubuntu:/home/kyle/api …`), \
                 which runs it inside Ubuntu"
            );
        }
        // A path that needs quoting to paste is quoted.
        let error = fixture
            .run(&["status", "--wsl", "Ubuntu:/home/kyle/my api", "--json"])
            .error(Exit::Unavailable, "backend_unavailable");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains(r#"(`dv.exe --wsl "Ubuntu:/home/kyle/my api" …`)"#),
            "{error}"
        );
    }

    /// Inside a distro, another distro is reached from Windows: `wsl -d` isn't a command there,
    /// and the other distro may have no dv to run.
    #[cfg(target_os = "linux")]
    #[test]
    fn inside_a_distro_another_distro_is_reached_from_windows() {
        let mut fixture = Fixture::new();
        fixture
            .env
            .insert("WSL_DISTRO_NAME".into(), "Ubuntu".into());
        let error = fixture
            .run(&["status", "--wsl", "docker-desktop:/tmp", "--json"])
            .error(Exit::Unavailable, "backend_unavailable");
        assert_eq!(
            error["message"],
            "docker-desktop:/tmp is in the WSL distro docker-desktop, which dv in Ubuntu can't \
             reach: run the command from Windows (`dv.exe --wsl docker-desktop:/tmp …`), which \
             runs it inside docker-desktop"
        );
        // The command to paste is quoted as PowerShell reads it: `$` and a backtick, which it
        // expands inside double quotes, in its literal single quotes.
        for (location, quoted) in [
            ("Other:/tmp/a b", r#""Other:/tmp/a b""#),
            ("Other:/tmp/a$x b", "'Other:/tmp/a$x b'"),
            ("Other:/tmp/a`x", "'Other:/tmp/a`x'"),
            ("Other:/tmp/it's $x", "'Other:/tmp/it''s $x'"),
        ] {
            let error = fixture
                .run(&["status", "--wsl", location, "--json"])
                .error(Exit::Unavailable, "backend_unavailable");
            let message = error["message"].as_str().unwrap();
            assert!(
                message.contains(&format!("(`dv.exe --wsl {quoted} …`)")),
                "{message}"
            );
        }
    }

    /// Inside a distro, a Windows drive's path names that drive as WSL mounts it, not a folder
    /// under the working directory.
    #[cfg(target_os = "linux")]
    #[test]
    fn inside_a_distro_a_windows_drive_names_its_mount() {
        let mut fixture = Fixture::new();
        fixture
            .env
            .insert("WSL_DISTRO_NAME".into(), "Ubuntu".into());
        for repo in [
            r"Q:\dv-no-such\repo",
            "Q:/dv-no-such/repo",
            r"q:\dv-no-such\repo",
        ] {
            let error = fixture
                .run(&["status", "--repo", repo, "--json"])
                .error(Exit::Failure, "not_a_repository");
            assert_eq!(
                error["message"], "no such directory: /mnt/q/dv-no-such/repo",
                "{repo}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inside_a_distro_its_own_locations_are_local_paths() {
        let mut fixture = Fixture::new();
        let here = fixture.run(&["status", "--json"]).ok_json();
        let root = here["repo"]["root"].as_str().unwrap().to_string();
        fixture
            .env
            .insert("WSL_DISTRO_NAME".into(), "Ubuntu".into());
        let unc = format!(r"\\wsl.localhost\Ubuntu{}", root.replace('/', r"\"));
        for line in [
            &["status", "--wsl", &format!("ubuntu:{root}"), "--json"][..],
            &["status", "--repo", &unc, "--json"],
        ] {
            assert_eq!(
                fixture.run(line).ok_json()["repo"]["root"],
                root,
                "{line:?}"
            );
        }
        // Another distro's are still out of reach.
        fixture
            .run(&["status", "--wsl", &format!("Debian:{root}"), "--json"])
            .error(Exit::Unavailable, "backend_unavailable");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inside_a_distro_a_file_spelled_through_its_share_is_its_path_there() {
        use dv_core::target::Target;

        let mut fixture = Fixture::new();
        let review = fixture.review(Target::Working);
        let thread = fixture.thread(&review, "src/app.ts", 10, 10, "Why a cache?");
        fixture
            .env
            .insert("WSL_DISTRO_NAME".into(), "Ubuntu".into());
        let root = fixture.path().to_str().unwrap().to_string();
        let unc = |distro: &str| {
            format!(
                r"\\wsl.localhost\{distro}{}\src\app.ts",
                root.replace('/', r"\")
            )
        };
        let listed = |file: &str| -> Vec<String> {
            fixture
                .run(&["comment", "list", "--file", file, "--json"])
                .ok_json()["threads"]
                .as_array()
                .unwrap()
                .iter()
                .map(|thread| thread["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(listed(&unc("ubuntu")), [thread.as_str()]);
        assert_eq!(
            listed(&format!(
                r"\\wsl$\Ubuntu{}\src\app.ts",
                root.replace('/', r"\")
            )),
            [thread.as_str()]
        );
        // Another distro's spelling names nothing here.
        assert_eq!(listed(&unc("Debian")), Vec::<String>::new());
        // A comment on it lands on the file.
        let added = fixture
            .run(&[
                "comment",
                "add",
                "--file",
                &unc("Ubuntu"),
                "--lines",
                "3",
                "--body",
                "And here?",
                "--json",
            ])
            .ok_json();
        assert_eq!(added["thread"]["path"], "src/app.ts");
    }
}

/// The run itself, through this machine's `/bin/sh` (`Shell::local`) in a scratch home, with a
/// stand-in build that says how it was started.
#[cfg(target_os = "linux")]
mod local {
    use std::path::PathBuf;

    use dv_backend::install::LinuxBuilds;
    use dv_backend::wsl::{Arch, Shell};
    use pretty_assertions::assert_eq;

    use super::*;

    /// A scratch home and a folder of stand-in builds.
    struct Scratch {
        _dir: tempfile::TempDir,
        home: PathBuf,
        builds: PathBuf,
    }

    fn scratch(arch: Arch) -> Scratch {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let builds = dir.path().join("builds");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&builds).unwrap();
        std::fs::write(
            builds.join(arch.file_name()),
            "#!/bin/sh\n\
             printf 'cwd=%s\\n' \"$PWD\"\n\
             for a; do printf '[%s]\\n' \"$a\"; done\n\
             printf 'stdin='; cat; printf '\\n'\n\
             printf 'env=%s,%s,%s\\n' \"${CLAUDECODE-}\" \"${DV_AUTHOR-}\" \"${DV_REVIEW-}\"\n\
             exit 3\n",
        )
        .unwrap();
        Scratch {
            _dir: dir,
            home,
            builds,
        }
    }

    impl Scratch {
        fn shell(&self) -> Shell {
            Shell::local(Some(self.home.to_str().unwrap().to_string())).unwrap()
        }

        fn builds(&self) -> LinuxBuilds {
            LinuxBuilds::find(&self.builds).unwrap()
        }
    }

    /// `line` run by [`crate::reexec::run_in`] on the repository at `repo`, with `stdin` and the
    /// fixture's environment.
    fn run(scratch: &Scratch, fixture: &Fixture, line: &[&str], repo: &str, stdin: &str) -> Ran {
        let input = Input {
            stdin: stdin.to_string(),
            terminal: false,
        };
        let (mut ctx, stdout, stderr) = fixture.context(fixture.path(), &input);
        ctx.args = words(
            &std::iter::once("dv")
                .chain(line.iter().copied())
                .collect::<Vec<_>>(),
        );
        let exit = crate::reexec::run_in(
            &scratch.shell(),
            &scratch.builds(),
            "Ubuntu",
            repo,
            &mut ctx,
        )
        .unwrap_or_else(|error| crate::output::error(&mut ctx, false, &error));
        Ran {
            exit,
            stdout: stdout.text(),
            stderr: stderr.text(),
        }
    }

    #[test]
    fn a_missing_build_is_installed_then_the_command_runs_with_its_text_on_stdin() {
        let scratch = scratch(Arch::host());
        let mut fixture = Fixture::new();
        fixture.env.insert("CLAUDECODE".into(), "1".into());
        fixture.env.insert("DV_REVIEW".into(), "r-1".into());
        let repo = scratch.home.to_str().unwrap();
        let line = [
            "--wsl",
            "Ubuntu:/elsewhere",
            "comment",
            "reply",
            "t-1",
            "--body-file",
            "-",
            "--json",
        ];
        let first = run(&scratch, &fixture, &line, repo, "the reply\n");
        assert_eq!(first.stderr, "dv: installing dv in this machine…\n");
        assert_eq!(first.exit, Exit::Timeout, "the build's own exit code");
        assert_eq!(
            first.stdout,
            format!(
                "cwd={repo}\n[--repo]\n[{repo}]\n[comment]\n[reply]\n[t-1]\n[--body-file]\n[-]\n\
                 [--json]\nstdin=the reply\n\nenv=1,,r-1\n"
            )
        );

        // Installed: the next command runs at once, with nothing on stdin when it reads none.
        let second = run(&scratch, &fixture, &["status"], repo, "unread");
        assert_eq!(second.stderr, "");
        assert_eq!(
            second.stdout,
            format!("cwd={repo}\n[--repo]\n[{repo}]\n[status]\nstdin=\nenv=1,,r-1\n")
        );
    }

    #[test]
    fn both_streams_into_one_file_keep_their_lines_whole() {
        // The distro's dv says why on stderr and writes its error document on stdout, as
        // every `--json` error does; the caller sends both to one file (`> out 2>&1`).
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let folder = dir.path().join("builds");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(Arch::host().file_name()),
            "#!/bin/sh\n\
             printf 'dv: the review id: invalid review id\\n' >&2\n\
             printf '{\"schema\":1,\"error\":{\"code\":\"usage\"}}\\n'\n\
             exit 2\n",
        )
        .unwrap();
        let shell = Shell::local(Some(home.to_str().unwrap().to_string())).unwrap();
        let builds = LinuxBuilds::find(&folder).unwrap();
        let fixture = Fixture::new();
        let input = Input {
            stdin: String::new(),
            terminal: false,
        };
        let (mut ctx, file, _) = fixture.context(fixture.path(), &input);
        ctx.stderr = Box::new(file.clone());
        ctx.args = words(&["dv", "review", "show", "r-nope", "--json"]);
        dv_backend::install::install(&shell, builds.get(Arch::host()).unwrap(), &|_, _| {})
            .unwrap();
        let exit = crate::reexec::run_in(&shell, &builds, "Ubuntu", "/srv/api", &mut ctx).unwrap();
        assert_eq!(exit, Exit::Usage);
        assert_eq!(
            file.text(),
            "dv: the review id: invalid review id\n{\"schema\":1,\"error\":{\"code\":\"usage\"}}\n"
        );
    }

    #[test]
    fn a_text_file_is_read_relative_to_the_working_directory() {
        let scratch = scratch(Arch::host());
        let fixture = Fixture::new();
        std::fs::write(fixture.path().join("note.md"), "- done\n").unwrap();
        let repo = scratch.home.to_str().unwrap();
        let ran = run(
            &scratch,
            &fixture,
            &["review", "done", "--note-file", "note.md"],
            repo,
            "",
        );
        assert!(
            ran.stdout.contains("[--note-file]\n[-]\nstdin=- done\n"),
            "{}",
            ran.stdout
        );

        let missing = run(
            &scratch,
            &fixture,
            &["review", "done", "--note-file", "missing.md"],
            repo,
            "",
        );
        assert_eq!(missing.exit, Exit::Failure);
        assert!(
            missing.stderr.starts_with("dv: reading ") && missing.stdout.is_empty(),
            "{missing:?}"
        );
    }

    #[test]
    fn a_text_file_that_isnt_utf8_is_named_and_nothing_runs() {
        let scratch = scratch(Arch::host());
        let fixture = Fixture::new();
        std::fs::write(fixture.path().join("big.md"), b"line \xe9t\xe9\n").unwrap();
        let ran = run(
            &scratch,
            &fixture,
            &["review", "done", "--note-file", "big.md"],
            scratch.home.to_str().unwrap(),
            "",
        );
        assert_eq!(ran.exit, Exit::Failure);
        assert_eq!(
            ran.stderr,
            format!(
                "dv: the text from {} isn't UTF-8\n",
                fixture.path().join("big.md").display()
            )
        );
        assert_eq!(ran.stdout, "", "the distro's dv never ran");
    }

    #[test]
    fn without_a_build_for_the_machine_it_is_exit_4_naming_the_file() {
        let other = if Arch::host() == Arch::X86_64 {
            Arch::Aarch64
        } else {
            Arch::X86_64
        };
        let scratch = scratch(other);
        let fixture = Fixture::new();
        let ran = run(
            &scratch,
            &fixture,
            &["status"],
            scratch.home.to_str().unwrap(),
            "",
        );
        assert_eq!(ran.exit, Exit::Unavailable);
        assert_eq!(
            ran.stderr,
            format!(
                "dv: {}\n",
                dv_backend::wsl::WslError::NoBuild {
                    arch: Arch::host(),
                    path: scratch.builds.join(Arch::host().file_name()),
                }
            )
        );
    }
}
