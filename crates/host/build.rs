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
    // `.git/HEAD` alone only changes on checkout/detach. The single most
    // common dev-loop case — committing on the branch you're already on —
    // instead updates `.git/refs/heads/<branch>`, which `.git/HEAD` merely
    // POINTS AT ("ref: refs/heads/main") without itself changing. Without
    // also watching that file, this build script wouldn't re-run and
    // DV_HOST_VERSION would go stale after same-branch commits (plan §8 S2
    // review finding P3-6).
    if let Some(ref_path) = current_branch_ref_path() {
        println!("cargo:rerun-if-changed={ref_path}");
    }
}

/// Resolves the ref file `.git/HEAD` points at for the current branch
/// (`"ref: refs/heads/main"` -> `../../.git/refs/heads/main`). `None` for a
/// detached HEAD (no `ref: ` line — `.git/HEAD` itself already holds the
/// raw sha, so watching it alone is already sufficient there) or if
/// `.git/HEAD` can't be read at all.
///
/// Doesn't handle a branch tip that lives ONLY in `.git/packed-refs`
/// (never checked out to loose-ref form) — the `.exists()` check below
/// just skips emitting a rerun-if-changed for those, same as if this
/// function had never been called. Worst case is a stale embedded sha,
/// which is diagnostic-only (the handshake's `version` field), never
/// correctness-affecting, so staying best-effort here is fine.
fn current_branch_ref_path() -> Option<String> {
    let head = std::fs::read_to_string("../../.git/HEAD").ok()?;
    let branch = head.trim().strip_prefix("ref: ")?;
    // `branch` is POSIX-style ("refs/heads/main") even on a Windows
    // checkout — git always writes it that way — so a plain `/` join is
    // safe on every platform this builds on.
    let ref_path = format!("../../.git/{branch}");
    std::path::Path::new(&ref_path).exists().then_some(ref_path)
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
