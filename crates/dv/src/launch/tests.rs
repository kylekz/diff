//! Where the app runs and what it opens (docs/PLAN.md §10.1, §13.1), as pure decisions: no test
//! ever starts the app.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use clap::Parser as _;
use dv_core::review::{Author, ReviewId, ReviewStore, ops};
use dv_core::target::Target;
use dv_test_support::FixtureRepo;
use pretty_assertions::assert_eq;

use super::*;
use crate::cli::Command;

/// `dv <args>` as a GUI command line, with `open`'s arguments when it was `dv open`.
fn gui(args: &[&str]) -> (Cli, Option<OpenArgs>) {
    let mut cli = Cli::try_parse_from(std::iter::once("dv").chain(args.iter().copied()))
        .unwrap_or_else(|error| panic!("{args:?}: {error}"));
    match cli.command.take() {
        None => (cli, None),
        Some(Command::Open(open)) => (cli, Some(open)),
        Some(other) => panic!("{args:?} is headless: {other:?}"),
    }
}

fn startup_of(args: &[&str], cwd: &Path) -> Result<Startup, CliError> {
    let (cli, open) = gui(args);
    startup(&cli, open.as_ref(), cwd, None, &|_| false)
}

fn cwd() -> PathBuf {
    std::env::temp_dir().join("dv-launch-tests")
}

const REVIEW: &str = "r-01k6zzzzzzzzzzzzzzzzzzzzzz";

#[test]
fn the_app_detaches_unless_automated_or_told_not_to() {
    assert_eq!(launch(&gui(&[]).0, Entry::Console), Launch::Detach);
    assert_eq!(
        launch(&gui(&["open", "."]).0, Entry::Console),
        Launch::Detach
    );
    assert_eq!(
        launch(&gui(&["--no-detach"]).0, Entry::Console),
        Launch::InProcess
    );
    assert_eq!(
        launch(&gui(&["--no-detach", "open", "."]).0, Entry::Console),
        Launch::InProcess
    );
    assert_eq!(
        launch(&gui(&["--automation"]).0, Entry::Console),
        Launch::InProcess
    );
    assert_eq!(
        launch(&gui(&["--automation-pipe", "dv-x"]).0, Entry::Console),
        Launch::InProcess
    );
}

#[test]
fn dv_gui_always_runs_the_app_in_its_own_process() {
    for line in [&[][..], &["open", "."], &["--no-detach"]] {
        assert_eq!(
            launch(&gui(line).0, Entry::Gui),
            Launch::InProcess,
            "{line:?}"
        );
    }
}

#[test]
fn the_detached_app_is_dv_gui_beside_a_packaged_dv_and_itself_otherwise() {
    let exe = Path::new(r"C:\Users\me\AppData\Local\dv\current\dv.exe");
    let gui = exe.with_file_name(GUI_EXE);
    let with_gui = detached_program(exe, true, |path| path == gui);
    if cfg!(any(windows, target_os = "macos")) {
        assert_eq!(with_gui, gui);
    } else {
        // Linux has one binary for both.
        assert_eq!(with_gui, exe);
    }
    // A packaged build without it: this executable again, as `--no-detach`.
    assert_eq!(detached_program(exe, true, |_| false), exe);
    // A development build never starts a `dv-gui.exe` beside it, which may be an older
    // build's (`cargo build --bin dv`).
    assert_eq!(detached_program(exe, false, |path| path == gui), exe);
}

fn words(line: &[&str]) -> Vec<OsString> {
    line.iter().map(OsString::from).collect()
}

/// The detached line for `dv <args>` run in `cwd`, `current_is_repo` saying whether a bare
/// `dv` is in a repository.
fn detached(args: &[&str], cwd: &Path, current_is_repo: bool) -> Vec<OsString> {
    let startup = startup_of(args, cwd).unwrap();
    let mut line = vec!["dv"];
    line.extend_from_slice(args);
    detached_args(&words(&line), &startup, current_is_repo)
}

#[test]
fn the_detached_copy_runs_in_process_and_names_the_repository_absolutely() {
    let cwd = cwd();
    let repo = cwd.join("repo");
    let review = detached(
        &["--data-dir", r"D:\x", "open", "repo", "--review", REVIEW],
        &cwd,
        false,
    );
    assert_eq!(
        review,
        [
            OsString::from("--data-dir"),
            OsString::from(r"D:\x"),
            OsString::from("--no-detach"),
            OsString::from("open"),
            repo.clone().into_os_string(),
            OsString::from("--review"),
            OsString::from(REVIEW),
        ]
    );
    // That line parses, runs in process, and opens the same review from any folder.
    let mut line = vec![OsString::from("dv")];
    line.extend(review);
    let cli = Cli::try_parse_from(line).unwrap();
    assert_eq!(launch(&cli, Entry::Console), Launch::InProcess);
    let Some(Command::Open(open)) = &cli.command else {
        panic!("{:?}", cli.command);
    };
    let elsewhere = std::env::temp_dir().join("somewhere-else");
    assert_eq!(
        startup(&cli, Some(open), &elsewhere, None, &|_| false).unwrap(),
        Startup::Review(
            Location::Local(repo.clone()),
            ReviewId::parse(REVIEW).unwrap()
        )
    );

    // `dv open .` and `--repo` become `open <absolute path>`; the app's options stay.
    let dot = detached(&["--window-size", "1100x700", "open", "."], &cwd, false);
    assert_eq!(
        &dot[..3],
        &words(&["--window-size", "1100x700", "--no-detach"])[..]
    );
    assert_eq!(dot[3], "open");
    let opened = PathBuf::from(&dot[4]);
    assert!(opened.is_absolute(), "{opened:?}");
    assert_eq!(std::path::absolute(&opened).unwrap(), opened);
    // `.` resolved: the working directory itself.
    assert_eq!(opened, cwd);
    assert_eq!(dot.len(), 5);
    let flag = detached(&["--repo", "repo"], &cwd, false);
    assert_eq!(
        flag,
        [
            OsString::from("--no-detach"),
            OsString::from("open"),
            repo.clone().into_os_string()
        ]
    );
}

#[test]
fn a_bare_dv_passes_its_repository_absolutely_and_nothing_elsewhere() {
    let cwd = cwd();
    assert_eq!(
        detached(&[], &cwd, true),
        [
            OsString::from("--no-detach"),
            OsString::from("open"),
            cwd.clone().into_os_string()
        ]
    );
    assert_eq!(detached(&[], &cwd, false), words(&["--no-detach"]));
    // A WSL repository goes as its share, whatever this folder is.
    let wsl = detached(
        &["open", r"\\wsl.localhost\Ubuntu\home\kyle\api"],
        &cwd,
        false,
    );
    assert_eq!(
        wsl,
        words(&[
            "--no-detach",
            "open",
            r"\\wsl.localhost\Ubuntu\home\kyle\api"
        ])
    );
    assert_eq!(
        open_args(
            &Location::Wsl {
                distro: "Ubuntu".into(),
                path: "/tmp/x".into()
            },
            None
        ),
        words(&["open", r"\\wsl.localhost\Ubuntu\tmp\x"])
    );
}

#[test]
fn only_a_bare_dv_in_a_repository_reads_as_one() {
    let repo = FixtureRepo::new();
    let options = || LocalBackendOptions {
        env: repo.env_overlay(),
    };
    let here = Startup::CurrentRepo(Location::Local(repo.path().to_path_buf()));
    assert!(current_is_repo(&here, options()));
    let empty = tempfile::tempdir().unwrap();
    let elsewhere = Startup::CurrentRepo(Location::Local(empty.path().to_path_buf()));
    assert!(!current_is_repo(&elsewhere, options()));
    let named = Startup::Repo(Location::Local(repo.path().to_path_buf()));
    assert!(!current_is_repo(&named, options()));
}

#[test]
fn bare_dv_opens_the_current_repository_if_it_is_one() {
    let cwd = cwd();
    assert_eq!(
        startup_of(&[], &cwd).unwrap(),
        Startup::CurrentRepo(Location::Local(cwd.clone()))
    );
    // An automation run opens only what the scenario asks for.
    assert_eq!(
        startup_of(&["--automation"], &cwd).unwrap(),
        Startup::Nothing
    );
    // Named on the bare line, it is asked for.
    assert_eq!(
        startup_of(&["--repo", "api"], &cwd).unwrap(),
        Startup::Repo(Location::Local(cwd.join("api")))
    );
}

#[test]
fn dv_open_names_a_repository_or_a_review() {
    let cwd = cwd();
    assert_eq!(
        startup_of(&["open"], &cwd).unwrap(),
        Startup::Repo(Location::Local(cwd.clone()))
    );
    assert_eq!(
        startup_of(&["open", "api"], &cwd).unwrap(),
        Startup::Repo(Location::Local(cwd.join("api")))
    );
    assert_eq!(
        startup_of(&["--automation", "open", "api"], &cwd).unwrap(),
        Startup::Repo(Location::Local(cwd.join("api")))
    );
    assert_eq!(
        startup_of(&["open", r"\\wsl.localhost\Ubuntu\home\kyle\api"], &cwd).unwrap(),
        Startup::Repo(Location::Wsl {
            distro: "Ubuntu".into(),
            path: "/home/kyle/api".into()
        })
    );
    assert_eq!(
        startup_of(&["open", "--wsl", "Ubuntu:/srv/api"], &cwd).unwrap(),
        Startup::Repo(Location::Wsl {
            distro: "Ubuntu".into(),
            path: "/srv/api".into()
        })
    );
    let review = ReviewId::parse(REVIEW).unwrap();
    assert_eq!(
        startup_of(&["open", "api", "--review", REVIEW], &cwd).unwrap(),
        Startup::Review(Location::Local(cwd.join("api")), review.clone())
    );
    assert_eq!(
        startup_of(&["--review", REVIEW], &cwd).unwrap(),
        Startup::Review(Location::Local(cwd.clone()), review)
    );
}

#[test]
fn inside_a_distro_dv_open_takes_a_windows_drive_as_its_mount() {
    let in_ubuntu = |args: &[&str]| {
        let (cli, open) = gui(args);
        startup(
            &cli,
            open.as_ref(),
            Path::new("/home/kyle"),
            Some("Ubuntu"),
            &|_| false,
        )
        .unwrap()
    };
    for args in [
        &["open", r"D:\code\api"][..],
        &["open", "d:/code/api"],
        &["--repo", r"D:\code\api\", "open"],
    ] {
        assert_eq!(
            in_ubuntu(args),
            Startup::Repo(Location::Local(PathBuf::from("/mnt/d/code/api"))),
            "{args:?}"
        );
    }
}

#[test]
fn dv_open_takes_a_pull_request() {
    use dv_core::github::PrRef;
    let cwd = cwd();
    let here = Location::Local(cwd.clone());
    let url = PrRef::Url {
        host: "github.com".into(),
        owner: "kylekz".into(),
        repo: "difftest".into(),
        number: 7,
    };
    for (line, expected) in [
        (&["open", "#482"][..], PrRef::Hash(482)),
        (
            &["open", "kylekz/difftest#7"],
            PrRef::Short {
                owner: "kylekz".into(),
                repo: "difftest".into(),
                number: 7,
            },
        ),
        (
            &["open", "https://github.com/kylekz/difftest/pull/7"],
            url.clone(),
        ),
        // As it is pasted: without its scheme, a tab of the pull request, in angle brackets.
        (
            &["open", "github.com/kylekz/difftest/pull/7/files"],
            url.clone(),
        ),
        (
            &["open", "<https://github.com/kylekz/difftest/pull/7>"],
            url.clone(),
        ),
    ] {
        assert_eq!(
            startup_of(line, &cwd).unwrap(),
            Startup::Pr(here.clone(), expected),
            "{line:?}"
        );
    }
    // By its flag, which no shell reads as a comment: every form `review new --pr` takes,
    // a bare number among them.
    for (reference, expected) in [
        ("482", PrRef::Number(482)),
        ("#482", PrRef::Hash(482)),
        ("https://github.com/kylekz/difftest/pull/7", url.clone()),
    ] {
        assert_eq!(
            startup_of(&["open", "--pr", reference], &cwd).unwrap(),
            Startup::Pr(here.clone(), expected),
            "{reference}"
        );
    }
    // Its repository is the working directory's unless the line names another: the flag's
    // path, `--repo` or `--wsl`, one of them.
    assert_eq!(
        startup_of(&["open", "api", "--pr", "482"], &cwd).unwrap(),
        Startup::Pr(Location::Local(cwd.join("api")), PrRef::Number(482))
    );
    assert_eq!(
        startup_of(&["open", "--pr", "482", "--repo", "api"], &cwd).unwrap(),
        Startup::Pr(Location::Local(cwd.join("api")), PrRef::Number(482))
    );
    let error = startup_of(&["open", "api", "--pr", "482", "--repo", "web"], &cwd).unwrap_err();
    assert_eq!(error.exit, dv_cli::Exit::Usage);
    // Beside the flag, an argument is the path however it reads.
    assert_eq!(
        startup_of(&["open", "#notes", "--pr", "482"], &cwd).unwrap(),
        Startup::Pr(Location::Local(cwd.join("#notes")), PrRef::Number(482))
    );
    assert_eq!(
        startup_of(&["open", "#7", "--pr", "482"], &cwd).unwrap(),
        Startup::Pr(Location::Local(cwd.join("#7")), PrRef::Number(482))
    );
    // A flag that names no pull request is refused.
    let error = startup_of(&["open", "--pr", "seven"], &cwd).unwrap_err();
    assert_eq!(error.exit, dv_cli::Exit::Usage);
    assert!(error.message.starts_with("--pr: "), "{}", error.message);
    assert_eq!(
        startup_of(&["open", "#482", "--repo", "api"], &cwd).unwrap(),
        Startup::Pr(Location::Local(cwd.join("api")), PrRef::Hash(482))
    );
    assert_eq!(
        startup_of(&["open", "#482", "--wsl", "Ubuntu:/home/kyle/api"], &cwd).unwrap(),
        Startup::Pr(
            Location::Wsl {
                distro: "Ubuntu".into(),
                path: "/home/kyle/api".into()
            },
            PrRef::Hash(482)
        )
    );
    // A bare number, `#` alone or followed by more than digits, and a URL that isn't a pull
    // request's are paths, as ever.
    for path in ["482", "#notes", "#", "https://github.com/kylekz/difftest"] {
        assert!(
            matches!(startup_of(&["open", path], &cwd), Ok(Startup::Repo(_))),
            "{path}"
        );
    }
    // An argument that names a path that is there is that path, whatever it reads as.
    for path in ["#482", "kylekz/difftest#7"] {
        let (cli, open) = gui(&["open", path]);
        let there = cwd.join(path);
        assert_eq!(
            startup(&cli, open.as_ref(), &cwd, None, &|asked| asked == there).unwrap(),
            Startup::Repo(Location::Local(there.clone())),
            "{path}"
        );
    }
    // A pull request opens its own review.
    let error = startup_of(&["open", "--pr", "482", "--review", REVIEW], &cwd).unwrap_err();
    assert_eq!(error.exit, dv_cli::Exit::Usage);
    let error = startup_of(&["open", "#482", "--review", REVIEW], &cwd).unwrap_err();
    assert_eq!(error.exit, dv_cli::Exit::Usage);
    assert!(
        error
            .message
            .starts_with("`dv open` takes a pull request or --review, not both"),
        "{}",
        error.message
    );
}

#[test]
fn a_pull_requests_detached_line_names_its_repository_absolutely() {
    use dv_core::github::PrRef;
    let cwd = cwd();
    for (line, reference) in [
        (&["open", "#482"][..], "#482"),
        (&["open", "--pr", "482"], "#482"),
        (&["open", "kylekz/difftest#7"], "kylekz/difftest#7"),
        (
            &["open", "github.com/kylekz/difftest/pull/7/files"],
            "https://github.com/kylekz/difftest/pull/7",
        ),
    ] {
        let detached_line = detached(line, &cwd, false);
        assert_eq!(
            detached_line,
            [
                OsString::from("--no-detach"),
                OsString::from("open"),
                cwd.clone().into_os_string(),
                OsString::from("--pr"),
                OsString::from(reference),
            ],
            "{line:?}"
        );
        // That line parses, runs in process, and opens the same pull request in the same
        // repository from any folder.
        let mut words = vec![OsString::from("dv")];
        words.extend(detached_line);
        let cli = Cli::try_parse_from(words).unwrap();
        assert_eq!(launch(&cli, Entry::Console), Launch::InProcess);
        let Some(Command::Open(open)) = &cli.command else {
            panic!("{:?}", cli.command);
        };
        let elsewhere = std::env::temp_dir().join("somewhere-else");
        let first = startup_of(line, &cwd).unwrap();
        let again = startup(&cli, Some(open), &elsewhere, None, &|_| false).unwrap();
        match (&first, &again) {
            (Startup::Pr(first_at, first_pr), Startup::Pr(again_at, again_pr)) => {
                assert_eq!(first_at, again_at);
                assert_eq!(first_pr.number(), again_pr.number());
            }
            other => panic!("{other:?}"),
        }
    }
    // A WSL repository's by its share.
    assert_eq!(
        open_pr_args(
            &Location::Wsl {
                distro: "Ubuntu".into(),
                path: "/tmp/x".into()
            },
            &PrRef::Number(9)
        ),
        words(&["open", r"\\wsl.localhost\Ubuntu\tmp\x", "--pr", "#9"])
    );
}

#[test]
fn a_pull_requests_flag_survives_the_detached_line() {
    use dv_core::github::PrRef;
    let cwd = cwd();
    // A WSL repository's, by its share.
    let wsl = &["open", "--pr", "482", "--wsl", "Ubuntu:/home/kyle/api"];
    assert_eq!(
        detached(wsl, &cwd, false),
        words(&[
            "--no-detach",
            "open",
            r"\\wsl.localhost\Ubuntu\home\kyle\api",
            "--pr",
            "#482",
        ])
    );
    // A GitHub Enterprise host's URL keeps its host there and back.
    let enterprise = PrRef::Url {
        host: "ghe.example.com".into(),
        owner: "acme".into(),
        repo: "api".into(),
        number: 9,
    };
    let url = "https://ghe.example.com/acme/api/pull/9";
    let line = &["open", "--pr", "ghe.example.com/acme/api/pull/9/files"];
    assert_eq!(
        startup_of(line, &cwd).unwrap(),
        Startup::Pr(Location::Local(cwd.clone()), enterprise.clone())
    );
    let detached_line = detached(line, &cwd, false);
    assert_eq!(
        detached_line[1..],
        [
            OsString::from("open"),
            cwd.clone().into_os_string(),
            OsString::from("--pr"),
            OsString::from(url),
        ]
    );
    let mut again = vec![OsString::from("dv")];
    again.extend(detached_line);
    let cli = Cli::try_parse_from(again).unwrap();
    let Some(Command::Open(open)) = &cli.command else {
        panic!("{:?}", cli.command);
    };
    let elsewhere = std::env::temp_dir().join("somewhere-else");
    assert_eq!(
        startup(&cli, Some(open), &elsewhere, None, &|_| false).unwrap(),
        Startup::Pr(Location::Local(cwd.clone()), enterprise)
    );
    // A flag that names no pull request is refused before any window: nothing, an option's
    // look, a number no pull request has.
    for line in [
        &["open", "--pr", ""][..],
        &["open", "--pr=-x"],
        &["open", "--pr", "0"],
        &["open", "--pr=--repo"],
    ] {
        let error = startup_of(line, &cwd).unwrap_err();
        assert_eq!(error.exit, dv_cli::Exit::Usage, "{line:?}");
        assert!(
            error.message.starts_with("--pr: "),
            "{line:?}: {}",
            error.message
        );
    }
}

#[test]
fn a_pull_requests_repository_is_checked_before_detaching() {
    use dv_core::github::PrRef;
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
    let options = || LocalBackendOptions {
        env: repo.env_overlay(),
    };
    // The repository must open; GitHub is the app's to ask.
    let here = Startup::Pr(Location::Local(repo.path().to_path_buf()), PrRef::Hash(482));
    assert!(check(&here, options()).is_ok());
    let empty = tempfile::tempdir().unwrap();
    let nowhere = Startup::Pr(
        Location::Local(empty.path().to_path_buf()),
        PrRef::Hash(482),
    );
    let error = check(&nowhere, options()).unwrap_err();
    assert_eq!(error.code, "not_a_repository");
    assert!(!current_is_repo(&here, options()));
}

#[test]
fn mistakes_are_refused_before_any_window() {
    let cwd = cwd();
    let error = startup_of(&["open", "api", "--repo", "web"], &cwd).unwrap_err();
    assert_eq!(error.exit, dv_cli::Exit::Usage);
    let error = startup_of(&["open", "--review", "r-bad"], &cwd).unwrap_err();
    assert_eq!(error.exit, dv_cli::Exit::Usage);
    assert!(error.message.starts_with("--review: "));
}

const JSON_REFUSED: &str = "`dv` and `dv open` start the app, which prints no JSON: --json is \
                            for the commands that print (`dv status --json`)";
const AUTHOR_REFUSED: &str = "the app always acts as the reviewer: --author is for the commands \
                              an agent runs (`dv comment reply <id> --author <name>`)";

#[test]
fn the_headless_commands_flags_are_refused_on_an_app_line() {
    let cwd = cwd();
    // An agent asking for JSON gets a usage error (as JSON, `main` reports it so) rather than a
    // window and nothing on stdout; `--author` means nothing to the app, which is the
    // reviewer's.
    for (args, message) in [
        (&["--json"][..], JSON_REFUSED),
        (&["open", ".", "--json"], JSON_REFUSED),
        (&["--author", "codex"], AUTHOR_REFUSED),
        (&["open", "--author", "claude-code"], AUTHOR_REFUSED),
    ] {
        let error = startup_of(args, &cwd).unwrap_err();
        assert_eq!(error.exit, dv_cli::Exit::Usage, "{args:?}");
        assert_eq!(error.code, dv_cli::error::code::USAGE, "{args:?}");
        // One sentence as written, without a wrapped literal's indentation inside it.
        assert_eq!(error.message, message, "{args:?}");
        assert!(!error.message.contains("  "), "{:?}", error.message);
    }
    // The global flags the app does take still work there.
    assert!(startup_of(&["--repo", "api", "--review", REVIEW], &cwd).is_ok());
}

#[test]
fn a_detaching_dv_checks_the_repository_and_the_review_first() {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit("base");
    let options = || LocalBackendOptions {
        env: repo.env_overlay(),
    };
    let backend: Arc<dyn Backend> =
        Arc::new(LocalBackend::open_with(repo.path(), options()).unwrap());
    let review = ops::open_review(
        &ReviewStore::new(backend),
        Target::Working,
        Author::reviewer("dv fixture"),
    )
    .unwrap()
    .review;
    let here = Location::Local(repo.path().to_path_buf());

    assert_eq!(check(&Startup::Repo(here.clone()), options()), Ok(()));
    assert_eq!(
        check(&Startup::Review(here.clone(), review.id.clone()), options()),
        Ok(())
    );
    let missing = ReviewId::generate();
    let error = check(&Startup::Review(here, missing.clone()), options()).unwrap_err();
    assert_eq!(error.code, dv_cli::error::code::NOT_FOUND);
    assert!(
        error
            .message
            .starts_with(&format!("no review {missing} in "))
    );

    let outside = repo.outside_dir("plain");
    let error = check(&Startup::Repo(Location::Local(outside.clone())), options()).unwrap_err();
    assert_eq!(error.code, dv_cli::error::code::NOT_A_REPOSITORY);
    // Bare `dv` outside a repository, and WSL locations, are the app's to deal with.
    assert_eq!(
        check(&Startup::CurrentRepo(Location::Local(outside)), options()),
        Ok(())
    );
    let wsl = Location::Wsl {
        distro: "Ubuntu".into(),
        path: "/srv/api".into(),
    };
    assert_eq!(check(&Startup::Repo(wsl.clone()), options()), Ok(()));
    assert_eq!(
        check(&Startup::Review(wsl, missing.clone()), options()),
        Ok(()),
        "a WSL review is the app's to find, through the distro"
    );
    assert_eq!(check(&Startup::Nothing, options()), Ok(()));
}

/// The role [`detach_probe`] plays in a process [`a_detached_app_leaves_the_callers_output_alone`]
/// starts: `parent` (dv starting the app) or `child` (the app).
const PROBE: &str = "DV_DETACH_PROBE";

/// A file the child stays for, until the test removes it (or 20 s pass): the child is
/// this test executable, which Windows keeps locked while it runs.
const PROBE_MARKER: &str = "DV_DETACH_PROBE_MARKER";

#[test]
fn a_detached_app_leaves_the_callers_output_alone() {
    // This test is a caller that captures `dv open`'s output. It runs this test binary as the
    // parent, which starts a child through `spawn_detached`, as `dv` starts the app, and
    // exits; the child stays until the test is done with it, as the app stays open. The end of
    // the parent's output must come when the parent exits, not when the child does.
    let marker = tempfile::NamedTempFile::new().unwrap();
    let started = Instant::now();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "launch::tests::detach_probe",
            "--ignored",
            "--test-threads=1",
        ])
        .env(PROBE, "parent")
        .env(PROBE_MARKER, marker.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let waited = started.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("detach_probe ... ok"), "{stdout}");
    // Well short of the 20 s a child that held the output would make it wait: the child is
    // still there (its marker is).
    assert!(
        waited < Duration::from_secs(10),
        "the caller waited {waited:?}: the detached child held its output open"
    );
    // Removing the marker ends the child.
    drop(marker);
}

#[test]
#[ignore = "a process `a_detached_app_leaves_the_callers_output_alone` starts"]
fn detach_probe() {
    match std::env::var(PROBE).as_deref() {
        Ok("parent") => {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", "launch::tests::detach_probe", "--ignored"])
                .env(PROBE, "child");
            spawn_detached(child).unwrap();
        }
        Ok("child") => {
            let marker = std::env::var_os(PROBE_MARKER).map(std::path::PathBuf::from);
            let until = Instant::now() + Duration::from_secs(20);
            while marker.as_ref().is_some_and(|marker| marker.exists()) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        _ => {}
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_detached_app_holds_none_of_the_callers_descriptors() {
    use std::os::fd::AsRawFd as _;

    // A descriptor this process inherited without close-on-exec, as a caller's shell leaves
    // one: `dup` makes such a copy of a pipe's end.
    let (reader, _writer) = std::io::pipe().unwrap();
    // SAFETY: `dup` takes no pointers; the copy is closed below.
    let inherited = unsafe { libc::dup(reader.as_raw_fd()) };
    assert!(inherited > 2, "dup: {}", std::io::Error::last_os_error());
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("fd");
    let mut child = std::process::Command::new("/bin/sh");
    child.args([
        "-c",
        r#"if [ -e "/proc/$$/fd/$1" ]; then echo open; else echo closed; fi > "$2""#,
        "sh",
        &inherited.to_string(),
        &out.display().to_string(),
    ]);
    spawn_detached(child).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let said = loop {
        match std::fs::read_to_string(&out) {
            Ok(text) if text.ends_with('\n') => break text,
            _ if Instant::now() > deadline => panic!("the detached child wrote nothing"),
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    // SAFETY: `inherited` is the descriptor `dup` returned above, closed once.
    unsafe { libc::close(inherited) };
    assert_eq!(said.trim(), "closed");
}
