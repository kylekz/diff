//! What can be checked without a distro: `wsl.exe`'s output decoded, the markers, the command
//! lines, the sandbox home; and, on Linux, the launch script and [`launch`] itself run by this
//! machine's `/bin/sh` (`Shell::local`) in a scratch home, installing a stand-in build. The real
//! `wsl.exe` is `tests/wsl.rs`'.

use pretty_assertions::assert_eq;

use super::*;

fn utf16(text: &str, bom: bool) -> Vec<u8> {
    let mut bytes = if bom { vec![0xFF, 0xFE] } else { Vec::new() };
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[test]
fn distro_names_are_read_from_utf16_or_utf8_output() {
    let names = vec!["Ubuntu".to_string(), "docker-desktop".to_string()];
    assert_eq!(
        parse_names(&utf16("Ubuntu\r\ndocker-desktop\r\n", false)),
        names
    );
    assert_eq!(
        parse_names(&utf16("Ubuntu\r\ndocker-desktop\r\n", true)),
        names
    );
    assert_eq!(parse_names(b"Ubuntu\ndocker-desktop\n\n"), names);
    assert_eq!(
        parse_names(&utf16("\r\n  Ubuntu-22.04 \r\n\r\n", false)),
        ["Ubuntu-22.04".to_string()]
    );
    assert_eq!(parse_names(b""), Vec::<String>::new());
}

#[test]
fn utf16_text_split_after_a_newline_still_decodes() {
    // `read_until(b'\n')` splits UTF-16LE's `\n` (0x0A 0x00) from its NUL, so the next part
    // starts on the odd byte.
    let bytes = utf16("There is no distribution.\r\nError code: X\r\n", false);
    let split = bytes.iter().position(|&byte| byte == b'\n').unwrap() + 1;
    assert_eq!(decode(&bytes[..split]).trim(), "There is no distribution.");
    assert_eq!(decode(&bytes[split..]).trim(), "Error code: X");
    assert_eq!(decode("dv-exec x86_64\n".as_bytes()), "dv-exec x86_64\n");
}

#[test]
fn the_launch_markers_are_recognised() {
    assert_eq!(marker("dv-exec x86_64\n"), Some(Marker::Exec(Arch::X86_64)));
    assert_eq!(
        marker("dv-missing aarch64"),
        Some(Marker::Missing(Arch::Aarch64))
    );
    assert_eq!(
        marker("dv-no-build aarch64"),
        Some(Marker::NoBuild(Arch::Aarch64))
    );
    assert_eq!(
        marker("dv-arch riscv64\r"),
        Some(Marker::UnknownArch("riscv64".into()))
    );
    assert_eq!(marker("dv-exec sparc"), None);
    assert_eq!(marker("wsl: a warning"), None);
    assert_eq!(marker("dv-exec"), None);
}

#[test]
fn architectures_have_their_names_and_build_files() {
    assert_eq!(Arch::from_name("amd64"), Some(Arch::X86_64));
    assert_eq!(Arch::from_name("arm64"), Some(Arch::Aarch64));
    assert_eq!(Arch::from_name(std::env::consts::ARCH), Some(Arch::host()));
    assert_eq!(Arch::from_name("i686"), None);
    assert_eq!(Arch::X86_64.file_name(), "dv-linux-x86_64");
    assert_eq!(Arch::Aarch64.file_name(), "dv-linux-aarch64");
}

#[test]
fn the_sandbox_home_must_be_an_absolute_posix_path() {
    // Unset: the distro user's own home.
    assert_eq!(sandbox(None), Ok(None));
    assert!(!is_sandbox(None));
    assert_eq!(
        sandbox(Some("/tmp/dv-home/".into())),
        Ok(Some("/tmp/dv-home".into()))
    );
    assert!(is_sandbox(Some("/tmp/dv-home".into())));
    // Normalised: `.` parts and repeated slashes go.
    assert_eq!(
        sandbox(Some("//tmp/./dv-home//x/.".into())),
        Ok(Some("/tmp/dv-home/x".into()))
    );
    // Set to anything else, never a fallback to the user's own home: an error, and no sandbox
    // an automation run may reach a real distro with.
    for bad in [
        "",
        " ",
        "\t",
        " /tmp/x",
        "/tmp/x ",
        "tmp/x",
        r"C:\Users\x",
        // What Git Bash makes of `/tmp/x` in a variable passed to a Windows program.
        "C:/Users/x/AppData/Local/Temp/x",
        "/tmp/../home/kyle",
        "/tmp/a\nb",
        // A value that isn't Unicode, as `home_var` reads it.
        "/tmp/\u{FFFD}",
        // `/` in every spelling.
        "/",
        "//",
        "/.",
        "/./",
        "/./.",
        "/..",
        "/tmp/..",
        "/tmp/x/../..",
    ] {
        assert_eq!(
            sandbox(Some(bad.into())),
            Err(WslError::BadHome(bad.into())),
            "{bad:?}"
        );
        assert!(!is_sandbox(Some(bad.into())), "{bad:?}");
    }
}

#[test]
fn a_distros_scripts_run_through_wsl_exe_without_a_shell_on_either_side() {
    let shell = Shell::wsl("Ubuntu", Some("/tmp/dv-home".into())).unwrap();
    let command = shell.command("echo \"$1\"", &["a b", "-"]);
    assert_eq!(command.get_program(), WSL_EXE);
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "-d",
            "Ubuntu",
            "--cd",
            "/",
            "--exec",
            "/usr/bin/env",
            "HOME=/tmp/dv-home",
            "/bin/sh",
            "-c",
            "echo \"$1\"",
            "sh",
            "a b",
            "-"
        ]
    );
    assert_eq!(shell.name(), "Ubuntu");

    let own_home = Shell::wsl("Debian", None).unwrap();
    let args: Vec<_> = own_home
        .command("true", &[])
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "-d", "Debian", "--cd", "/", "--exec", "/bin/sh", "-c", "true", "sh"
        ]
    );
    assert_eq!(
        own_home.describe("the script", &["x"]),
        "wsl.exe -d Debian --cd / --exec /bin/sh -c <the script> sh x"
    );

    // The CLI's run shares the caller's console, on the same line.
    let shared = shell.console_command("echo \"$1\"", ["a b", "-"]);
    assert_eq!(shared.get_program(), WSL_EXE);
    assert_eq!(
        shared.get_args().collect::<Vec<_>>(),
        shell
            .command("echo \"$1\"", &["a b", "-"])
            .get_args()
            .collect::<Vec<_>>()
    );
}

#[test]
fn errors_say_what_to_do() {
    assert_eq!(
        WslError::NoDistro {
            name: "Ubunt".into(),
            known: vec!["Ubuntu".into(), "Debian".into()]
        }
        .to_string(),
        "there is no WSL distro named Ubunt (installed: Ubuntu, Debian)"
    );
    assert_eq!(
        WslError::NoDistro {
            name: "Ubuntu".into(),
            known: Vec::new()
        }
        .to_string(),
        "there is no WSL distro named Ubuntu (wsl.exe --list shows none)"
    );
    let missing = WslError::NoBuild {
        arch: Arch::Aarch64,
        path: PathBuf::from("dir").join("dv-linux-aarch64"),
    };
    assert_eq!(
        missing.to_string(),
        format!(
            "no Linux build of dv for aarch64: {} is missing (script/build-linux --aarch64 \
             makes it)",
            PathBuf::from("dir").join("dv-linux-aarch64").display()
        )
    );
    assert!(missing.permanent());
    assert!(WslError::UnknownArch("riscv64".into()).permanent());
    assert!(!WslError::Failed("the distro stopped".into()).permanent());
}

/// The launch script and [`launch`] under this machine's `/bin/sh` (`Shell::local`).
#[cfg(target_os = "linux")]
mod local {
    use std::io::{BufReader, Write as _};

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::install::LinuxBuilds;

    /// A scratch home, a folder with a stand-in build for this machine's architecture, and
    /// the shell whose `$HOME` is that home.
    struct Scratch {
        _dir: tempfile::TempDir,
        home: PathBuf,
        builds: PathBuf,
        shell: Shell,
    }

    fn scratch() -> Scratch {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let builds = dir.path().join("builds");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&builds).unwrap();
        // The stand-in build says how it was started, then echoes its input.
        std::fs::write(
            builds.join(Arch::host().file_name()),
            "#!/bin/sh\necho \"started with $*\"\nexec cat\n",
        )
        .unwrap();
        let shell = Shell::local(Some(home.to_str().unwrap().to_string())).unwrap();
        Scratch {
            _dir: dir,
            home,
            builds,
            shell,
        }
    }

    /// Waits for a launched stand-in to be running (its first line), then ends it: until
    /// then its file is being executed, and writing it would fail with ETXTBSY.
    fn finish(transport: Transport) -> String {
        let Transport {
            reader,
            writer,
            child,
            ..
        } = transport;
        let mut line = String::new();
        BufReader::new(reader).read_line(&mut line).unwrap();
        drop(writer);
        child.unwrap().wait().unwrap();
        line
    }

    fn run_launch(shell: &Shell, args: &[&str]) -> (Option<i32>, String) {
        let output = shell.run(LAUNCH, args, Duration::from_secs(20)).unwrap();
        (
            output.status.code(),
            String::from_utf8(output.stdout).unwrap(),
        )
    }

    #[test]
    fn the_launch_script_says_what_is_missing() {
        let scratch = scratch();
        let host = Arch::host().name();
        let (x86, arm) = if Arch::host() == Arch::X86_64 {
            ("abc", "-")
        } else {
            ("-", "abc")
        };
        assert_eq!(
            run_launch(&scratch.shell, &[x86, arm]),
            (Some(120), format!("dv-missing {host}\n"))
        );
        assert_eq!(
            run_launch(&scratch.shell, &["-", "-"]),
            (Some(122), format!("dv-no-build {host}\n"))
        );
    }

    /// [`RUN`] to its end with `args` after the hashes: its exit code, stdout and stderr.
    fn run_cli(shell: &Shell, args: &[&str]) -> (Option<i32>, String, String) {
        let output = super::run(
            shell.console_command(RUN, args),
            None,
            Duration::from_secs(20),
        )
        .unwrap();
        (
            output.status.code(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    #[test]
    fn the_clis_run_says_nothing_of_its_own_and_runs_the_build_in_the_folder() {
        let scratch = scratch();
        let builds = LinuxBuilds::find(&scratch.builds).unwrap();
        let hashes = builds.hash_args();
        let folder = scratch.home.to_str().unwrap();
        // Missing, or no build for the machine: the exit code alone says so.
        assert_eq!(
            run_cli(&scratch.shell, &[&hashes[0], &hashes[1], folder, "status"]),
            (Some(RUN_MISSING), String::new(), String::new())
        );
        assert_eq!(
            run_cli(&scratch.shell, &["-", "-", folder, "status"]),
            (Some(RUN_NO_BUILD[1]), String::new(), String::new())
        );

        // Installed: the build runs in the folder with the arguments as given, and its exit
        // code is the run's.
        let build = builds.get(Arch::host()).unwrap();
        crate::install::install(&scratch.shell, build, &|_, _| {}).unwrap();
        let installed = scratch
            .home
            .join(".local/share/dv/bin")
            .join(format!("dv-{}", build.hash));
        std::fs::write(
            &installed,
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\"; for a; do printf '[%s]\\n' \"$a\"; done\nexit 3\n",
        )
        .unwrap();
        let (code, stdout, stderr) = run_cli(
            &scratch.shell,
            &[&hashes[0], &hashes[1], folder, "a b", "", "$x"],
        );
        assert_eq!(
            (code, stdout, stderr),
            (
                Some(3),
                format!("{folder}\n[a b]\n[]\n[$x]\n"),
                String::new()
            )
        );
        // A folder that isn't there: `/`, where an absolute `--repo` still names the
        // repository.
        let (_, stdout, _) = run_cli(
            &scratch.shell,
            &[&hashes[0], &hashes[1], "/no/such/dv/folder", "x"],
        );
        assert_eq!(stdout, "/\n[x]\n");
    }

    #[test]
    fn a_launch_installs_the_missing_build_and_starts_it() {
        let scratch = scratch();
        let builds = LinuxBuilds::find(&scratch.builds).unwrap();
        let build = builds.get(Arch::host()).unwrap().clone();
        let heard = Mutex::new(Vec::new());
        let progress = |done, total| heard.lock().push((done, total));
        let transport = launch(&scratch.shell, &builds, None, &progress).unwrap();
        assert_eq!(transport.build.as_deref(), Some(build.hash.as_str()));
        let installed = scratch
            .home
            .join(".local/share/dv/bin")
            .join(format!("dv-{}", build.hash));
        assert!(installed.is_file());
        assert_eq!(
            std::fs::read_link(scratch.home.join(".local/bin/dv")).unwrap(),
            installed
        );
        let heard = heard.into_inner();
        assert_eq!(heard.first(), Some(&(0, build.len)));
        assert_eq!(heard.last(), Some(&(build.len, build.len)));

        let Transport {
            reader,
            mut writer,
            child,
            ..
        } = transport;
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "started with backend --stdio\n");
        writer.write_all(b"ping\n").unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "ping\n");
        drop(writer);
        let mut child = child.unwrap();
        assert!(child.wait().unwrap().success());

        // Installed now: the next launch starts it at once.
        let again = launch(&scratch.shell, &builds, None, &|_, _| {
            panic!("installed again")
        })
        .unwrap();
        drop(again.writer);
        let mut child = again.child.unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_reinstall_copies_the_build_again_before_starting_it() {
        let scratch = scratch();
        let builds = LinuxBuilds::find(&scratch.builds).unwrap();
        let build = builds.get(Arch::host()).unwrap().clone();
        finish(launch(&scratch.shell, &builds, None, &|_, _| {}).unwrap());
        // Another build's bytes under this build's name.
        let installed = scratch
            .home
            .join(".local/share/dv/bin")
            .join(format!("dv-{}", build.hash));
        std::fs::write(&installed, "#!/bin/sh\necho another build\n").unwrap();
        let copied = Mutex::new(0);
        let transport = launch(&scratch.shell, &builds, Some(Arch::host()), &|done, _| {
            *copied.lock() = done;
        })
        .unwrap();
        assert_eq!(*copied.lock(), build.len);
        assert_eq!(
            std::fs::read(&installed).unwrap(),
            std::fs::read(&build.path).unwrap()
        );
        assert_eq!(
            finish(transport),
            "started with backend --stdio
"
        );
    }

    #[test]
    fn a_launch_without_a_build_for_the_machine_names_the_file() {
        let scratch = scratch();
        let other = if Arch::host() == Arch::X86_64 {
            Arch::Aarch64
        } else {
            Arch::X86_64
        };
        std::fs::rename(
            scratch.builds.join(Arch::host().file_name()),
            scratch.builds.join(other.file_name()),
        )
        .unwrap();
        let builds = LinuxBuilds::find(&scratch.builds).unwrap();
        let Err(error) = launch(&scratch.shell, &builds, None, &|_, _| {}) else {
            panic!("started another architecture's build");
        };
        assert_eq!(
            error,
            WslError::NoBuild {
                arch: Arch::host(),
                path: scratch.builds.join(Arch::host().file_name())
            }
        );
    }
}

/// Every `wsl.exe` dv starts is made without a console window but the CLI's run inside a
/// distro (module docs). `Command` can't be asked for its creation flags, so the sources are:
/// `wsl.exe` is made in [`wsl_exe`], through `toolbox_process::Command::to_std` (which adds
/// `CREATE_NO_WINDOW`), and in
/// `Shell::console_command`, and nowhere else.
#[test]
fn every_wsl_exe_but_the_clis_run_starts_without_a_console_window() {
    let sources = [
        ("wsl.rs", include_str!("../wsl.rs")),
        ("install.rs", include_str!("../install.rs")),
        ("launch.rs", include_str!("../launch.rs")),
        ("client/mod.rs", include_str!("../client/mod.rs")),
        ("client/link.rs", include_str!("../client/link.rs")),
    ];
    let mut makers = Vec::new();
    for (file, text) in sources {
        assert!(
            !text.contains("Command::new(\"wsl"),
            "{file} spells wsl.exe itself"
        );
        for (at, _) in text.match_indices("Command::new(WSL_EXE)") {
            let before = &text[..at];
            let function = before
                .rfind("fn ")
                .map(|start| {
                    before[start + 3..]
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .next()
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            makers.push(format!("{file}: {function}"));
        }
    }
    assert_eq!(makers, ["wsl.rs: wsl_exe", "wsl.rs: console_command"]);
    let wsl = include_str!("../wsl.rs");
    let builder = &wsl[wsl.find("fn wsl_exe() -> Command {").unwrap()..];
    let body = &builder[..builder.find("\n}").unwrap()];
    assert!(body.contains(".to_std()"), "{body}");
}
