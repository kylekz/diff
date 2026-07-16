//! asdf-aware node/vtsls detection, plus the ONE user-environment-mutating
//! fn in this crate ([`install_vtsls`]) — see [`super`]'s module doc for the
//! boot-storm and consent contracts every fn here must honor.
//!
//! node/vtsls detection MUST NOT use `PATH` — asdf is not on `PATH`, not
//! even under `bash -lc` (verified against the real Ubuntu distro, where
//! global node is asdf-managed: `~/.tool-versions` → `nodejs 22.22.0`,
//! concrete binary `~/.asdf/installs/nodejs/22.22.0/bin/node`). Instead
//! [`detect_script_via_tool_versions`] reads `~/.tool-versions` directly and
//! constructs the asdf install path deterministically.

use std::time::Duration;

use anyhow::anyhow;

use crate::command::CommandBuilder;
use crate::location::RepoLocation;
use crate::remote::install::InstallError;

/// Wall-clock bound for the (read-only) node/vtsls detection script — a
/// single `sh -c` one-liner, so this is generous but nowhere near
/// [`crate::remote::install`]'s streaming-install timeout.
const NODE_DETECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Wall-clock bound for `npm install -g @vtsls/language-server` — the ONE
/// user-environment-mutating command in this crate. npm-global installs can
/// legitimately take a while (registry latency, a cold npm cache), so this
/// is generous — but still bounded: a wedged install must degrade to a
/// [`ComponentState::Failed`](super::ComponentState::Failed) + cooldown on
/// the app side, never hang the onboarding page forever.
const VTSLS_INSTALL_TIMEOUT: Duration = Duration::from_secs(180);

/// A detected node + (optional) vtsls pair, resolved via asdf's
/// `~/.tool-versions` — never via `PATH` (see the module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeVtsls {
    pub node_path: String,
    pub node_version: String,
    pub vtsls_path: Option<String>,
    pub vtsls_version: Option<String>,
}

/// Every way [`detect_node_vtsls`] can fail to produce a [`NodeVtsls`].
#[derive(Debug)]
pub enum DetectError {
    /// Reserved for a future non-WSL (local/Windows-native) node detection
    /// path. Every caller today passes a WSL distro, so this is never
    /// constructed yet — kept so [`super::check_node_vtsls`]'s match arms
    /// don't need reshaping when that lands.
    NotWsl,
    /// The bounded WSL round trip itself failed: a spawn failure, it hit
    /// [`NODE_DETECT_TIMEOUT`], or the detect script's own `$HOME`-empty
    /// guard tripped (see [`detect_script_via_tool_versions`]'s doc) — a
    /// transient/environmental problem, not a verdict on whether node/vtsls
    /// are installed. Kept distinct from [`DetectError::NoNodeFound`] so
    /// [`super::check_node_vtsls`] maps this to
    /// [`super::ComponentState::Failed`] (retryable) rather than
    /// [`super::ComponentState::Missing`] (which would misleadingly tell the
    /// user to go install node when the real problem was e.g. a wedged
    /// distro or an unresolved `$HOME`).
    Bounded(anyhow::Error),
    /// The round trip completed successfully (exit 0) but the script found
    /// no node at all via asdf's `~/.tool-versions` — a genuine "not
    /// installed" state, distinct from [`DetectError::Bounded`] above.
    NoNodeFound { distro: String },
}

impl std::fmt::Display for DetectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetectError::NotWsl => write!(f, "node/vtsls detection requires a WSL distro"),
            DetectError::Bounded(err) => write!(f, "{err:#}"),
            DetectError::NoNodeFound { distro } => {
                write!(f, "no node found via asdf inside {distro}")
            }
        }
    }
}

impl std::error::Error for DetectError {}

/// Detect node + vtsls inside `distro` via asdf's `~/.tool-versions` — ONE
/// bounded, spawn-only `sh -c` round trip (see
/// [`detect_script_via_tool_versions`]'s exact text). **Boots a stopped
/// distro** — see [`super`]'s module doc; callers must only invoke this for
/// an already-live distro.
///
/// `node_version_override`, when `Some`, skips the `~/.tool-versions` read
/// and looks up that exact asdf node version directly — a seam for a
/// caller that already knows the version it wants (and keeps the exact-text
/// unit tests below independent of any real `$HOME/.tool-versions`
/// content).
pub fn detect_node_vtsls(
    distro: &str,
    node_version_override: Option<&str>,
) -> Result<NodeVtsls, DetectError> {
    let builder = CommandBuilder::new_spawn_only(RepoLocation::Wsl {
        distro: distro.to_string(),
        path: "/".to_string(),
    });
    let script = detect_script(node_version_override);
    // Deliberately NOT `run_text_timeout`: its blanket `.trim_end()` strips
    // trailing whitespace from the WHOLE blob, including a trailing tab
    // that's structurally part of the last printed field — which happens
    // for real when `vtsls --version` prints nothing (its `#!/usr/bin/env
    // node` shebang can't find `node` without asdf on `PATH`, exactly the
    // env this script exists to route around). That trimmed tab collapses
    // `"vtsls\t<path>\t"` (present, empty version) down to `"vtsls\t<path>"`
    // (only two fields), which `parse_detect_output` would then silently
    // drop as unparseable — caught live against the real Ubuntu distro.
    let bytes = builder
        .run_timeout("sh", &["-c", &script], NODE_DETECT_TIMEOUT)
        .map_err(DetectError::Bounded)?;
    let out = crate::command::decode_output(&bytes);
    parse_detect_output(&out).ok_or_else(|| DetectError::NoNodeFound {
        distro: distro.to_string(),
    })
}

/// Install `@vtsls/language-server` globally via the asdf-resolved `npm`
/// sitting alongside `node.node_path` — the ONE user-environment-mutating
/// fn in this crate (see [`super`]'s module doc: must only ever be called
/// from an explicit, per-row user-consent action, never from a passive
/// check). **Boots a stopped distro** — see [`super`]'s module doc.
///
/// `npm` is invoked as `"$node" "$npm" install -g ...`, NOT `"$npm"
/// install ...` directly — the identical shebang trap
/// [`detect_script_via_tool_versions`]'s doc describes for `vtsls`: asdf's
/// `npm` is a symlink to a JS file whose `#!/usr/bin/env node` shebang
/// can't resolve `node` via `PATH` (asdf is never on it), so execing `npm`
/// directly fails with `env: 'node': No such file or directory` before npm
/// itself ever runs. Passing the resolved `node.node_path` explicitly as
/// the program sidesteps `PATH` entirely, exactly as the detection script
/// already does for `vtsls --version`.
pub fn install_vtsls(distro: &str, node: &NodeVtsls) -> Result<(), InstallError> {
    let npm_path = npm_path_for(&node.node_path)?;
    let builder = CommandBuilder::new_spawn_only(RepoLocation::Wsl {
        distro: distro.to_string(),
        path: "/".to_string(),
    });
    builder
        .run_timeout(
            &node.node_path,
            &[&npm_path, "install", "-g", "@vtsls/language-server"],
            VTSLS_INSTALL_TIMEOUT,
        )
        .map_err(InstallError::Io)?;
    Ok(())
}

/// `npm` sits alongside `node` in the same asdf `bin/` directory — derive
/// its path from `node_path` rather than a second detection round trip.
/// Split out as a pure fn so the derivation is unit-testable without a live
/// builder.
fn npm_path_for(node_path: &str) -> Result<String, InstallError> {
    node_path
        .strip_suffix("/node")
        .map(|bin_dir| format!("{bin_dir}/npm"))
        .ok_or_else(|| InstallError::Io(anyhow!("unexpected node path shape: {node_path}")))
}

fn detect_script(node_version_override: Option<&str>) -> String {
    match node_version_override {
        Some(version) => detect_script_for_version(version),
        None => detect_script_via_tool_versions().to_string(),
    }
}

/// The exact asdf-aware detection one-liner (VERIFIED against the real
/// Ubuntu distro — node 22.22.0 + vtsls 0.3.0 live at
/// `~/.asdf/installs/nodejs/22.22.0/bin`). Reads `~/.tool-versions`'s
/// `nodejs` line to construct the asdf install path deterministically;
/// asdf itself is never on `PATH`. Prints up to two tab-separated lines,
/// `<name>\t<path>\t<version>`, one per component that's actually present
/// and executable — a missing component simply emits no line for it (see
/// [`parse_detect_output`]).
///
/// Opens with `[ -n "$HOME" ] || { ...; exit 1; }` — mirrors
/// [`crate::remote::install`]'s `resolve_home`, which treats `$HOME`
/// resolving empty under this exact `wsl.exe --exec sh -c` (non-login)
/// shape as a real, previously-hit failure, not a hypothetical one. Without
/// this guard, an empty `$HOME` silently makes `nb` root at `/`, `[ -x
/// "$nb/node" ]` fails, the script still exits 0 via the trailing `; true`,
/// and [`detect_node_vtsls`] returns `DetectError::NoNodeFound` —
/// indistinguishable from node genuinely not being installed. Failing this
/// script loudly instead routes that case through
/// [`CommandBuilder::run_timeout`](crate::command::CommandBuilder::run_timeout)'s
/// normal non-zero-exit handling into `DetectError::Bounded` (retryable,
/// [`super::ComponentState::Failed`]) rather than a misleading `Missing`
/// (S8d review, P2).
///
/// `vtsls --version` is invoked as `"$nb/node" "$nb/vtsls" --version`, NOT
/// `"$nb/vtsls" --version` directly — caught live against the real Ubuntu
/// distro. `vtsls` is an asdf-managed symlink to a JS file whose
/// `#!/usr/bin/env node` shebang resolves `node` via `PATH`, which asdf's
/// node is never on (that's this whole script's reason to exist); run
/// directly, `vtsls --version` silently prints nothing (`env: 'node': No
/// such file or directory` on stderr, swallowed by `2>/dev/null`) rather
/// than failing loudly, so this isn't a "gate off vtsls" case — it's a "the
/// binary IS there but needs its own interpreter named explicitly" case.
/// Passing the resolved `$nb/node` explicitly sidesteps `PATH` entirely.
///
/// Ends with `; true` deliberately: without it, the script's own exit
/// status is whatever the LAST `[ -x ... ] && printf ...` clause's test
/// returned, so a present node + absent vtsls (the exact state
/// [`super::ComponentState::NeedsConsent`] exists for) makes the final `[ -x
/// "$nb/vtsls" ]` fail and the whole `sh -c` round trip exit non-zero —
/// [`CommandBuilder::run_timeout`](crate::command::CommandBuilder::run_timeout)
/// then discards the already-printed, valid `node` line and reports it as
/// a spawn failure. `; true` pins the exit status to 0 whenever the script
/// itself ran to completion, regardless of which optional components it
/// found — reproduced against a synthetic asdf-shaped layout (real shell,
/// no WSL needed) by the
/// `detect_script_exits_zero_when_node_present_and_vtsls_absent` test below.
fn detect_script_via_tool_versions() -> &'static str {
    r#"[ -n "$HOME" ] || { printf 'dv: $HOME resolved empty\n' >&2; exit 1; }; v=$(awk '/^nodejs /{print $2;exit}' "$HOME/.tool-versions" 2>/dev/null); nb="$HOME/.asdf/installs/nodejs/$v/bin"; [ -x "$nb/node" ] && printf 'node\t%s\t%s\n' "$nb/node" "$($nb/node --version 2>/dev/null)"; [ -x "$nb/vtsls" ] && printf 'vtsls\t%s\t%s\n' "$nb/vtsls" "$("$nb/node" "$nb/vtsls" --version 2>/dev/null)"; true"#
}

/// Same shape as [`detect_script_via_tool_versions`], with `version`
/// substituted directly in place of the `~/.tool-versions` awk lookup — see
/// [`detect_node_vtsls`]'s doc for when this is used. `version` only ever
/// comes from dv's own process (a caller-supplied asdf version string, e.g.
/// via a test), never untrusted input, so it's embedded unescaped — same
/// posture `crate::remote::install`'s hash-embedding helpers take for their
/// own fixed-shape inputs. Carries the same leading `$HOME`-empty guard (see
/// [`detect_script_via_tool_versions`]'s doc — `nb` is `$HOME`-relative
/// here too) and ends with `; true` for the same exit-status reason
/// documented there.
fn detect_script_for_version(version: &str) -> String {
    format!(
        r#"[ -n "$HOME" ] || {{ printf 'dv: $HOME resolved empty\n' >&2; exit 1; }}; nb="$HOME/.asdf/installs/nodejs/{version}/bin"; [ -x "$nb/node" ] && printf 'node\t%s\t%s\n' "$nb/node" "$($nb/node --version 2>/dev/null)"; [ -x "$nb/vtsls" ] && printf 'vtsls\t%s\t%s\n' "$nb/vtsls" "$("$nb/node" "$nb/vtsls" --version 2>/dev/null)"; true"#
    )
}

/// Pure counterpart to [`detect_script_via_tool_versions`]/
/// [`detect_script_for_version`]: parses their `<name>\t<path>\t<version>`
/// output lines back into a [`NodeVtsls`]. `None` when no `node` line is
/// present at all — vtsls without node is nonsensical and never treated as
/// success.
fn parse_detect_output(output: &str) -> Option<NodeVtsls> {
    let mut node_path = None;
    let mut node_version = None;
    let mut vtsls_path = None;
    let mut vtsls_version = None;

    for line in output.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(name), Some(path), Some(version)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        match name {
            "node" => {
                node_path = Some(path.to_string());
                node_version = Some(version.trim().to_string());
            }
            "vtsls" => {
                vtsls_path = Some(path.to_string());
                vtsls_version = Some(version.trim().to_string());
            }
            _ => {}
        }
    }

    Some(NodeVtsls {
        node_path: node_path?,
        node_version: node_version?,
        vtsls_path,
        vtsls_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_script_via_tool_versions_exact_text() {
        assert_eq!(
            detect_script_via_tool_versions(),
            r#"[ -n "$HOME" ] || { printf 'dv: $HOME resolved empty\n' >&2; exit 1; }; v=$(awk '/^nodejs /{print $2;exit}' "$HOME/.tool-versions" 2>/dev/null); nb="$HOME/.asdf/installs/nodejs/$v/bin"; [ -x "$nb/node" ] && printf 'node\t%s\t%s\n' "$nb/node" "$($nb/node --version 2>/dev/null)"; [ -x "$nb/vtsls" ] && printf 'vtsls\t%s\t%s\n' "$nb/vtsls" "$("$nb/node" "$nb/vtsls" --version 2>/dev/null)"; true"#
        );
    }

    /// Regression for the exit-code trap: both scripts must end in `; true`
    /// so a failed final `[ -x ... ]` test (the exact shape when node is
    /// present but vtsls is absent) doesn't leak into the script's own exit
    /// status — see [`detect_script_via_tool_versions`]'s doc.
    #[test]
    fn detect_scripts_end_with_a_trailing_true_so_exit_status_never_leaks() {
        assert!(detect_script_via_tool_versions().ends_with("; true"));
        assert!(detect_script_for_version("1.2.3").ends_with("; true"));
    }

    #[test]
    fn detect_script_for_version_substitutes_the_version_and_skips_tool_versions() {
        let script = detect_script_for_version("22.22.0");
        assert!(script.contains(r#"nb="$HOME/.asdf/installs/nodejs/22.22.0/bin""#));
        assert!(!script.contains("tool-versions"));
    }

    #[test]
    fn detect_script_dispatches_on_the_override() {
        assert_eq!(detect_script(None), detect_script_via_tool_versions());
        assert_eq!(
            detect_script(Some("1.2.3")),
            detect_script_for_version("1.2.3")
        );
    }

    #[test]
    fn parse_detect_output_both_present() {
        let nv = parse_detect_output(
            "node\t/home/kyle/.asdf/installs/nodejs/22.22.0/bin/node\tv22.22.0\n\
             vtsls\t/home/kyle/.asdf/installs/nodejs/22.22.0/bin/vtsls\t0.3.0",
        )
        .expect("both lines present");
        assert_eq!(
            nv.node_path,
            "/home/kyle/.asdf/installs/nodejs/22.22.0/bin/node"
        );
        assert_eq!(nv.node_version, "v22.22.0");
        assert_eq!(
            nv.vtsls_path.as_deref(),
            Some("/home/kyle/.asdf/installs/nodejs/22.22.0/bin/vtsls")
        );
        assert_eq!(nv.vtsls_version.as_deref(), Some("0.3.0"));
    }

    #[test]
    fn parse_detect_output_node_only_leaves_vtsls_none() {
        let nv = parse_detect_output("node\t/x/node\tv22.22.0").expect("node line present");
        assert_eq!(nv.node_path, "/x/node");
        assert_eq!(nv.node_version, "v22.22.0");
        assert!(nv.vtsls_path.is_none());
        assert!(nv.vtsls_version.is_none());
    }

    #[test]
    fn parse_detect_output_empty_is_none() {
        assert!(parse_detect_output("").is_none());
    }

    /// Regression, run through a REAL shell rather than only comparing the
    /// script's literal text: a synthetic asdf-shaped layout with `node`
    /// present and executable but no `vtsls` at all must still make the
    /// script exit 0 (the `; true` fix above) and yield a `node` line that
    /// [`parse_detect_output`] can parse — before the fix, the final `[ -x
    /// "$nb/vtsls" ]` test failing made the WHOLE `sh -c` round trip exit
    /// non-zero, which `run_timeout` treats as a spawn failure and discards
    /// the already-printed, valid `node` line entirely (caught live against
    /// the real Ubuntu distro in exactly this node-present/vtsls-absent
    /// state — the common pre-onboarding case the consent flow exists for).
    /// Best-effort: skips rather than fails when `sh`/`chmod` aren't usable
    /// on `PATH` (this test needs a real POSIX-ish shell, not guaranteed on
    /// every future CI image; it runs today on this dev machine's Git Bash
    /// and on the `ubuntu-latest`/`windows-latest`/`macos-latest` CI images,
    /// all of which ship one).
    #[test]
    fn detect_script_exits_zero_when_node_present_and_vtsls_absent() {
        use std::process::Command;

        let sh_ok = Command::new("sh")
            .arg("-c")
            .arg("true")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !sh_ok {
            eprintln!("skipping: no working `sh` on PATH");
            return;
        }

        let home = std::env::temp_dir().join(format!(
            "dv-test-node-vtsls-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let bin_dir = home.join(".asdf/installs/nodejs/9.9.9/bin");
        std::fs::create_dir_all(&bin_dir).expect("create fake asdf bin dir");
        let node_path = bin_dir.join("node");
        std::fs::write(&node_path, "#!/bin/sh\necho v9.9.9\n").expect("write fake node");

        let chmod_ok = Command::new("chmod")
            .arg("+x")
            .arg(&node_path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !chmod_ok {
            let _ = std::fs::remove_dir_all(&home);
            eprintln!("skipping: `chmod +x` unavailable/failed on this machine");
            return;
        }

        let script = detect_script_for_version("9.9.9");
        let output = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .env("HOME", &home)
            .output()
            .expect("run sh -c script");

        let _ = std::fs::remove_dir_all(&home);

        assert!(
            output.status.success(),
            "script must exit 0 even when vtsls is absent (got {:?}); stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        let nv = parse_detect_output(&crate::command::decode_output(&output.stdout))
            .expect("the node line should still parse despite vtsls being absent");
        assert!(nv.vtsls_path.is_none());
        assert!(nv.node_path.ends_with("bin/node"));
    }

    /// Regression for the `$HOME`-empty guard (S8d review, P2): unlike
    /// [`detect_script_exits_zero_when_node_present_and_vtsls_absent`]'s
    /// "node present, vtsls absent" case, this must exit NON-zero so
    /// [`detect_node_vtsls`] reads it as `DetectError::Bounded` (retryable)
    /// rather than silently reporting `NoNodeFound` (indistinguishable from
    /// a genuine absence) — see [`detect_script_via_tool_versions`]'s doc.
    /// Empty `$HOME` is forced with a leading `unset HOME;` splice run
    /// through the SAME shell, rather than via `Command::env`/`env_remove`:
    /// on this dev machine's Windows/MSYS `sh.exe`, a Win32-level empty or
    /// absent `HOME` env entry gets silently reconstituted from the OS user
    /// profile before the script ever sees it (an MSYS quirk, not something
    /// the real target — `wsl.exe --exec sh -c` inside a real Linux
    /// distro — does), which made the equivalent `Command::env("HOME", "")`
    /// version of this test flake false-green. Best-effort, same
    /// `sh`-on-`PATH` caveat as the sibling test above.
    #[test]
    fn detect_script_exits_non_zero_when_home_is_empty() {
        use std::process::Command;

        let sh_ok = Command::new("sh")
            .arg("-c")
            .arg("true")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !sh_ok {
            eprintln!("skipping: no working `sh` on PATH");
            return;
        }

        let script = detect_script_for_version("9.9.9");
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("unset HOME; {script}"))
            .output()
            .expect("run sh -c script");

        assert!(
            !output.status.success(),
            "script must fail loudly when $HOME resolves empty, not silently report no node"
        );
    }

    /// Regression: caught live against the real Ubuntu distro before the
    /// `"$nb/node" "$nb/vtsls"` invocation fix — when a component's version
    /// command prints nothing, the script still emits its line with an
    /// EMPTY trailing field (`"vtsls\t<path>\t"`, trailing tab kept). That
    /// must still parse as present-with-empty-version, not be dropped.
    #[test]
    fn parse_detect_output_tolerates_an_empty_trailing_version_field() {
        let nv = parse_detect_output("node\t/x/node\tv22.22.0\nvtsls\t/x/vtsls\t")
            .expect("node line present");
        assert_eq!(nv.vtsls_path.as_deref(), Some("/x/vtsls"));
        assert_eq!(nv.vtsls_version.as_deref(), Some(""));
    }

    #[test]
    fn parse_detect_output_vtsls_without_node_is_none() {
        // Shouldn't happen given the script's own gating, but node missing
        // entirely must never read as success just because vtsls printed.
        assert!(parse_detect_output("vtsls\t/x/vtsls\t0.3.0").is_none());
    }

    #[test]
    fn npm_path_for_derives_the_sibling_npm_binary() {
        assert_eq!(
            npm_path_for("/home/kyle/.asdf/installs/nodejs/22.22.0/bin/node").unwrap(),
            "/home/kyle/.asdf/installs/nodejs/22.22.0/bin/npm"
        );
    }

    #[test]
    fn npm_path_for_rejects_a_path_not_ending_in_node() {
        assert!(npm_path_for("/weird/path/nodejs").is_err());
    }
}
