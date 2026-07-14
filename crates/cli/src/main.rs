//! The native `dv-cli` binary — a gpui-free, musl-buildable Linux build of
//! the agent CLI (docs/phase-8-lsp-and-polish.md § Distribution & first-run,
//! Phase 8 S8b). On first use per WSL distro, `crates/core/src/remote/
//! install.rs`'s `CLI_SPEC` streams this binary WSL-side and installs it as
//! `~/.local/bin/dv` — the source filename here doesn't matter (the
//! installer renames on disk, exactly as `dv-host-linux-x64` installs as
//! `dv-host`), which is also why this binary is named `dv-cli` rather than
//! `dv`: the `dv` package (`crates/app`) already produces a `dv`/`dv.exe`
//! binary target, and two same-named binaries would collide at
//! `target/release/`.
//!
//! `dv_cli::run` doesn't assume `args[0]` is one of `{review, comment, pr}`
//! the way the GUI's pre-filtered dispatch does — it's handed the raw
//! `argv[1..]` here and handles `--version`/unknown-subcommand itself. See
//! `dv_cli`'s crate-root doc comment.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    std::process::exit(dv_cli::run(&args[1..]));
}
