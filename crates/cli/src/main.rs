//! The native `dv-cli` binary. Two roles, split by target OS:
//!
//! - **Linux (musl)**: the WSL-side agent CLI (docs/phase-8-lsp-and-polish
//!   .md § Distribution & first-run, Phase 8 S8b). On first use per WSL
//!   distro, `crates/core/src/remote/install.rs`'s `CLI_SPEC` streams this
//!   binary WSL-side and installs it as `~/.local/bin/dv` — the source
//!   filename here doesn't matter (the installer renames on disk, exactly
//!   as `dv-host-linux-x64` installs as `dv-host`), which is also why this
//!   binary is named `dv-cli` rather than `dv`: the `dv` package
//!   (`crates/app`) already produces a `dv`/`dv.exe` binary target, and two
//!   same-named binaries would collide at `target/release/`. Purely
//!   headless: never spawns a GUI.
//!
//! - **Windows**: the console launcher the dist bundle ships as `dv.exe`
//!   (the PATH-visible entry point). Headless invocations
//!   ([`dv_cli::is_headless`]) run right here — a real console-subsystem
//!   process, so the shell waits and output lands before the prompt,
//!   unlike the windowed GUI binary's attach-after-the-prompt printing
//!   (live-reported as "broken newline handling"). Everything else (a repo
//!   path, `dv pr <number|url>`, a bare `dv`) is a GUI launch: forwarded
//!   to the windowed `dv-gui.exe` sitting next to this exe.
//!
//! `dv_cli::run` doesn't assume `args[0]` is one of `{review, comment, pr}`
//! the way the GUI's pre-filtered dispatch does — it's handed the raw
//! `argv[1..]` here and handles `--version`/unknown-subcommand itself. See
//! `dv_cli`'s crate-root doc comment.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    if !dv_cli::is_headless(&args) {
        std::process::exit(forward_to_gui(&args));
    }
    std::process::exit(dv_cli::run(&args));
}

/// Hand a GUI-shaped invocation to the windowed binary (`dv-gui.exe`,
/// exe-adjacent — same lookup convention as the WSL sidecars), then give
/// the terminal its prompt back.
///
/// The grace loop is the part that makes errors feel native: the GUI
/// binary validates its arguments BEFORE any gpui initialization, so a bad
/// invocation (`dv no-such-dir`, `dv pr not-a-number`) exits within
/// milliseconds — inside the grace window — and its stderr (inherited from
/// this console, so it prints cleanly BEFORE the prompt returns) arrives
/// with the real exit code propagated. A launch still alive at the end of
/// the grace has committed to opening a window; waiting any longer would
/// hold the terminal hostage for the whole GUI session.
#[cfg(windows)]
fn forward_to_gui(args: &[String]) -> i32 {
    use std::time::Duration;

    let gui = match std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("dv-gui.exe")))
    {
        Some(path) if path.exists() => path,
        Some(path) => {
            eprintln!(
                "dv-gui.exe not found next to this launcher (looked at {}) — this dv.exe only \
                 routes CLI commands; reinstall the dv bundle so both binaries sit together",
                path.display()
            );
            return 1;
        }
        None => {
            eprintln!("could not resolve this launcher's own location");
            return 1;
        }
    };

    let mut child = match std::process::Command::new(&gui).args(args).spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("failed to launch {}: {err}", gui.display());
            return 1;
        }
    };
    for _ in 0..16 {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(1),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            // The child exists but its status is unreadable — nothing more
            // this launcher can do for it; assume it's running.
            Err(_) => break,
        }
    }
    0
}
