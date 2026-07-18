//! Local (non-WSL) node/vtsls detection — the Windows-side counterpart to
//! [`super::node`]'s asdf-aware WSL path (docs/backlog.md "Local/Windows LSP
//! spawn path"). No asdf convention exists locally, so this is deliberately
//! PATH-only: every common Windows node install (nvm-windows, the official
//! MSI, volta, scoop) ends up with `node.exe` + npm global shims on `PATH`,
//! so a plain PATH search covers them all without per-manager special
//! casing. `npm root -g` is a cheap, generic fallback for vtsls specifically
//! (a global install whose shim directory isn't (yet) reflected on this
//! process's `PATH`) — not a substitute for real PATH detection, just a
//! second cheap look before giving up.
//!
//! **No consent flow, unlike WSL.** [`super::node::install_vtsls`] is
//! WSL-only and gated behind the onboarding page's explicit per-row user
//! consent (see [`super`]'s module doc). This module has no equivalent
//! install function at all: a local repo with node but no vtsls degrades to
//! an actionable status hint ("npm i -g @vtsls/language-server") and stops
//! there — dv never runs `npm install -g` against the user's own,
//! non-sandboxed global npm on their behalf. This is a deliberate scope cut
//! for this slice, not an oversight — see docs/backlog.md and
//! docs/phase-8-lsp-and-polish.md's deviation note.
//!
//! **Windows shims.** A global npm install puts a `.cmd`/`.bat` *shim* on
//! `PATH` for `vtsls`, not a real executable — [`crate::command::
//! is_windows_shell_shim`]/[`vtsls_invocation`] handle spawning/probing it
//! via `cmd /C`; see their doc comments.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::command::CommandBuilder;
use crate::location::RepoLocation;

use super::{DetectError, NodeVtsls};

/// Wall-clock bound for a single `node --version`/`vtsls --version`/`npm
/// root -g` probe — all cheap, local, no-network round trips, so this is
/// much tighter than the WSL side's [`super::node`]-equivalent timeouts
/// (which pay for a `wsl.exe` boot on top).
const LOCAL_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Env var test seam mirroring `DV_HOST_PATH`'s precedent (docs/phase-5
/// WSL host doc): when set, [`detect_node_vtsls_local`] uses this path as
/// the vtsls entry point directly — a `.js` bin OR a shim — skipping the
/// `PATH`/`npm root -g` search entirely. This is how the live-verification
/// loop (and any future CI) points at a throwaway `npm i --prefix <scratch>`
/// install without mutating the user's global npm or `PATH`.
pub const DV_VTSLS_PATH_ENV: &str = "DV_VTSLS_PATH";

/// Detect node + vtsls on the local (non-WSL) `PATH`. Never fails just
/// because vtsls is absent — mirrors [`super::node::detect_node_vtsls`]'s
/// `Ok(NodeVtsls { vtsls_path: None, .. })` convention for "node present,
/// vtsls missing" (the caller renders an actionable hint, doesn't retry
/// automatically). Only errs when node itself can't be found at all
/// ([`DetectError::LocalNodeNotFound`]).
pub fn detect_node_vtsls_local() -> Result<NodeVtsls, DetectError> {
    let node_path = find_node().ok_or(DetectError::LocalNodeNotFound)?;
    let node_path_str = node_path.to_string_lossy().into_owned();
    let node_version = run_probe(&node_path_str, &["--version"]).unwrap_or_default();

    let vtsls_path = find_vtsls();
    let vtsls_version = vtsls_path.as_ref().and_then(|vtsls| {
        let (program, args) =
            vtsls_invocation(&node_path_str, &vtsls.to_string_lossy(), &["--version"]);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_probe(&program, &arg_refs)
    });

    Ok(NodeVtsls {
        node_path: node_path_str,
        node_version,
        vtsls_path: vtsls_path.map(|p| p.to_string_lossy().into_owned()),
        vtsls_version,
    })
}

/// Build the `(program, args)` pair to actually run vtsls with `extra_args`
/// appended (`--stdio` for a real spawn, `--version` for detection's own
/// probe) — shared by [`detect_node_vtsls_local`] and
/// [`crate::lsp::client::LspHandle::spawn`]'s local vtsls spawn, so the
/// "how do we invoke this exact `vtsls_path`" decision is made in exactly
/// one place. A plain `.js` bin (the resolved real entry point — what
/// [`DV_VTSLS_PATH_ENV`] points the live-verification loop at) runs via
/// `node_path` directly, identical in shape to the WSL side's `node
/// <asdf-resolved vtsls symlink> --stdio`. A Windows `.cmd`/`.bat` shim
/// (what a plain PATH search finds after a real `npm install -g`) can't be
/// spawned directly ([`crate::command::is_windows_shell_shim`]'s doc) —
/// runs via `cmd /C` instead, bypassing `node_path` entirely (the shim
/// resolves its own node internally).
pub(crate) fn vtsls_invocation(
    node_path: &str,
    vtsls_path: &str,
    extra_args: &[&str],
) -> (String, Vec<String>) {
    if crate::command::is_windows_shell_shim(vtsls_path) {
        let mut args = vec!["/C".to_string(), vtsls_path.to_string()];
        args.extend(extra_args.iter().map(|s| s.to_string()));
        ("cmd".to_string(), args)
    } else {
        let mut args = vec![vtsls_path.to_string()];
        args.extend(extra_args.iter().map(|s| s.to_string()));
        (node_path.to_string(), args)
    }
}

/// Run `program` with `args` (already the complete, final argv — callers
/// build the full `["--version"]`/shim-wrapped shape before calling this),
/// bounded, never-fail-hard: any spawn failure/timeout/non-zero-exit
/// degrades to `None` (every caller already treats a missing/empty version
/// as "not usable" without needing to distinguish why).
fn run_probe(program: &str, args: &[&str]) -> Option<String> {
    let builder = CommandBuilder::new_spawn_only(RepoLocation::Local(PathBuf::from(".")));
    let bytes = builder
        .run_timeout(program, args, LOCAL_PROBE_TIMEOUT)
        .ok()?;
    let text = crate::command::decode_output(&bytes).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn find_node() -> Option<PathBuf> {
    find_on_path("node")
}

fn find_vtsls() -> Option<PathBuf> {
    if let Some(over) = std::env::var(DV_VTSLS_PATH_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return Some(PathBuf::from(over));
    }
    find_on_path("vtsls").or_else(npm_root_g_vtsls)
}

/// `npm root -g` + the deterministic `@vtsls/language-server` install
/// layout — a cheap, generic fallback for the case where a real npm global
/// install exists but its shim directory isn't (yet) reflected on this
/// process's own `PATH` (docs/backlog.md: "npm root -g-based fallback if
/// cheap"). Returns the real `.js` bin, not a `.cmd` shim — so a hit here
/// always takes [`vtsls_invocation`]'s direct-via-node branch. `npm` itself
/// is just as likely to be a `.cmd` shim as `vtsls` — same
/// [`crate::command::is_windows_shell_shim`] check, inlined here rather
/// than through [`vtsls_invocation`] (that fn's shape is specifically
/// "vtsls_path run via node_path or cmd", not a generic "any shim" helper).
fn npm_root_g_vtsls() -> Option<PathBuf> {
    let npm_path = find_on_path("npm")?;
    let npm_path = npm_path.to_string_lossy();
    let (program, args): (String, Vec<String>) = if crate::command::is_windows_shell_shim(&npm_path)
    {
        (
            "cmd".to_string(),
            vec![
                "/C".to_string(),
                npm_path.to_string(),
                "root".to_string(),
                "-g".to_string(),
            ],
        )
    } else {
        (
            npm_path.to_string(),
            vec!["root".to_string(), "-g".to_string()],
        )
    };
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let builder = CommandBuilder::new_spawn_only(RepoLocation::Local(PathBuf::from(".")));
    let bytes = builder
        .run_timeout(&program, &arg_refs, LOCAL_PROBE_TIMEOUT)
        .ok()?;
    let root = crate::command::decode_output(&bytes).trim().to_string();
    if root.is_empty() {
        return None;
    }
    let candidate = Path::new(&root)
        .join("@vtsls")
        .join("language-server")
        .join("bin")
        .join("vtsls.js");
    candidate.is_file().then_some(candidate)
}

/// Candidate executable names for `base` on this platform, in PATH-search
/// order — Windows tries every `PATHEXT` extension (falling back to a
/// built-in default if the env var is somehow unset) plus the bare name
/// last; every other platform just tries the bare name (no shim concept).
/// Pure and unit-tested independent of any real `PATH`/`PATHEXT`.
fn candidate_names(base: &str, pathext: Option<&str>) -> Vec<String> {
    if !cfg!(windows) {
        return vec![base.to_string()];
    }
    let pathext = pathext.unwrap_or(".COM;.EXE;.BAT;.CMD");
    let mut names: Vec<String> = pathext
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| format!("{base}{}", ext.to_ascii_lowercase()))
        .collect();
    names.push(base.to_string());
    names
}

/// Search `dirs` (in order) for any of `names`, returning the first file
/// that actually exists. Pure and unit-tested against a synthetic temp
/// directory — the real caller ([`find_on_path`]) feeds it `PATH`'s
/// directory list.
fn find_in_dirs(dirs: &[PathBuf], names: &[String]) -> Option<PathBuf> {
    for dir in dirs {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn find_on_path(base: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
    let pathext = std::env::var("PATHEXT").ok();
    let names = candidate_names(base, pathext.as_deref());
    find_in_dirs(&dirs, &names)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- candidate_names: PATHEXT-driven search order --------------------

    #[test]
    fn candidate_names_on_non_windows_is_just_the_bare_name() {
        if !cfg!(windows) {
            assert_eq!(candidate_names("node", None), vec!["node".to_string()]);
        }
    }

    #[test]
    fn candidate_names_uses_pathext_and_falls_back_to_the_bare_name_last() {
        if cfg!(windows) {
            let names = candidate_names("vtsls", Some(".COM;.EXE;.CMD"));
            assert_eq!(
                names,
                vec![
                    "vtsls.com".to_string(),
                    "vtsls.exe".to_string(),
                    "vtsls.cmd".to_string(),
                    "vtsls".to_string(),
                ]
            );
        }
    }

    #[test]
    fn candidate_names_defaults_pathext_when_env_is_missing() {
        if cfg!(windows) {
            let names = candidate_names("node", None);
            assert!(names.contains(&"node.exe".to_string()));
            assert!(names.contains(&"node.cmd".to_string()));
            assert!(names.last() == Some(&"node".to_string()));
        }
    }

    // --- find_in_dirs: filesystem search over a synthetic layout ---------

    #[test]
    fn find_in_dirs_finds_the_first_existing_candidate() {
        let tmp = std::env::temp_dir().join(format!(
            "dv-test-find-in-dirs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let dir_a = tmp.join("a");
        let dir_b = tmp.join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(dir_b.join("vtsls.cmd"), "").unwrap();

        let names = vec!["vtsls.exe".to_string(), "vtsls.cmd".to_string()];
        let found = find_in_dirs(&[dir_a.clone(), dir_b.clone()], &names);
        assert_eq!(found, Some(dir_b.join("vtsls.cmd")));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_in_dirs_is_none_when_nothing_matches() {
        let tmp = std::env::temp_dir().join(format!(
            "dv-test-find-in-dirs-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let found = find_in_dirs(std::slice::from_ref(&tmp), &["vtsls.exe".to_string()]);
        assert_eq!(found, None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // --- vtsls_invocation: the shim-vs-direct spawn decision --------------

    #[test]
    fn vtsls_invocation_runs_a_js_entry_via_node_directly() {
        let (program, args) = vtsls_invocation(
            "C:/node/node.exe",
            "C:/npm/node_modules/@vtsls/language-server/bin/vtsls.js",
            &["--stdio"],
        );
        assert_eq!(program, "C:/node/node.exe");
        assert_eq!(
            args,
            vec![
                "C:/npm/node_modules/@vtsls/language-server/bin/vtsls.js".to_string(),
                "--stdio".to_string(),
            ]
        );
    }

    #[test]
    fn vtsls_invocation_wraps_a_windows_cmd_shim_via_cmd_c() {
        if cfg!(windows) {
            let (program, args) =
                vtsls_invocation("C:/node/node.exe", "C:/npm/vtsls.cmd", &["--stdio"]);
            assert_eq!(program, "cmd");
            assert_eq!(
                args,
                vec![
                    "/C".to_string(),
                    "C:/npm/vtsls.cmd".to_string(),
                    "--stdio".to_string(),
                ]
            );
        }
    }

    #[test]
    fn vtsls_invocation_is_never_a_shim_off_windows() {
        if !cfg!(windows) {
            let (program, _) =
                vtsls_invocation("/usr/bin/node", "/usr/bin/vtsls.cmd", &["--stdio"]);
            assert_eq!(program, "/usr/bin/node");
        }
    }
}
