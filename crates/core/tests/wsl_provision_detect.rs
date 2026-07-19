//! Live WSL detection tests for Phase 8's provisioning model (S8d,
//! docs/phase-8-lsp-and-polish.md's onboarding spine) — exercises
//! [`dv_core::provision::detect_node_vtsls`] and
//! [`dv_core::provision::consistency_check`] against the REAL Ubuntu distro,
//! where node is asdf-managed (`~/.tool-versions` → `nodejs 22.22.0`) and
//! vtsls 0.3.0 is installed alongside it at
//! `~/.asdf/installs/nodejs/22.22.0/bin/vtsls` (VERIFIED — see
//! docs/phase-8-lsp-and-polish.md and `crates/core/src/provision/node.rs`'s
//! module doc).
//!
//! Run:
//!
//!   cargo test -p dv-core --test wsl_provision_detect -- --ignored --nocapture
//!
//! **BOOT-STORM NOTE:** [`dv_core::provision`]'s module doc forbids dv-core
//! itself from ever deciding which distros to touch — every fn here is
//! called with an explicit, hardcoded `Ubuntu`, standing in for the app's
//! own "this distro is already live" gate (never a blind walk of every
//! known distro). This file is deliberately the one place outside
//! `dv_core::provision` allowed to name a distro directly, because it's
//! simulating that already-gated caller, not replacing the gate.

use dv_core::provision::{self, ComponentId};
use dv_core::{CommandBuilder, RepoLocation};

const DISTRO: &str = "Ubuntu";

#[test]
#[ignore = "requires WSL Ubuntu with asdf node 22.22.0 + vtsls 0.3.0 installed — see module docs"]
fn detect_node_vtsls_finds_the_real_asdf_install() {
    let nv = provision::detect_node_vtsls(DISTRO, None)
        .expect("detect_node_vtsls against the real Ubuntu distro");

    assert!(
        nv.node_path
            .ends_with("/.asdf/installs/nodejs/22.22.0/bin/node"),
        "unexpected node path: {}",
        nv.node_path
    );
    assert!(
        nv.node_version.contains("22.22.0"),
        "unexpected node version: {}",
        nv.node_version
    );

    let vtsls_path = nv.vtsls_path.as_deref().expect("vtsls should be detected");
    assert!(
        vtsls_path.ends_with("/.asdf/installs/nodejs/22.22.0/bin/vtsls"),
        "unexpected vtsls path: {vtsls_path}"
    );
    let vtsls_version = nv
        .vtsls_version
        .as_deref()
        .expect("vtsls version should be detected");
    assert!(
        vtsls_version.contains("0.3.0"),
        "unexpected vtsls version: {vtsls_version}"
    );
}

/// The exact same asdf version, looked up explicitly via
/// `node_version_override` instead of `~/.tool-versions`, must resolve to
/// the identical binaries — proves the override seam actually reaches the
/// real distro, not just its own unit-tested script text.
#[test]
#[ignore = "requires WSL Ubuntu with asdf node 22.22.0 + vtsls 0.3.0 installed — see module docs"]
fn detect_node_vtsls_override_matches_the_tool_versions_lookup() {
    let via_tool_versions =
        provision::detect_node_vtsls(DISTRO, None).expect("detect_node_vtsls via ~/.tool-versions");
    let via_override = provision::detect_node_vtsls(DISTRO, Some("22.22.0"))
        .expect("detect_node_vtsls via explicit override");
    assert_eq!(via_tool_versions, via_override);
}

/// `consistency_check` over one live distro reports exactly the host-side
/// rows — gh (always) plus, on Windows, dv-on-PATH — followed by
/// dv-host/dv-cli/node+vtsls for that one distro, in a stable order.
/// Doesn't assert every row is `Ok`: this test binary carries no
/// `dv-host-linux-x64`/`dv-linux-x64` sidecar next to it, so those two
/// rows are expected to read `Skipped` (no sidecar), not `Ok` — see
/// `wsl_host_provision.rs`/`wsl_cli_provision.rs` for the tests that
/// actually exercise the install path with a real sidecar.
#[test]
#[ignore = "requires WSL Ubuntu — see module docs"]
fn consistency_check_reports_expected_rows_for_one_distro() {
    let report = provision::consistency_check(&[DISTRO.to_string()]);

    let mut expected = vec![ComponentId::GhCli];
    if cfg!(windows) {
        expected.push(ComponentId::DvOnPath);
    }
    expected.extend([
        ComponentId::DvHost,
        ComponentId::DvCli,
        ComponentId::NodeVtsls,
    ]);

    let ids: Vec<ComponentId> = report.components.iter().map(|c| c.id).collect();
    assert_eq!(ids, expected, "got: {:#?}", report.components);
}

/// The boot-storm-safety proof at the API level: an empty `distros_allowed`
/// must check ONLY `gh` (host-side, never boots anything) and never touch
/// WSL at all.
#[test]
#[ignore = "requires WSL Ubuntu to be a meaningful contrast run alongside the above — see module docs"]
fn consistency_check_with_no_distros_only_checks_host_side_rows() {
    let report = provision::consistency_check(&[]);
    assert_eq!(report.components.len(), if cfg!(windows) { 2 } else { 1 });
    assert_eq!(report.components[0].id, ComponentId::GhCli);
    if cfg!(windows) {
        assert_eq!(report.components[1].id, ComponentId::DvOnPath);
    }
}

/// Live regression for the S8d review P1 fix: `install_vtsls` used to exec
/// npm's shebang-script path directly (`wsl.exe --exec <npm> install ...`),
/// which fails with `env: 'node': No such file or directory` in exactly the
/// asdf environment this module targets — npm, like vtsls, is a
/// `#!/usr/bin/env node` script and asdf is never on `PATH` (see
/// `crates/core/src/provision/node.rs`'s module doc). This test uninstalls
/// the real vtsls, confirms `detect_node_vtsls` still succeeds with
/// `vtsls_path: None` rather than erroring (the companion script
/// exit-code fix — a present node with vtsls absent must not read as a
/// detection failure), then calls `install_vtsls` and confirms it actually
/// reinstalls vtsls via the fixed `node <npm> install ...` invocation.
///
/// **Mutates the real distro**: uninstalls, then reinstalls,
/// `@vtsls/language-server`. Run manually and deliberately — never part of
/// an automated gate.
#[test]
#[ignore = "requires WSL Ubuntu with asdf node — UNINSTALLS then REINSTALLS the real vtsls; run manually and deliberately, see module docs"]
fn install_vtsls_recovers_after_being_uninstalled() {
    let before = provision::detect_node_vtsls(DISTRO, None)
        .expect("detect_node_vtsls against the real Ubuntu distro");
    assert!(
        before.vtsls_path.is_some(),
        "test precondition: vtsls must already be installed"
    );
    let npm_path = before
        .node_path
        .strip_suffix("/node")
        .map(|bin_dir| format!("{bin_dir}/npm"))
        .expect("node path should end in /node");

    // Uninstall via the resolved node interpreter — the SAME shebang trap
    // `install_vtsls` itself now works around applies to npm run bare.
    let builder = CommandBuilder::new(RepoLocation::Wsl {
        distro: DISTRO.to_string(),
        path: "/".to_string(),
    });
    builder
        .run(
            &before.node_path,
            &[&npm_path, "uninstall", "-g", "@vtsls/language-server"],
        )
        .expect("uninstall vtsls via node <npm> uninstall");

    let after_uninstall = provision::detect_node_vtsls(DISTRO, None).expect(
        "detect_node_vtsls must still succeed with vtsls absent — the script exit-code fix",
    );
    assert!(
        after_uninstall.vtsls_path.is_none(),
        "vtsls should read as absent immediately after uninstall"
    );

    provision::install_vtsls(DISTRO, &after_uninstall)
        .expect("install_vtsls should reinstall vtsls via the fixed node <npm> invocation");

    let after_reinstall =
        provision::detect_node_vtsls(DISTRO, None).expect("detect_node_vtsls after reinstall");
    assert!(
        after_reinstall.vtsls_path.is_some(),
        "vtsls should be detected again after install_vtsls"
    );
}
