//! Headless provisioning model for Phase 8's onboarding spine
//! (docs/phase-8-lsp-and-polish.md § Distribution & first-run, extended by
//! the onboarding/consistency spine — doc-deviation 3). This module is the
//! domain the app's onboarding PAGE (S8e) renders: host-side components
//! (`gh`, and on Windows the `dv`-on-PATH row) plus three per-distro ones
//! (`dv-host`, `dv-cli`, `node`/`vtsls`), each reduced to a
//! [`ComponentState`], rolled up into a [`ConsistencyReport`].
//!
//! **BOOT-STORM CONTRACT — read before calling anything here.** Every fn in
//! this module (transitively, via [`crate::remote::install`] and
//! [`node::detect_node_vtsls`]/[`node::install_vtsls`]) that touches a WSL
//! distro routes through [`crate::command::CommandBuilder::new_spawn_only`]
//! (Stage-A `wsl.exe`), which **boots a stopped distro**. This module NEVER
//! decides which distros to touch: [`consistency_check`] checks ONLY the
//! distros the caller passes in `distros_allowed`. The caller — the app,
//! never dv-core — is solely responsible for gating that list, either on
//! [`crate::remote::manager::has_running_host`] for any passive/launch-time
//! walk (exactly as `shell.rs::refresh_all_badges`/`hydrate_index` already
//! gate their own WSL badge walks), or on the one "user is opening this WSL
//! repo right now" moment (which implies the distro is already live). A
//! first-run page that blindly walks every known WSL distro reintroduces
//! the exact boot storm Phase 5/6 eliminated. `gh` is the one component
//! checked unconditionally on every call — it always runs host-side
//! (Windows), never through a distro, so it can never boot anything.
//!
//! **NEVER-FAIL-HARD.** Every fn here reduces to a [`ComponentState`], never
//! a panic and never a hard error that blocks a caller — an absent or
//! failed dependency degrades to `Missing`/`Failed`/`Skipped`, never blocks
//! first paint, never wedges the onboarding page. Every WSL-bound command
//! stays wall-clock bounded, mirroring [`crate::remote::install`]'s
//! `INSTALL_COMMAND_TIMEOUT` convention (see [`node`]'s own timeouts).
//!
//! **CONSENT.** [`node::install_vtsls`] is the ONLY fn in this crate that
//! mutates the user's own environment (an `npm install -g` of a
//! third-party server, `@vtsls/language-server`). It must never be called
//! from [`consistency_check`] or any other passive check — only from an
//! explicit, per-row user-consent action the app wires up (S8e). `gh` is
//! detect-only: dv never runs `gh auth login` on the user's behalf. dv's
//! OWN bits (`dv-host`, `dv-cli`) are the opposite case — they auto-install/
//! auto-repair silently by content hash with no consent needed, because
//! they're dv's own bytes, not a third party's; that's exactly what
//! [`check_dv_host`]/[`check_dv_cli`] do by delegating straight to
//! [`crate::remote::install::ensure_installed`]/
//! [`crate::remote::install::ensure_cli_installed`].

mod local_node;
mod node;
#[cfg(windows)]
mod win_path;

pub(crate) use local_node::vtsls_invocation;
pub use local_node::{DV_VTSLS_PATH_ENV, detect_node_vtsls_local};
pub use node::{DetectError, NodeVtsls, detect_node_vtsls, install_vtsls};
#[cfg(windows)]
pub use win_path::add_dv_to_path;

/// Non-Windows stub for the [`ConsentAction::AddDvToPath`] executor: the
/// variant itself is unconditional (the app's exhaustive matches must
/// compile on every platform), but no non-Windows check ever produces it,
/// so this is unreachable in practice. A macOS PATH story (a
/// `/usr/local/bin` symlink) is a separate, future component.
#[cfg(not(windows))]
pub fn add_dv_to_path(_dir: &str) -> anyhow::Result<()> {
    anyhow::bail!("PATH registration is only implemented on Windows")
}

use crate::github;
use crate::remote::install::{self, HostBinarySource};

/// The components the onboarding spine tracks. `Copy` because it's
/// used as a cheap tag on [`ComponentReport`] and compared/matched freely by
/// the app's rendering code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentId {
    GhCli,
    /// Windows only: whether the running `dv.exe`'s directory is on the
    /// user's PATH (see [`win_path`]). Never produced on other platforms.
    DvOnPath,
    DvHost,
    DvCli,
    NodeVtsls,
}

impl ComponentId {
    /// Human-readable base title (no distro suffix — [`consistency_check`]
    /// appends that for the three per-distro components).
    pub fn title(self) -> &'static str {
        match self {
            ComponentId::GhCli => "GitHub CLI (gh)",
            ComponentId::DvOnPath => "Terminal command (dv on PATH)",
            ComponentId::DvHost => "WSL host (dv-host)",
            ComponentId::DvCli => "Native CLI (dv)",
            ComponentId::NodeVtsls => "TypeScript language server (vtsls)",
        }
    }
}

/// A consent action the onboarding page can offer the user, carried by
/// [`ComponentState::NeedsConsent`] so the app's consent button has
/// everything it needs to invoke the mutating fn without a second detection
/// round trip.
#[derive(Debug, Clone)]
pub enum ConsentAction {
    /// Invoke [`node::install_vtsls`] with these exact args once the user
    /// clicks "install" on the row.
    InstallVtsls { distro: String, node: NodeVtsls },
    /// Invoke [`add_dv_to_path`] with this directory (the running
    /// `dv.exe`'s own dir) once the user clicks the row's consent button.
    /// Windows only in practice — see [`ComponentId::DvOnPath`].
    AddDvToPath { dir: String },
}

/// The state of one provisioning component, as far as the last check could
/// tell. Never a panic, never an unhandled error — every detection/install
/// path in this module reduces to one of these (see the module doc's
/// never-fail-hard contract).
#[derive(Debug, Clone)]
pub enum ComponentState {
    /// Present and working. `detail` is a short human-readable status line
    /// ("gh 2.96 · authed", "node v22.22.0 · vtsls 0.3.0").
    Ok { detail: String },
    /// Absent, but the user can fix it themselves — `guidance` is the
    /// actionable next step (install instructions, `gh auth login`, ...).
    Missing { guidance: String },
    /// Detected as installable, but the install would mutate the user's own
    /// environment — needs an explicit consent click before
    /// [`ConsentAction`] is invoked.
    NeedsConsent {
        action: ConsentAction,
        detail: String,
    },
    /// An install/re-provision is actively in flight. Not produced by
    /// [`consistency_check`] itself (that call is synchronous end-to-end for
    /// dv's own bits); the app's onboarding page sets this on a row locally
    /// while a background consent-triggered install task is running.
    Installing,
    /// Something went wrong beyond "not installed" — a spawn failure, a
    /// verify-after-install mismatch, a wedged command that hit its bound.
    Failed { error: String },
    /// Not applicable right now, and that's fine — no distro running to
    /// check, no WSL repo open, this build has no sidecar for the
    /// component. Never blocks anything downstream.
    Skipped { reason: String },
}

/// One row of the onboarding page / consistency check.
#[derive(Debug, Clone)]
pub struct ComponentReport {
    pub id: ComponentId,
    pub title: String,
    pub state: ComponentState,
}

/// The result of [`consistency_check`] — every row it produced, plus a
/// single rolled-up `drift` flag: `true` when at least one row isn't fully
/// settled (anything other than [`ComponentState::Ok`]/
/// [`ComponentState::Skipped`]), which is the app's cue to surface the
/// onboarding page instead of staying silent.
#[derive(Debug, Clone)]
pub struct ConsistencyReport {
    pub components: Vec<ComponentReport>,
    pub drift: bool,
}

impl ConsistencyReport {
    /// A stable identity for this report's "needs a human" subset —
    /// `(id, state KIND, title)` tuples, sorted then hashed. `None` exactly
    /// when `drift` is `false` (nothing here needs a human at all). Kind
    /// only, never the human-readable `guidance`/`detail`/`error` text: two
    /// checks describing the same underlying problem (the same declined
    /// vtsls consent, re-detected on a later launch) must fingerprint
    /// identically even if the message wording changed across an app
    /// version. Used by the app (`shell.rs`) to persist "the user already
    /// dismissed exactly this" across restarts, distinguishing it from
    /// "something genuinely new needs attention" (phase-8 capstone review,
    /// P3: drift dismissal used to be session-only, so a permanent-by-
    /// choice state — no `gh`, a declined vtsls consent — re-popped the
    /// onboarding page on every single launch, forever).
    pub fn needs_human_fingerprint(&self) -> Option<String> {
        let mut entries: Vec<(String, &'static str, String)> = self
            .components
            .iter()
            .filter_map(|c| {
                let kind = match &c.state {
                    ComponentState::Missing { .. } => "missing",
                    ComponentState::NeedsConsent { .. } => "consent",
                    ComponentState::Installing => "installing",
                    ComponentState::Failed { .. } => "failed",
                    ComponentState::Ok { .. } | ComponentState::Skipped { .. } => return None,
                };
                Some((format!("{:?}", c.id), kind, c.title.clone()))
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        entries.sort();
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        entries.hash(&mut hasher);
        Some(format!("{:016x}", hasher.finish()))
    }
}

/// Check every component: `gh` (and, on Windows, dv-on-PATH)
/// unconditionally (host-side, never boots anything), then
/// `dv-host`/`dv-cli`/`node`+`vtsls` for every distro in
/// `distros_allowed` — and ONLY those. See the module doc's boot-storm
/// contract: this fn will boot a stopped distro if the caller passes one in
/// `distros_allowed`, so the caller must have already gated that list on
/// [`crate::remote::manager::has_running_host`] (or an explicit "opening
/// this repo now" trigger) before calling.
///
/// `dv-host`/`dv-cli` checks silently re-provision on content-hash drift
/// (see [`check_dv_host`]/[`check_dv_cli`]) — no consent needed, they're
/// dv's own bytes. `node`/`vtsls` checks are read-only; a present node with
/// no vtsls surfaces [`ComponentState::NeedsConsent`] rather than installing
/// anything (see the module doc's consent contract).
pub fn consistency_check(distros_allowed: &[String]) -> ConsistencyReport {
    let mut components = vec![ComponentReport {
        id: ComponentId::GhCli,
        title: ComponentId::GhCli.title().to_string(),
        state: github::gh_status(),
    }];
    // Host-side like `gh` (registry read only — never touches a distro,
    // never boots anything), so it's checked unconditionally too.
    #[cfg(windows)]
    components.push(ComponentReport {
        id: ComponentId::DvOnPath,
        title: ComponentId::DvOnPath.title().to_string(),
        state: win_path::check_dv_on_path(),
    });

    for distro in distros_allowed {
        components.push(ComponentReport {
            id: ComponentId::DvHost,
            title: format!("{} — {distro}", ComponentId::DvHost.title()),
            state: check_dv_host(distro),
        });
        components.push(ComponentReport {
            id: ComponentId::DvCli,
            title: format!("{} — {distro}", ComponentId::DvCli.title()),
            state: check_dv_cli(distro),
        });
        components.push(ComponentReport {
            id: ComponentId::NodeVtsls,
            title: format!("{} — {distro}", ComponentId::NodeVtsls.title()),
            state: check_node_vtsls(distro),
        });
    }

    let drift = compute_drift(&components);
    ConsistencyReport { components, drift }
}

/// `dv-host`'s row: delegate straight to
/// [`crate::remote::install::ensure_installed`], which silently streams a
/// fresh copy on content-hash drift and is a cheap no-op when the marker
/// already matches. `InstallError::NoSidecar` (this build carries no
/// `dv-host-linux-x64` sidecar) is `Skipped`, not `Failed` — mirrors
/// `install.rs`'s own silent handling of that case (a sidecar that appears
/// later just works on the next check).
fn check_dv_host(distro: &str) -> ComponentState {
    match install::ensure_installed(distro) {
        Ok(bin) => ComponentState::Ok {
            detail: managed_detail(bin.source, "dv-host"),
        },
        Err(install::InstallError::NoSidecar { component }) => ComponentState::Skipped {
            reason: format!("no {component} sidecar shipped with this build"),
        },
        Err(err) => ComponentState::Failed {
            error: err.to_string(),
        },
    }
}

/// `dv-cli`'s row — same shape as [`check_dv_host`], over
/// [`crate::remote::install::ensure_cli_installed`].
fn check_dv_cli(distro: &str) -> ComponentState {
    match install::ensure_cli_installed(distro) {
        Ok(bin) => ComponentState::Ok {
            detail: managed_detail(bin.source, "dv-cli"),
        },
        Err(install::InstallError::NoSidecar { component }) => ComponentState::Skipped {
            reason: format!("no {component} sidecar shipped with this build"),
        },
        Err(err) => ComponentState::Failed {
            error: err.to_string(),
        },
    }
}

fn managed_detail(source: HostBinarySource, label: &str) -> String {
    match source {
        HostBinarySource::DevOverride => format!("{label} (dev override)"),
        HostBinarySource::Managed => format!("{label} installed"),
    }
}

/// `node`/`vtsls`'s row — read-only detection only, never installs
/// anything. A present node with no vtsls becomes
/// [`ComponentState::NeedsConsent`] (the app offers an install button); a
/// genuinely absent node becomes [`ComponentState::Missing`] (nothing dv
/// can install on the user's behalf — asdf/node setup is out of scope). A
/// transient/environmental failure of the detection round trip itself
/// (spawn error, hit its timeout — [`DetectError::Bounded`]) becomes
/// [`ComponentState::Failed`] instead: mapping it to `Missing` would tell
/// the user to go install node when node may well already be there and the
/// real problem was e.g. a wedged distro — see [`DetectError::Bounded`]'s
/// doc.
fn check_node_vtsls(distro: &str) -> ComponentState {
    match detect_node_vtsls(distro, None) {
        Ok(nv) => match (&nv.vtsls_path, &nv.vtsls_version) {
            (Some(_), Some(vtsls_version)) if !vtsls_version.trim().is_empty() => {
                ComponentState::Ok {
                    detail: format!("node {} · vtsls {vtsls_version}", nv.node_version),
                }
            }
            // Executable bit set (`[ -x "$nb/vtsls" ]` passed) but `--version`
            // printed nothing — e.g. an `npm install -g` killed mid-write by
            // `install_vtsls`'s bounded timeout, or a corrupted node_modules.
            // Reporting this as `Ok` would mask a broken install as healthy
            // (S8d review, P2); route it through the same consent-install
            // affordance as "not installed" so the row offers a reinstall
            // instead of a false green.
            (Some(_), Some(_)) => ComponentState::NeedsConsent {
                detail: format!(
                    "node {} found, vtsls present but not responding to --version \
                     — reinstall",
                    nv.node_version
                ),
                action: ConsentAction::InstallVtsls {
                    distro: distro.to_string(),
                    node: nv,
                },
            },
            _ => ComponentState::NeedsConsent {
                detail: format!("node {} found, vtsls not installed", nv.node_version),
                action: ConsentAction::InstallVtsls {
                    distro: distro.to_string(),
                    node: nv,
                },
            },
        },
        Err(DetectError::NotWsl) => ComponentState::Skipped {
            reason: "not a WSL repo".to_string(),
        },
        Err(DetectError::NoNodeFound { distro }) => ComponentState::Missing {
            guidance: format!("node not found via asdf (~/.tool-versions) inside {distro}"),
        },
        // Structurally unreachable: `detect_node_vtsls` (the WSL detector
        // this fn calls) never constructs this variant — it's
        // `detect_node_vtsls_local`'s own error, and this fn is never
        // called with a local location (`consistency_check` only ever
        // loops over WSL distros — see this module's doc comment). Kept as
        // a real, never-fail-hard arm rather than an `unreachable!()`
        // anyway: matching the enum exhaustively must not become a panic
        // risk just because one caller happens not to hit this path today.
        Err(DetectError::LocalNodeNotFound) => ComponentState::Failed {
            error: format!("unexpected local-only detection error for WSL distro {distro}"),
        },
        Err(DetectError::Bounded(err)) => ComponentState::Failed {
            error: format!("node/vtsls detection failed inside {distro}: {err:#}"),
        },
    }
}

/// `true` when any row needs attention — anything other than
/// [`ComponentState::Ok`]/[`ComponentState::Skipped`] (which are both
/// "nothing to do here" states). Split out as a pure fn so it's unit
/// testable against synthetic rows without a live gh/WSL round trip.
fn compute_drift(components: &[ComponentReport]) -> bool {
    components.iter().any(|c| {
        !matches!(
            c.state,
            ComponentState::Ok { .. } | ComponentState::Skipped { .. }
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(state: ComponentState) -> ComponentReport {
        ComponentReport {
            id: ComponentId::GhCli,
            title: "test".to_string(),
            state,
        }
    }

    #[test]
    fn drift_is_false_when_everything_is_ok_or_skipped() {
        let components = vec![
            report(ComponentState::Ok {
                detail: "fine".to_string(),
            }),
            report(ComponentState::Skipped {
                reason: "n/a".to_string(),
            }),
        ];
        assert!(!compute_drift(&components));
    }

    #[test]
    fn drift_is_true_when_anything_needs_attention() {
        for state in [
            ComponentState::Missing {
                guidance: "x".to_string(),
            },
            ComponentState::NeedsConsent {
                action: ConsentAction::InstallVtsls {
                    distro: "Ubuntu".to_string(),
                    node: NodeVtsls {
                        node_path: "/x/node".to_string(),
                        node_version: "v22.22.0".to_string(),
                        vtsls_path: None,
                        vtsls_version: None,
                    },
                },
                detail: "x".to_string(),
            },
            ComponentState::Installing,
            ComponentState::Failed {
                error: "x".to_string(),
            },
        ] {
            assert!(compute_drift(&[report(state)]));
        }
    }

    #[test]
    fn component_id_titles_are_stable() {
        assert_eq!(ComponentId::GhCli.title(), "GitHub CLI (gh)");
        assert_eq!(ComponentId::DvHost.title(), "WSL host (dv-host)");
        assert_eq!(ComponentId::DvCli.title(), "Native CLI (dv)");
        assert_eq!(
            ComponentId::NodeVtsls.title(),
            "TypeScript language server (vtsls)"
        );
    }

    #[test]
    #[ignore = "consistency_check(&[]) still checks gh unconditionally (by design — \
                see the module doc), which hits the real gh binary and the network; \
                mirrors the identical #[ignore]'d test in \
                crates/core/tests/wsl_provision_detect.rs (S8d review, P3)"]
    fn consistency_check_with_no_distros_only_checks_gh() {
        // gh always runs host-side, never boots anything — this must be
        // safe to call with an empty distro list (the exact call the app
        // makes when no WSL distro is currently live).
        let report = consistency_check(&[]);
        assert_eq!(report.components.len(), 1);
        assert_eq!(report.components[0].id, ComponentId::GhCli);
    }

    #[test]
    fn needs_human_fingerprint_is_none_when_everything_ok_or_skipped() {
        let report = ConsistencyReport {
            drift: false,
            components: vec![
                report(ComponentState::Ok {
                    detail: "fine".to_string(),
                }),
                report(ComponentState::Skipped {
                    reason: "n/a".to_string(),
                }),
            ],
        };
        assert_eq!(report.needs_human_fingerprint(), None);
    }

    #[test]
    fn needs_human_fingerprint_is_stable_and_order_independent() {
        let missing = report(ComponentState::Missing {
            guidance: "x".to_string(),
        });
        let failed = report(ComponentState::Failed {
            error: "y".to_string(),
        });
        let forward = ConsistencyReport {
            drift: true,
            components: vec![missing.clone(), failed.clone()],
        };
        let reversed = ConsistencyReport {
            drift: true,
            components: vec![failed, missing],
        };
        let fp_forward = forward.needs_human_fingerprint();
        let fp_reversed = reversed.needs_human_fingerprint();
        assert!(fp_forward.is_some());
        assert_eq!(fp_forward, fp_reversed);
    }

    #[test]
    fn needs_human_fingerprint_ignores_message_text_but_not_kind() {
        let base = ConsistencyReport {
            drift: true,
            components: vec![report(ComponentState::Missing {
                guidance: "install gh from https://cli.github.com".to_string(),
            })],
        };
        let reworded = ConsistencyReport {
            drift: true,
            components: vec![report(ComponentState::Missing {
                guidance: "totally different wording".to_string(),
            })],
        };
        let different_kind = ConsistencyReport {
            drift: true,
            components: vec![report(ComponentState::Failed {
                error: "install gh from https://cli.github.com".to_string(),
            })],
        };
        assert_eq!(
            base.needs_human_fingerprint(),
            reworded.needs_human_fingerprint(),
            "message wording alone must not change the fingerprint"
        );
        assert_ne!(
            base.needs_human_fingerprint(),
            different_kind.needs_human_fingerprint(),
            "a different state kind must change the fingerprint"
        );
    }
}
