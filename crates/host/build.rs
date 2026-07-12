//! Bakes a `{CARGO_PKG_VERSION}+{short-sha}` string into the binary at
//! compile time (read back in main.rs via `env!("DV_HOST_VERSION")`), so
//! the handshake's `version` field identifies exactly which commit is
//! running — the client-side sidecar hash check (S3) needs this to detect
//! dev-dirty upgrades. `GITHUB_SHA` (set by CI) wins when present; falls
//! back to `git rev-parse --short HEAD` for local dev builds; falls back
//! to "dev" when neither is available (e.g. building from a tarball with
//! no `.git`).

use std::process::Command;

fn main() {
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let sha = github_sha()
        .or_else(git_short_sha)
        .unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=DV_HOST_VERSION={pkg_version}+{sha}");
    // Only re-run when HEAD moves (not on every build.rs re-check) — build
    // scripts already re-run when their own inputs change; this narrows
    // reruns to the one thing this script actually reads besides CARGO_*.
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}

fn github_sha() -> Option<String> {
    let full = std::env::var("GITHUB_SHA").ok()?;
    full.get(0..7).map(str::to_string)
}

fn git_short_sha() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}
