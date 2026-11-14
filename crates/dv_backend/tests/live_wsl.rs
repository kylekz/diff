//! The owner's live WSL check (docs/PLAN.md §14.3): this build of dv installed into the real
//! home of a real distro, as the app installs it, and used there. Never run by an agent, a
//! script or `script/check`: it is `#[ignore]` and does nothing without `DV_LIVE_WSL=1`.
//!
//! ```text
//! cargo build -p dv
//! script/build-linux
//! DV_LIVE_WSL=1 cargo test -p dv_backend --test live_wsl -- --ignored --test-threads=1
//! ```
//!
//! `DV_LIVE_WSL_DISTRO` names the distro (default `Ubuntu`). It installs
//! `~/.local/share/dv/bin/dv-<hash>` there, points `~/.local/bin/dv` at it (replacing what was
//! there, such as the first implementation's file) and prunes other builds of dv in that
//! folder, as the app does; it leaves `~/.local/share/dv`'s other folders alone. Its scratch
//! repository lives under the distro's `/tmp` and is removed at the end. Last, the Windows
//! `dv` (`target/debug/dv.exe`) runs a command for that repository, which crosses into the
//! distro (PLAN §10.4).

#![cfg(windows)]

mod wsl_common;

use std::path::PathBuf;
use std::process::{Command, Stdio};

use dv_backend::install;
use dv_backend::wsl::{Arch, Shell};
use dv_backend::{Connection, ConnectionState, Launcher};
use dv_core::backend::{Backend, GitOpts, WatchSpec};
use dv_core::location::Location;
use pretty_assertions::assert_eq;

use self::wsl_common::*;

/// The distro to check, when the owner asked for the live check.
fn live() -> Option<String> {
    if std::env::var("DV_LIVE_WSL").ok().as_deref() != Some("1") {
        eprintln!("skipped: the live WSL check runs only with DV_LIVE_WSL=1 (the owner's)");
        return None;
    }
    Some(
        std::env::var("DV_LIVE_WSL_DISTRO")
            .ok()
            .filter(|distro| !distro.trim().is_empty())
            .unwrap_or_else(|| "Ubuntu".into()),
    )
}

#[test]
#[ignore = "the owner's live check: DV_LIVE_WSL=1 (PLAN §14.3)"]
fn live_wsl_install_open_watch_and_the_cli() {
    let Some(distro) = live() else {
        return;
    };
    let builds = builds();
    let build = builds.get(Arch::X86_64).unwrap().clone();
    // The distro user's own home, as the app uses it.
    let shell = Shell::wsl(distro.clone(), None).unwrap();

    // The app's connection: installs this build when the distro lacks it, then shakes hands.
    let connection = Connection::new(
        &distro,
        Launcher::Wsl {
            distro: distro.clone(),
            builds: builds_dir(),
            home: None,
        },
    );
    let (_subscription, rx) = states(&connection);
    // The explicit start an open makes: it boots the distro when it isn't running.
    connection.retry().unwrap();
    let seen = states_until(&rx, |state| *state == ConnectionState::Connected);
    eprintln!("states: {seen:?}");
    assert_eq!(connection.server().unwrap().build_ok, Some(true));
    let probe = install::probe(&shell, &builds).unwrap();
    assert!(probe.up_to_date(), "{probe:?}");
    eprintln!("installed: {}", probe.path);

    // `~/.local/bin/dv` is this build, or, for a development build once the owner installed
    // the packaged app, the installed copy's build, which it leaves alone (PLAN §5.2's link
    // rule): a packaged build with its `.packaged` marker.
    let linked = sh(&shell, r#"readlink "$HOME/.local/bin/dv""#, &[]);
    if linked.trim_end() == probe.path {
        assert_eq!(
            sh(&shell, r#""$HOME/.local/bin/dv" --version"#, &[]),
            format!("dv {}\n", dv_backend::build::version_text())
        );
        assert_eq!(
            sh(
                &shell,
                r#"sha256sum < "$HOME/.local/bin/dv" | cut -d' ' -f1"#,
                &[]
            ),
            format!("{}\n", build.hash)
        );
    } else {
        assert!(
            !builds.packaged(),
            "a packaged build always takes the link, but it runs {linked}"
        );
        assert_eq!(probe.link, install::LinkState::Packaged, "{probe:?}");
        assert_eq!(
            sh(
                &shell,
                r#"[ -f "$(readlink "$HOME/.local/bin/dv").packaged" ] && echo marked"#,
                &[]
            ),
            "marked\n",
            "{linked}"
        );
    }

    // A repository under /tmp opens, and its git, store and watch work.
    let scratch = TmpDir::new(&shell, "/tmp", "dv-live-check");
    let repo = scratch.join("repo");
    scratch_repo(&shell, &repo);
    let backend = connection.open(&repo, Vec::new()).unwrap();
    assert_eq!(
        backend.repo().location,
        Location::Wsl {
            distro: distro.clone(),
            path: repo.clone()
        }
    );
    let status = backend
        .git(&["status", "--porcelain"], &GitOpts::default())
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&status.stdout), "");
    let (handle, events) = watch(&backend, WatchSpec::WORKTREE);
    assert_eq!(handle.worktree_error(), None);
    sh(&shell, r#"printf 'b\n' > "$1/b.txt""#, &[&repo]);
    batch_naming(&events, "b.txt");
    drop(handle);

    // The dv on the distro's PATH reads the same repository.
    let listed = sh(
        &shell,
        r#"cd "$1" && env -u CLAUDECODE -u DV_AUTHOR -u DV_MODEL -u DV_REVIEW "$HOME/.local/bin/dv" review list --json"#,
        &[&repo],
    );
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed["schema"], 1);

    // dv on Windows runs a command for it inside the distro, as this build.
    let output = Command::new(windows_dv())
        .args(["--wsl", &format!("{distro}:{repo}"), "status", "--json"])
        .env_remove("CLAUDECODE")
        .env_remove("DV_AUTHOR")
        .env_remove("DV_MODEL")
        .env_remove("DV_REVIEW")
        .env_remove("DV_WSL_HOME")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(
        !stderr.contains("installing"),
        "installed already: {stderr}"
    );
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["repo"]["root"], repo.as_str());
}

/// The Windows `dv` beside the Linux build (`cargo build -p dv`): `target/debug/dv.exe`, this
/// test's executable being in `target/debug/deps`.
fn windows_dv() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dv = exe.parent().unwrap().parent().unwrap().join("dv.exe");
    assert!(
        dv.is_file(),
        "{} isn't there: run cargo build -p dv first",
        dv.display()
    );
    dv
}
