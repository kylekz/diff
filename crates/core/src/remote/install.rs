//! Sidecar resolution + install/upgrade flow for dv's own managed binaries
//! (docs/phase-5-implementation-plan.md §5, §8 S3; generalized in
//! docs/phase-8-lsp-and-polish.md's onboarding spine, §8 S8c). Consumed by
//! [`super::manager::client_for`], which calls [`ensure_installed`] inside
//! its per-distro lock to turn "no `DV_HOST_PATH`" into a real, in-distro
//! POSIX path — installing (or upgrading) the binary first if needed.
//!
//! Every subprocess here goes through [`CommandBuilder::new_spawn_only`]
//! (plain `wsl.exe -d <distro> --exec` — Stage A) rather than
//! [`CommandBuilder::new`]: the host isn't up yet by definition (that's the
//! whole point of this module), and `new` would recurse back into
//! `manager::client_for` for the same distro, deadlocking on the very lock
//! [`ensure_installed`]'s caller is already holding.
//!
//! **Deviation from the plan, called out explicitly (pre-approved — see the
//! Phase 5 S3 task):** §5 says the install directory should be named after
//! the sidecar's *version string*. That string isn't knowable client-side
//! without executing the (Linux) binary from Windows, which we can't do —
//! so this module names the directory after the first 16 hex characters of
//! the sidecar's own content hash instead. That's strictly more correct for
//! dev-dirty builds (two builds of the same `Cargo.toml` version but
//! different commits get different directories, and identical bytes always
//! reuse the same one) and costs nothing distribution-side, since the
//! marker file this module writes already carries the full hash anyway.
//!
//! **S8c generalization:** the pipeline above was written, hardened, and
//! reviewed for exactly one binary (`dv-host`). Phase 8's onboarding spine
//! needs a SECOND managed binary — the native `dv` CLI (crate `dv-cli`,
//! S8b), installed to a STABLE path (`~/.local/bin/dv`, so it can sit on
//! `PATH`) rather than a hash-named directory. [`ManagedSpec`] +
//! [`InstallLayout`] parameterize the same stream/verify pipeline over
//! either shape without duplicating it: [`HOST_SPEC`] reproduces today's
//! `dv-host` behavior byte-for-byte (proven by the exact-string unit tests
//! below staying green unchanged), and [`CLI_SPEC`] is the new `dv` CLI
//! path. [`ensure_installed`]/[`force_reinstall`] stay the exact public API
//! [`super::manager::client_for`] already calls — now thin wrappers over the
//! generalized [`ensure_installed_spec`]/[`force_reinstall_spec`] machinery.
//! [`ensure_cli_installed`]/[`cli_install_marker`] are the new CLI_SPEC
//! entry points (the latter reads the marker WITHOUT streaming, for a fast
//! per-launch consistency check).
//!
//! Because [`InstallLayout::StablePath`] has no hash-named directory, a
//! present binary can never be trusted on its own — the marker file is the
//! primary drift signal. A `~/.local/bin/dv` with a missing or mismatched
//! marker MUST be treated as drift and re-provisioned, never assumed-good
//! (see [`InstallLayout::StablePath`]'s doc and [`is_drift`]). The mirror
//! case matters too: `bin_dir` and `marker_dir` are separate directories, so
//! an out-of-band deletion of just the binary can leave a still-matching
//! marker behind. [`install_spec`]'s non-force check and
//! [`cli_install_marker`] both fold a binary-presence stat into the same
//! round trip as the marker read
//! ([`read_marker_and_binary_presence`]) so that state is drift too, never
//! assumed-good.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::anyhow;
use sha2::{Digest, Sha256};

use crate::command::CommandBuilder;
use crate::location::RepoLocation;

/// The Windows-side sidecar file name, resolved next to `dv.exe`.
const SIDECAR_FILENAME: &str = "dv-host-linux-x64";
/// The installed binary's file name inside its content-hash directory.
const BINARY_FILENAME: &str = "dv-host";
/// The marker file recording the full sha256 of the binary currently
/// installed at this directory.
const MARKER_FILENAME: &str = "dv-host.sha256";

/// The Windows-side sidecar for the native `dv` CLI (built by `dv-cli`, S8b,
/// as `x86_64-unknown-linux-musl`, renamed by the release pipeline).
const CLI_SIDECAR_FILENAME: &str = "dv-linux-x64";
/// The CLI installs under dv's own name — it IS `dv` on the user's `PATH`.
const CLI_BINARY_FILENAME: &str = "dv";
/// The CLI's marker file — see [`InstallLayout::StablePath`]'s doc for why
/// this is the sole drift signal for a stable-path install.
const CLI_MARKER_FILENAME: &str = "dv.sha256";

/// Env var overriding the install root a [`ManagedSpec`] resolves against —
/// verbatim, replacing the computed `$HOME`-relative default entirely (test
/// seam). Named after `dv-host` for backward compatibility (predates this
/// generalization) but honored by every [`InstallLayout`], including
/// [`InstallLayout::StablePath`]'s `marker_dir` (never its `bin_dir` — see
/// that variant's doc for why the binary path itself is never redirected).
const INSTALL_ROOT_ENV: &str = "DV_HOST_INSTALL_ROOT";

/// Wall-clock bound on every install bootstrap command (plan §5, §8 S5) —
/// `$HOME` resolution, marker read/delete, and the binary stream, all of
/// which run under `manager::client_for`'s per-distro lock with no other
/// timeout of their own. Generous on purpose: it must comfortably cover a
/// cold distro boot (the SAME boot the 15s handshake timeout budgets for,
/// plus the time to actually run a tiny shell one-liner afterward) — a wait
/// beyond this is a genuine wedge, not just a slow-but-normal boot, and
/// should degrade to `InstallError::Io` (→ `Failed` + cool-down) rather than
/// blocking every future `client_for` call for this distro indefinitely.
const INSTALL_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Where the runnable `dv-host` binary this call resolved to actually came
/// from — [`super::manager::client_for`] uses this to decide whether a
/// handshake proto mismatch is worth a forced reinstall (only [`Managed`]
/// binaries have a marker to invalidate) or should just cool down like any
/// other spawn failure ([`DevOverride`] is an arbitrary dev-supplied path
/// with no install flow behind it at all).
///
/// [`Managed`]: HostBinarySource::Managed
/// [`DevOverride`]: HostBinarySource::DevOverride
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostBinarySource {
    /// Resolved straight from `DV_HOST_PATH` — the dev loop bypass. Skips
    /// sidecar lookup, hashing, and the install pipeline entirely; there is
    /// no marker anywhere to reinstall.
    DevOverride,
    /// Resolved (and, if needed, freshly streamed) through the sidecar
    /// install flow below.
    Managed,
}

/// The result of a successful [`ensure_installed`]/[`force_reinstall`] call.
#[derive(Debug, Clone)]
pub struct HostBinary {
    /// Absolute POSIX path *inside the distro* to the runnable binary —
    /// suitable to hand straight to [`super::client::HostClient::spawn_wsl`]
    /// as its `--exec` argument (which runs with no shell, so this must
    /// never contain an unexpanded `$HOME` or other shell syntax).
    pub path: String,
    pub source: HostBinarySource,
}

/// Where a [`ManagedSpec`]'s installed binary (and its drift marker) live
/// inside the distro.
#[derive(Debug, Clone, Copy)]
pub enum InstallLayout {
    /// `<root>/<hash-prefix>/<binary_filename>`, marker colocated in the
    /// same hash-named directory — today's `dv-host` layout (see the module
    /// doc's pre-approved content-hash-directory deviation). `<root>` is
    /// `~/.local/share/dv/<subdir>` by default, or [`INSTALL_ROOT_ENV`]
    /// verbatim when set.
    HashDir {
        subdir: &'static str,
        binary_filename: &'static str,
    },
    /// `<home>/<bin_dir>/<binary_filename>` — a STABLE path (no hash-named
    /// directory, so it can sit on `PATH`) whose marker is tracked
    /// SEPARATELY at `<home>/<marker_dir>/<marker_filename>` (or
    /// [`INSTALL_ROOT_ENV`] verbatim when set — a test seam; see
    /// [`stable_dirs`]). `bin_dir` itself is never redirected by
    /// [`INSTALL_ROOT_ENV`]: the whole point of a stable path is that it's
    /// the real, on-`PATH` location (docs/phase-8-lsp-and-polish.md
    /// doc-deviation 5 — dv owns the `dv` name there). Because the binary
    /// path never moves, the marker is the primary drift signal: a binary
    /// present with a missing or mismatched marker is drift, never
    /// assumed-good (see [`is_drift`]). The mirror case is drift too: since
    /// `bin_dir` and `marker_dir` are separate directories, a matching
    /// marker with the binary itself missing (out-of-band deletion) must
    /// not be assumed-good either — callers stat the binary alongside the
    /// marker read (see [`read_marker_and_binary_presence`]) rather than
    /// trusting the marker alone.
    StablePath {
        bin_dir: &'static str,
        binary_filename: &'static str,
        marker_dir: &'static str,
    },
}

/// A binary dv installs/upgrades by content hash inside a distro.
/// [`HOST_SPEC`] is today's `dv-host`; [`CLI_SPEC`] is the native `dv` CLI
/// added for Phase 8's onboarding spine (docs/phase-8-lsp-and-polish.md).
#[derive(Debug, Clone, Copy)]
pub struct ManagedSpec {
    /// Windows-side sidecar file name, resolved next to the running `dv.exe`
    /// (see [`sidecar_path`]/[`sidecar_path_for`]).
    pub sidecar_filename: &'static str,
    /// Env var overriding the sidecar search entirely — a test seam letting
    /// tests point at any local file standing in for the sidecar.
    pub sidecar_env: &'static str,
    /// Env var bypassing the install flow altogether with an in-distro path
    /// (the dev loop bypass — no marker, no verification, used as-is).
    pub dev_override_env: &'static str,
    /// File name recording the full sha256 of the currently-installed
    /// binary.
    pub marker_filename: &'static str,
    /// Human-readable name of the managed binary this spec installs —
    /// interpolated into [`InstallError`]'s `Display` text so a CLI_SPEC
    /// failure reads "dv-cli", not a hardcoded "dv-host" left over from
    /// before this type was generalized (S8c review, P2/P3).
    pub component: &'static str,
    pub layout: InstallLayout,
}

/// dv-host: today's exact, hardened layout — reproduced byte-for-byte so the
/// exact-string unit tests below (written against the pre-generalization
/// module) stay green unchanged, which IS the regression proof this
/// refactor left `dv-host`'s behavior untouched.
pub const HOST_SPEC: ManagedSpec = ManagedSpec {
    sidecar_filename: SIDECAR_FILENAME,
    sidecar_env: "DV_HOST_SIDECAR",
    dev_override_env: "DV_HOST_PATH",
    marker_filename: MARKER_FILENAME,
    component: "dv-host",
    layout: InstallLayout::HashDir {
        subdir: "host",
        binary_filename: BINARY_FILENAME,
    },
};

/// The native `dv` CLI (crate `dv-cli`, S8b): installed to a STABLE path so
/// it can sit on `PATH` and be invoked as plain `dv` from inside the distro
/// (docs/phase-8-lsp-and-polish.md § Distribution & first-run).
pub const CLI_SPEC: ManagedSpec = ManagedSpec {
    sidecar_filename: CLI_SIDECAR_FILENAME,
    sidecar_env: "DV_CLI_SIDECAR",
    dev_override_env: "DV_CLI_PATH",
    marker_filename: CLI_MARKER_FILENAME,
    component: "dv-cli",
    layout: InstallLayout::StablePath {
        bin_dir: ".local/bin",
        binary_filename: CLI_BINARY_FILENAME,
        marker_dir: ".local/share/dv/cli",
    },
};

/// The result of a successful [`ensure_installed_spec`]/
/// [`force_reinstall_spec`] call — [`HostBinary`] plus the content hash the
/// binary was verified against (S8d's consistency check compares this
/// against [`cli_install_marker`] without a second round trip).
#[derive(Debug, Clone)]
pub struct InstalledBinary {
    /// Absolute POSIX path *inside the distro* to the runnable binary.
    pub path: String,
    /// The sha256 hex digest the binary was installed/verified against.
    /// Empty for [`HostBinarySource::DevOverride`] (no hash computed at
    /// all — an arbitrary dev-supplied path, nothing to verify).
    pub hash: String,
    pub source: HostBinarySource,
}

/// Every way [`ensure_installed`]/[`force_reinstall`] can fail.
#[derive(Debug)]
pub enum InstallError {
    /// No sidecar configured: no sidecar file (`dv-host-linux-x64` /
    /// `dv-linux-x64`) next to the running `dv.exe`, and no env override
    /// either. `component` names which [`ManagedSpec`] was resolving (S8c
    /// review, P2/P3 — before the generalization this was always dv-host,
    /// so the `Display` text could hardcode it; now that [`InstallError`] is
    /// shared with [`CLI_SPEC`] it must say which one actually failed).
    ///
    /// [`super::manager::client_for`] treats this exactly like today's
    /// missing `DV_HOST_PATH`: silent, no `Failed` registry entry, so a
    /// sidecar (or env var) that appears later works on the very next call
    /// instead of being stuck behind a stale cool-down.
    NoSidecar { component: &'static str },
    /// Anything else: the sidecar file couldn't be read/hashed, a Stage-A
    /// WSL round trip failed (spawn failure, `$HOME` unresolvable, the
    /// stream-install script itself failing), ... Carries the full
    /// `anyhow::Error` chain for the one `eprintln!` the manager logs on
    /// this path.
    Io(anyhow::Error),
    /// The marker read back after a fresh install still doesn't match the
    /// sidecar's hash — something is wrong beyond a stale cache (a
    /// half-written file from a killed process, a foreign/hostile file at
    /// the target path, a filesystem that silently truncated the write, …).
    /// `component` — see [`NoSidecar`](InstallError::NoSidecar)'s doc.
    VerifyFailed {
        component: &'static str,
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::NoSidecar { component } => {
                write!(f, "no {component} sidecar configured")
            }
            InstallError::Io(err) => write!(f, "{err:#}"),
            InstallError::VerifyFailed {
                component,
                expected,
                actual,
            } => write!(
                f,
                "{component} install verification failed: expected sha256 {expected}, found {actual:?}"
            ),
        }
    }
}

impl std::error::Error for InstallError {}

/// The Windows-side sidecar file — `dv-host-linux-x64` next to the running
/// `dv.exe` (plan §5: "Sidecar next to dv.exe, resolved via
/// `current_exe()`"). `DV_HOST_SIDECAR` overrides the search path entirely —
/// a test seam, letting tests point at any local file standing in for the
/// sidecar (typically a real linux `dv-host` built inside WSL and copied out
/// via the 9P mount) without needing an actual `dv.exe`-adjacent layout.
/// `None` means "not configured"; [`ensure_installed`] maps that to
/// [`InstallError::NoSidecar`].
pub fn sidecar_path() -> Option<PathBuf> {
    sidecar_path_for(&HOST_SPEC)
}

/// [`sidecar_path`], generalized over any [`ManagedSpec`] — resolves
/// `spec.sidecar_env` (verbatim override) then falls back to
/// `spec.sidecar_filename` next to the running `dv.exe`.
fn sidecar_path_for(spec: &ManagedSpec) -> Option<PathBuf> {
    if let Ok(over) = std::env::var(spec.sidecar_env)
        && !over.is_empty()
    {
        let path = PathBuf::from(over);
        if !path.is_file() {
            // An explicitly-set override pointing nowhere is a config
            // mistake, not the ordinary "no sidecar shipped" case — say so
            // once instead of silently degrading (S3 review, P3). Once per
            // (env var, not just per process): each `ManagedSpec` has its
            // own override var, and either's NoSidecar path is retried per
            // command by design — this must not become per-command spam for
            // EITHER spec.
            log_missing_sidecar_override_once(spec, &path);
            return None;
        }
        return Some(path);
    }
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join(spec.sidecar_filename);
    candidate.is_file().then_some(candidate)
}

fn log_missing_sidecar_override_once(spec: &ManagedSpec, path: &Path) {
    static LOGGED: std::sync::Mutex<Option<std::collections::HashSet<&'static str>>> =
        std::sync::Mutex::new(None);
    let mut guard = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    let logged_vars = guard.get_or_insert_with(std::collections::HashSet::new);
    if logged_vars.insert(spec.sidecar_env) {
        eprintln!(
            "[dv] {} is set but not a file: {}",
            spec.sidecar_env,
            path.display()
        );
    }
}

/// Resolve `distro`'s runnable `dv-host` binary, installing/upgrading it
/// first if necessary. See the module doc for the overall flow and the
/// content-hash-directory deviation from plan §5. Thin wrapper over
/// [`ensure_installed_spec`] with [`HOST_SPEC`] — kept as its own function
/// (rather than inlined at call sites) so [`super::manager::client_for`]
/// stays untouched by the S8c generalization.
pub fn ensure_installed(distro: &str) -> Result<HostBinary, InstallError> {
    ensure_installed_spec(distro, &HOST_SPEC).map(InstalledBinary::into_host_binary)
}

/// Force a fresh install even if the on-disk marker currently matches:
/// delete the marker, then run the exact same install-and-verify path
/// [`ensure_installed`] uses. [`super::manager::client_for`] calls this
/// exactly once per handshake proto mismatch against a
/// [`HostBinarySource::Managed`] binary (plan §2) — never for a
/// `DevOverride` path, which has no marker to invalidate in the first
/// place. Thin wrapper over [`force_reinstall_spec`] with [`HOST_SPEC`].
pub fn force_reinstall(distro: &str) -> Result<HostBinary, InstallError> {
    force_reinstall_spec(distro, &HOST_SPEC).map(InstalledBinary::into_host_binary)
}

/// Resolve `distro`'s runnable binary for `spec`, installing/upgrading it
/// first if necessary — the generalized form of [`ensure_installed`] over
/// any [`ManagedSpec`]/[`InstallLayout`]. [`ensure_cli_installed`] is the
/// [`CLI_SPEC`] instantiation.
pub fn ensure_installed_spec(
    distro: &str,
    spec: &ManagedSpec,
) -> Result<InstalledBinary, InstallError> {
    install_spec(distro, spec, ForceReinstall::No)
}

/// [`force_reinstall`]'s generalized form over any [`ManagedSpec`].
pub fn force_reinstall_spec(
    distro: &str,
    spec: &ManagedSpec,
) -> Result<InstalledBinary, InstallError> {
    install_spec(distro, spec, ForceReinstall::Yes)
}

/// Resolve (installing/upgrading if necessary) `distro`'s native `dv` CLI —
/// the [`CLI_SPEC`] instantiation of [`ensure_installed_spec`].
/// `CLI_SPEC.dev_override_env` (`DV_CLI_PATH`) short-circuits everything
/// else, exactly like [`HostBinarySource::DevOverride`] does for `dv-host`.
pub fn ensure_cli_installed(distro: &str) -> Result<InstalledBinary, InstallError> {
    ensure_installed_spec(distro, &CLI_SPEC)
}

/// Read [`CLI_SPEC`]'s installed marker inside `distro` WITHOUT streaming a
/// binary — S8d's fast per-launch consistency check compares this against
/// the sidecar's LOCAL hash to decide "drift" without paying for a
/// reinstall (or even a hash of the remote binary) on every launch.
/// `Ok(None)` means "not installed" as far as this check can tell: either no
/// marker file exists yet (never installed, or an install that failed
/// before the marker write landed), OR the marker matches but
/// `~/.local/bin/dv` itself is missing — an out-of-band deletion of just the
/// binary (dotfile-manager resync, PATH cleanup, a non-persisted bin mount)
/// that would otherwise read as falsely "installed" (S8c review, P2). Per
/// [`InstallLayout::StablePath`]'s doc, a present `~/.local/bin/dv` with a
/// missing marker — or a present marker with a missing binary — is drift,
/// never assumed-good; this fn folds the binary-presence check in so the
/// caller only has to compare the returned hash, no separate stat needed.
pub fn cli_install_marker(distro: &str) -> Result<Option<String>, InstallError> {
    let InstallLayout::StablePath {
        bin_dir,
        binary_filename,
        marker_dir,
    } = &CLI_SPEC.layout
    else {
        unreachable!("CLI_SPEC always uses InstallLayout::StablePath")
    };
    let builder = spawn_only_builder(distro);
    let (bin_dir, marker_dir) = stable_dirs(&builder, bin_dir, marker_dir, CLI_SPEC.component)?;
    let (marker, binary_present) = read_marker_and_binary_presence(
        &builder,
        &bin_dir,
        binary_filename,
        &marker_dir,
        CLI_SPEC.marker_filename,
    )?;
    if !binary_present {
        return Ok(None);
    }
    Ok((!marker.is_empty()).then_some(marker))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ForceReinstall {
    Yes,
    No,
}

/// The shared install pipeline behind [`ensure_installed_spec`]/
/// [`force_reinstall_spec`]: resolve the sidecar, resolve `spec`'s
/// destination path(s) for its [`InstallLayout`], compare the marker
/// against the sidecar's hash (or unconditionally delete it first when
/// `force` requests a reinstall), and stream+verify on a mismatch.
fn install_spec(
    distro: &str,
    spec: &ManagedSpec,
    force: ForceReinstall,
) -> Result<InstalledBinary, InstallError> {
    if let Some(path) = dev_override_path_for(spec) {
        return Ok(InstalledBinary {
            path,
            hash: String::new(),
            source: HostBinarySource::DevOverride,
        });
    }

    let (bytes, hash) = locate_and_hash_sidecar_for(spec)?;
    let builder = spawn_only_builder(distro);

    let (bin_dir, marker_dir, binary_filename) = match &spec.layout {
        InstallLayout::HashDir {
            subdir,
            binary_filename,
        } => {
            let dir = resolve_dir_generic(&builder, subdir, &hash, spec.component)?;
            (dir.clone(), dir, *binary_filename)
        }
        InstallLayout::StablePath {
            bin_dir,
            binary_filename,
            marker_dir,
        } => {
            let (bin_dir, marker_dir) = stable_dirs(&builder, bin_dir, marker_dir, spec.component)?;
            (bin_dir, marker_dir, *binary_filename)
        }
    };

    if force == ForceReinstall::Yes {
        delete_marker_at(&builder, &marker_dir, spec.marker_filename)?;
    } else {
        // For `StablePath` the marker alone is NOT a trustworthy "already
        // installed" signal: bin_dir and marker_dir are separate
        // directories, so a marker can survive an out-of-band deletion of
        // just the binary (dotfile-manager resync, PATH cleanup, a
        // non-persisted bin mount). Fold a binary-presence check into the
        // same round trip so that state is reported as drift too, never
        // assumed-good (S8c review, P2 — mirrors the missing/mismatched-
        // marker handling `is_drift` already does). `HashDir` doesn't need
        // this: binary and marker are colocated, so a marker match implies
        // the binary is right there in the same directory, and this branch
        // is kept byte-identical to the pre-generalization behavior so the
        // exact-string regression tests stay meaningful.
        let up_to_date = match &spec.layout {
            InstallLayout::StablePath { .. } => {
                let (marker, binary_present) = read_marker_and_binary_presence(
                    &builder,
                    &bin_dir,
                    binary_filename,
                    &marker_dir,
                    spec.marker_filename,
                )?;
                binary_present && !is_drift(&marker, &hash)
            }
            InstallLayout::HashDir { .. } => {
                let marker = read_marker_at(&builder, &marker_dir, spec.marker_filename)?;
                !is_drift(&marker, &hash)
            }
        };
        if up_to_date {
            return Ok(InstalledBinary {
                path: format!("{bin_dir}/{binary_filename}"),
                hash,
                source: HostBinarySource::Managed,
            });
        }
    }

    let script = if bin_dir == marker_dir {
        install_script_generic(&bin_dir, &hash, binary_filename, spec.marker_filename)
    } else {
        install_script_stable(
            &bin_dir,
            &marker_dir,
            &hash,
            binary_filename,
            spec.marker_filename,
        )
    };
    run_install_script(&builder, &script, &bytes)?;
    verify_marker(
        &builder,
        &marker_dir,
        spec.marker_filename,
        &hash,
        spec.component,
    )?;

    Ok(InstalledBinary {
        path: format!("{bin_dir}/{binary_filename}"),
        hash,
        source: HostBinarySource::Managed,
    })
}

/// A present binary can never be trusted on its own — the marker is the
/// sole drift signal (loudest for [`InstallLayout::StablePath`], which has
/// no hash-named directory to fall back on): a missing marker (`marker` is
/// empty — [`read_marker_at`]'s "doesn't exist yet" reading) or one that no
/// longer matches the sidecar's current hash both count as drift and must
/// trigger a re-provision, never be assumed-good.
fn is_drift(marker: &str, hash: &str) -> bool {
    marker != hash
}

/// Test-only alias for `dev_override_path_for(&HOST_SPEC)` — kept
/// (`#[cfg(test)]`, unused outside the regression suite below) purely so the
/// pre-generalization exact-string tests keep calling it by its original
/// zero-arg name; production code always goes through
/// [`dev_override_path_for`] with an explicit spec.
#[cfg(test)]
fn dev_override_path() -> Option<String> {
    dev_override_path_for(&HOST_SPEC)
}

fn dev_override_path_for(spec: &ManagedSpec) -> Option<String> {
    std::env::var(spec.dev_override_env)
        .ok()
        .filter(|v| !v.is_empty())
}

fn spawn_only_builder(distro: &str) -> CommandBuilder {
    CommandBuilder::new_spawn_only(RepoLocation::Wsl {
        distro: distro.to_string(),
        path: "/".to_string(),
    })
}

fn locate_and_hash_sidecar_for(spec: &ManagedSpec) -> Result<(Vec<u8>, String), InstallError> {
    let sidecar = sidecar_path_for(spec).ok_or(InstallError::NoSidecar {
        component: spec.component,
    })?;
    let bytes = std::fs::read(&sidecar)
        .map_err(|err| InstallError::Io(anyhow!("reading sidecar {}: {err}", sidecar.display())))?;
    let hash = hash_hex(&bytes);
    Ok((bytes, hash))
}

fn hash_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `$HOME`, resolved by asking the remote shell directly (one Stage-A round
/// trip: `printf %s "$HOME"`) rather than embedding an unexpanded `$HOME` in
/// every later script: the paths this module ultimately returns are handed
/// to `HostClient::spawn_wsl`'s `--exec`, which runs with **no shell** and
/// would never expand it. Once resolved, every subsequent script embeds the
/// fully-resolved directory, single-quote-escaped, the same way
/// `review::io::StoreIo::write_atomic`'s WSL branch already handles
/// arbitrary absolute paths.
fn resolve_home(builder: &CommandBuilder, component: &'static str) -> Result<String, InstallError> {
    let home = builder
        .run_text_timeout(
            "sh",
            &["-c", home_resolve_script()],
            INSTALL_COMMAND_TIMEOUT,
        )
        .map_err(InstallError::Io)?;
    let home = home.trim();
    if home.is_empty() {
        return Err(InstallError::Io(anyhow!(empty_home_message(component))));
    }
    Ok(home.trim_end_matches('/').to_string())
}

/// [`resolve_home`]'s empty-`$HOME` error text, component-labeled so a
/// `dv-cli` provisioning failure never reads identically to a `dv-host` one
/// (S8c review, P3) — split out as a pure fn so the exact text is
/// unit-testable without a live builder, matching this module's existing
/// script/message-text test pattern.
fn empty_home_message(component: &str) -> String {
    format!("{component} install: \"$HOME\" resolved empty inside the distro")
}

/// The install directory for a given sidecar hash under an
/// [`InstallLayout::HashDir`]: `<root>/<hash-prefix>`, where `<root>` is
/// `~/.local/share/dv/<subdir>` by default or [`INSTALL_ROOT_ENV`] verbatim
/// when set (test seam: an absolute path to a throwaway root instead of the
/// real user directory, so tests never touch it), and `<hash-prefix>` is the
/// first 16 hex characters of `hash` (see the module doc's content-hash-
/// directory deviation note). `component` is only used to label a
/// [`resolve_home`] failure (e.g. an empty `$HOME`) with the [`ManagedSpec`]
/// that was resolving, so a `dv-cli` provisioning failure never reads as a
/// `dv-host` one or vice versa (S8c review, P3).
fn resolve_dir_generic(
    builder: &CommandBuilder,
    subdir: &str,
    hash: &str,
    component: &'static str,
) -> Result<String, InstallError> {
    let root = if let Ok(root) = std::env::var(INSTALL_ROOT_ENV)
        && !root.is_empty()
    {
        root
    } else {
        format!(
            "{}/.local/share/dv/{subdir}",
            resolve_home(builder, component)?
        )
    };
    Ok(format!("{}/{}", root.trim_end_matches('/'), &hash[..16]))
}

/// Resolve [`InstallLayout::StablePath`]'s two directories from a live
/// `$HOME` lookup, then delegate to [`stable_dirs_from_home`] for the pure
/// arithmetic (kept separate so it's unit-testable without a live builder).
/// `component` — see [`resolve_dir_generic`]'s doc.
fn stable_dirs(
    builder: &CommandBuilder,
    bin_dir: &str,
    marker_dir: &str,
    component: &'static str,
) -> Result<(String, String), InstallError> {
    let home = resolve_home(builder, component)?;
    Ok(stable_dirs_from_home(&home, bin_dir, marker_dir))
}

/// `bin_dir` is ALWAYS `$HOME`-relative and NEVER redirected by
/// [`INSTALL_ROOT_ENV`] — see [`InstallLayout::StablePath`]'s doc for why
/// (it must be the real, on-`PATH` location). `marker_dir` DOES honor the
/// override, matching [`resolve_dir_generic`]'s `HashDir` behavior, so tests
/// can point the drift signal at a throwaway location without disturbing a
/// real prior install's marker.
fn stable_dirs_from_home(home: &str, bin_dir: &str, marker_dir: &str) -> (String, String) {
    let home = home.trim_end_matches('/');
    let bin_dir_resolved = format!("{home}/{bin_dir}");
    let marker_dir_resolved = if let Ok(root) = std::env::var(INSTALL_ROOT_ENV)
        && !root.is_empty()
    {
        root
    } else {
        format!("{home}/{marker_dir}")
    };
    (bin_dir_resolved, marker_dir_resolved)
}

/// `cat <dir>/<marker_filename>`, tolerating "doesn't exist yet" as an empty
/// string rather than an error — `2>/dev/null || true` keeps the shell's own
/// exit code always 0, so there's no need for `is_missing_path_error`-style
/// stderr sniffing here.
fn read_marker_at(
    builder: &CommandBuilder,
    dir: &str,
    marker_filename: &str,
) -> Result<String, InstallError> {
    let script = marker_read_script_generic(dir, marker_filename);
    let out = builder
        .run_text_timeout("sh", &["-c", &script], INSTALL_COMMAND_TIMEOUT)
        .map_err(InstallError::Io)?;
    Ok(out.trim().to_string())
}

/// [`InstallLayout::StablePath`]'s combined "is this actually installed"
/// probe: reads the marker AND stats the binary in one round trip, so a
/// marker that still matches after the binary alone was removed
/// out-of-band is never mistaken for "installed" (S8c review, P2 — see the
/// call site in [`install_spec`] and [`cli_install_marker`]).
fn read_marker_and_binary_presence(
    builder: &CommandBuilder,
    bin_dir: &str,
    binary_filename: &str,
    marker_dir: &str,
    marker_filename: &str,
) -> Result<(String, bool), InstallError> {
    let script =
        marker_and_binary_probe_script(bin_dir, binary_filename, marker_dir, marker_filename);
    let out = builder
        .run_text_timeout("sh", &["-c", &script], INSTALL_COMMAND_TIMEOUT)
        .map_err(InstallError::Io)?;
    Ok(parse_marker_and_binary_probe(&out))
}

/// Script for [`read_marker_and_binary_presence`]: `cat`s the marker
/// (tolerating "doesn't exist yet" the same way [`marker_read_script_generic`]
/// does) then, on its own line, `1`/`0` for whether `binary_filename` is
/// present and executable at `bin_dir`. Marker content is a lowercase hex
/// sha256 digest (or empty) — never contains a newline — so splitting the
/// two-line output back apart in [`parse_marker_and_binary_probe`] is
/// unambiguous.
fn marker_and_binary_probe_script(
    bin_dir: &str,
    binary_filename: &str,
    marker_dir: &str,
    marker_filename: &str,
) -> String {
    let bd = sh_escape(bin_dir);
    let md = sh_escape(marker_dir);
    format!(
        "m=$(cat '{md}/{marker_filename}' 2>/dev/null || true); [ -x '{bd}/{binary_filename}' ] && b=1 || b=0; printf '%s\\n%s' \"$m\" \"$b\""
    )
}

/// Pure counterpart to [`marker_and_binary_probe_script`]: splits the
/// two-line `<marker>\n<0|1>` output back into `(marker, binary_present)`.
/// Kept separate from [`read_marker_and_binary_presence`] so the parsing
/// logic is unit-testable without a live builder.
fn parse_marker_and_binary_probe(output: &str) -> (String, bool) {
    let mut lines = output.splitn(2, '\n');
    let marker = lines.next().unwrap_or("").trim().to_string();
    let present = lines.next().unwrap_or("").trim() == "1";
    (marker, present)
}

fn delete_marker_at(
    builder: &CommandBuilder,
    dir: &str,
    marker_filename: &str,
) -> Result<(), InstallError> {
    let script = marker_delete_script_generic(dir, marker_filename);
    builder
        .run_timeout("sh", &["-c", &script], INSTALL_COMMAND_TIMEOUT)
        .map_err(InstallError::Io)?;
    Ok(())
}

fn run_install_script(
    builder: &CommandBuilder,
    script: &str,
    bytes: &[u8],
) -> Result<(), InstallError> {
    builder
        .run_with_stdin_timeout("sh", &["-c", script], bytes, INSTALL_COMMAND_TIMEOUT)
        .map_err(InstallError::Io)?;
    Ok(())
}

fn verify_marker(
    builder: &CommandBuilder,
    dir: &str,
    marker_filename: &str,
    hash: &str,
    component: &'static str,
) -> Result<(), InstallError> {
    let verify = read_marker_at(builder, dir, marker_filename)?;
    if verify != hash {
        return Err(InstallError::VerifyFailed {
            component,
            expected: hash.to_string(),
            actual: verify,
        });
    }
    Ok(())
}

fn home_resolve_script() -> &'static str {
    "printf %s \"$HOME\""
}

fn marker_read_script_generic(dir: &str, marker_filename: &str) -> String {
    format!(
        "cat '{}/{marker_filename}' 2>/dev/null || true",
        sh_escape(dir)
    )
}

/// Test-only alias for `marker_read_script_generic(dir, MARKER_FILENAME)` —
/// see [`dev_override_path`]'s doc for why these zero-generic-arg names are
/// kept `#[cfg(test)]`-only.
#[cfg(test)]
fn marker_read_script(dir: &str) -> String {
    marker_read_script_generic(dir, MARKER_FILENAME)
}

fn marker_delete_script_generic(dir: &str, marker_filename: &str) -> String {
    format!("rm -f '{}/{marker_filename}'", sh_escape(dir))
}

/// Test-only alias — see [`dev_override_path`]'s doc.
#[cfg(test)]
fn marker_delete_script(dir: &str) -> String {
    marker_delete_script_generic(dir, MARKER_FILENAME)
}

/// The stream-install script for an [`InstallLayout::HashDir`] (binary and
/// marker colocated in one directory): create the directory, pipe the
/// sidecar bytes to a temp file via stdin, verify the received bytes hash to
/// the expected digest, mark it executable, rename into place, then write
/// the marker. Same mkdir + tmp-file + rename shape as
/// [`crate::review::io::StoreIo::write_atomic`]'s WSL branch, with two
/// hardenings the review demanded (Phase-5 S3 review, P1):
/// - the tmp name is suffixed with THIS process's pid — two dv processes
///   installing concurrently otherwise share one tmp inode and `cat >`'s
///   O_TRUNC tears the bytes mid-write;
/// - the in-script `sha256sum` gate before `mv` — a dv killed mid-stream
///   closes the pipe, which `cat` treats as clean EOF, and without the gate
///   the chain completes into a TORN binary behind a matching marker that
///   the marker-only mismatch check can never detect.
///
/// `hash` is a lowercase hex sha256 digest (fixed charset, no shell
/// metacharacters) so it's embedded as-is inside its own single quotes
/// without needing [`sh_escape`].
fn install_script_generic(
    dir: &str,
    hash: &str,
    binary_filename: &str,
    marker_filename: &str,
) -> String {
    let d = sh_escape(dir);
    let tmp = format!("{binary_filename}.tmp.{}", std::process::id());
    format!(
        "mkdir -p '{d}' && cat > '{d}/{tmp}' && [ \"$(sha256sum < '{d}/{tmp}' | cut -d' ' -f1)\" = '{hash}' ] && chmod +x '{d}/{tmp}' && mv '{d}/{tmp}' '{d}/{binary_filename}' && printf %s '{hash}' > '{d}/{marker_filename}'"
    )
}

/// Test-only alias — see [`dev_override_path`]'s doc.
#[cfg(test)]
fn install_script(dir: &str, hash: &str) -> String {
    install_script_generic(dir, hash, BINARY_FILENAME, MARKER_FILENAME)
}

/// [`InstallLayout::StablePath`]'s install script — same shape as
/// [`install_script_generic`] (pid-suffixed tmp, hash gate before `mv`,
/// atomic rename, marker write) but `mkdir -p`s and writes into TWO
/// directories: `bin_dir` (kept stable, on `PATH`) and a SEPARATE
/// `marker_dir` (the sole drift signal — see [`InstallLayout::StablePath`]'s
/// doc).
fn install_script_stable(
    bin_dir: &str,
    marker_dir: &str,
    hash: &str,
    binary_filename: &str,
    marker_filename: &str,
) -> String {
    let bd = sh_escape(bin_dir);
    let md = sh_escape(marker_dir);
    let tmp = format!("{binary_filename}.tmp.{}", std::process::id());
    format!(
        "mkdir -p '{bd}' '{md}' && cat > '{bd}/{tmp}' && [ \"$(sha256sum < '{bd}/{tmp}' | cut -d' ' -f1)\" = '{hash}' ] && chmod +x '{bd}/{tmp}' && mv '{bd}/{tmp}' '{bd}/{binary_filename}' && printf %s '{hash}' > '{md}/{marker_filename}'"
    )
}

/// Single-quote-escape `s` for interpolation inside a `sh -c '...'`
/// argument — identical to (and kept in sync by hand with, rather than
/// exposed cross-module from) [`crate::review::io`]'s private helper of the
/// same name and behavior: `'` → `'\''` (close the quote, an escaped
/// literal quote, reopen the quote).
fn sh_escape(s: &str) -> String {
    s.replace('\'', r"'\''")
}

impl InstalledBinary {
    /// Drop the [`InstalledBinary`]-only `hash` field to get the
    /// [`HostBinary`] shape [`ensure_installed`]/[`force_reinstall`] (and
    /// thus [`super::manager::client_for`]) still expect.
    fn into_host_binary(self) -> HostBinary {
        HostBinary {
            path: self.path,
            source: self.source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- sh_escape / script builders: pure string assertions, no WSL ------

    #[test]
    fn sh_escape_handles_single_quotes() {
        assert_eq!(sh_escape("plain"), "plain");
        assert_eq!(sh_escape("it's"), r"it'\''s");
    }

    #[test]
    fn marker_read_script_exact_text() {
        assert_eq!(
            marker_read_script("/home/kyle/.local/share/dv/host/abc123"),
            "cat '/home/kyle/.local/share/dv/host/abc123/dv-host.sha256' 2>/dev/null || true"
        );
    }

    #[test]
    fn marker_read_script_escapes_single_quotes_in_the_dir() {
        assert_eq!(
            marker_read_script("/tmp/dv-test's dir/abc123"),
            r"cat '/tmp/dv-test'\''s dir/abc123/dv-host.sha256' 2>/dev/null || true"
        );
    }

    #[test]
    fn marker_delete_script_exact_text() {
        assert_eq!(
            marker_delete_script("/home/kyle/.local/share/dv/host/abc123"),
            "rm -f '/home/kyle/.local/share/dv/host/abc123/dv-host.sha256'"
        );
    }

    #[test]
    fn install_script_exact_text() {
        let hash = "deadbeef00000000000000000000000000000000000000000000000000000000";
        let script = install_script("/home/kyle/.local/share/dv/host/abc123", hash);
        let d = "/home/kyle/.local/share/dv/host/abc123";
        let tmp = format!("dv-host.tmp.{}", std::process::id());
        assert_eq!(
            script,
            format!(
                "mkdir -p '{d}' && cat > '{d}/{tmp}' && \
[ \"$(sha256sum < '{d}/{tmp}' | cut -d' ' -f1)\" = '{hash}' ] && \
chmod +x '{d}/{tmp}' && mv '{d}/{tmp}' '{d}/dv-host' && \
printf %s '{hash}' > '{d}/dv-host.sha256'"
            )
        );
    }

    #[test]
    fn install_script_tmp_name_is_pid_unique_and_hash_gated() {
        let script = install_script("/x", "aa");
        // Concurrent installers must not share a tmp inode (S3 review P1).
        assert!(script.contains(&format!("dv-host.tmp.{}", std::process::id())));
        // A truncated stream (dv killed mid-install; cat sees EOF) must fail
        // the chain before mv, not install a torn binary behind a good marker.
        let gate = script.find("sha256sum").expect("hash gate present");
        let mv = script.find(" mv ").expect("mv present");
        assert!(gate < mv, "hash gate must run before mv");
    }

    #[test]
    fn install_script_escapes_single_quotes_in_the_dir() {
        let script = install_script("/tmp/o'brien/abc123", "aa");
        assert!(script.contains(r"mkdir -p '/tmp/o'\''brien/abc123'"));
        assert!(script.contains(r"cat > '/tmp/o'\''brien/abc123/dv-host.tmp."));
    }

    #[test]
    fn home_resolve_script_is_the_documented_one_liner() {
        assert_eq!(home_resolve_script(), "printf %s \"$HOME\"");
    }

    // --- hash_hex: sha256 hex formatting -----------------------------------

    #[test]
    fn hash_hex_is_64_lowercase_hex_chars() {
        let hash = hash_hex(b"hello world");
        assert_eq!(hash.len(), 64);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        // Known sha256("hello world") (`printf '%s' "hello world" |
        // sha256sum`) — pinned so a hashing-library swap or an endianness
        // slip would fail loudly rather than just changing
        // installed-directory names silently.
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn hash_hex_is_deterministic_and_content_sensitive() {
        assert_eq!(hash_hex(b"abc"), hash_hex(b"abc"));
        assert_ne!(hash_hex(b"abc"), hash_hex(b"abd"));
    }

    // --- resolve_dir: pure path arithmetic once a root is known ------------

    #[test]
    fn resolve_dir_uses_first_16_hex_chars_of_the_hash() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd";
        let dir = format!("{}/{}", "/home/kyle/.local/share/dv/host", &hash[..16]);
        assert_eq!(dir, "/home/kyle/.local/share/dv/host/0123456789abcdef");
    }

    // --- sidecar_path: resolution order (env override > exe-adjacent) -----
    //
    // Guards process-global env mutation the same way
    // `github::client`'s `resolve_gh_path_honors_dv_gh_env_var` test does —
    // `std::env::set_var` races under the parallel test harness otherwise.
    static ENV_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn sidecar_path_missing_is_none() {
        let _guard = test_env_lock();
        // SAFETY: test-only env mutation, scoped to this process; guarded
        // by `test_env_lock` against concurrent access from other tests in
        // this file that touch `DV_HOST_SIDECAR`.
        unsafe {
            std::env::remove_var("DV_HOST_SIDECAR");
        }
        // No real dv.exe-adjacent sidecar exists in a `cargo test` binary's
        // directory, so absent an override this must be None.
        assert_eq!(sidecar_path(), None);
    }

    #[test]
    fn sidecar_path_env_override_wins_when_the_file_exists() {
        let _guard = test_env_lock();
        let dir = std::env::temp_dir().join(format!(
            "dv-install-test-sidecar-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&dir, b"fake sidecar bytes").expect("write fake sidecar");

        // SAFETY: test-only env mutation; guarded by test_env_lock above.
        unsafe {
            std::env::set_var("DV_HOST_SIDECAR", &dir);
        }
        assert_eq!(sidecar_path(), Some(dir.clone()));
        unsafe {
            std::env::remove_var("DV_HOST_SIDECAR");
        }
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn sidecar_path_env_override_to_a_missing_file_is_none() {
        let _guard = test_env_lock();
        let missing = std::env::temp_dir().join("dv-install-test-does-not-exist-at-all");
        let _ = std::fs::remove_file(&missing);

        // SAFETY: test-only env mutation; guarded by test_env_lock above.
        unsafe {
            std::env::set_var("DV_HOST_SIDECAR", &missing);
        }
        assert_eq!(sidecar_path(), None);
        unsafe {
            std::env::remove_var("DV_HOST_SIDECAR");
        }
    }

    // --- dev_override_path: DV_HOST_PATH empty-vs-absent handling ---------

    #[test]
    fn dev_override_path_treats_empty_as_absent() {
        let _guard = test_env_lock();
        // SAFETY: test-only env mutation; guarded by test_env_lock above.
        unsafe {
            std::env::set_var("DV_HOST_PATH", "");
        }
        assert_eq!(dev_override_path(), None);
        unsafe {
            std::env::remove_var("DV_HOST_PATH");
        }
        assert_eq!(dev_override_path(), None);
    }

    #[test]
    fn dev_override_path_passes_through_a_real_value() {
        let _guard = test_env_lock();
        // SAFETY: test-only env mutation; guarded by test_env_lock above.
        unsafe {
            std::env::set_var("DV_HOST_PATH", "/opt/dv/host/dv-host");
        }
        assert_eq!(
            dev_override_path(),
            Some("/opt/dv/host/dv-host".to_string())
        );
        unsafe {
            std::env::remove_var("DV_HOST_PATH");
        }
    }

    // --- S8c generalization: HOST_SPEC reproduces the pre-generalization ---
    // --- constants byte-for-byte (the regression proof this refactor left -
    // --- dv-host untouched, on top of every exact-string test above still -
    // --- passing unchanged). -------------------------------------------

    #[test]
    fn host_spec_reproduces_the_original_dv_host_constants() {
        assert_eq!(HOST_SPEC.sidecar_filename, "dv-host-linux-x64");
        assert_eq!(HOST_SPEC.sidecar_env, "DV_HOST_SIDECAR");
        assert_eq!(HOST_SPEC.dev_override_env, "DV_HOST_PATH");
        assert_eq!(HOST_SPEC.marker_filename, "dv-host.sha256");
        let InstallLayout::HashDir {
            subdir,
            binary_filename,
        } = HOST_SPEC.layout
        else {
            panic!("HOST_SPEC must use InstallLayout::HashDir");
        };
        assert_eq!(subdir, "host");
        assert_eq!(binary_filename, "dv-host");
    }

    // --- install_script_stable: InstallLayout::StablePath's two-dir install
    // --- script mirrors install_script_generic exactly (S8c binding (5)) --

    #[test]
    fn install_script_stable_exact_text() {
        let hash = "deadbeef00000000000000000000000000000000000000000000000000000000";
        let bd = "/home/kyle/.local/bin";
        let md = "/home/kyle/.local/share/dv/cli";
        let script = install_script_stable(bd, md, hash, "dv", "dv.sha256");
        let tmp = format!("dv.tmp.{}", std::process::id());
        assert_eq!(
            script,
            format!(
                "mkdir -p '{bd}' '{md}' && cat > '{bd}/{tmp}' && \
[ \"$(sha256sum < '{bd}/{tmp}' | cut -d' ' -f1)\" = '{hash}' ] && \
chmod +x '{bd}/{tmp}' && mv '{bd}/{tmp}' '{bd}/dv' && \
printf %s '{hash}' > '{md}/dv.sha256'"
            )
        );
    }

    #[test]
    fn install_script_stable_tmp_name_is_pid_unique_and_hash_gated() {
        let script = install_script_stable("/bin", "/marker", "aa", "dv", "dv.sha256");
        assert!(script.contains(&format!("dv.tmp.{}", std::process::id())));
        let gate = script.find("sha256sum").expect("hash gate present");
        let mv = script.find(" mv ").expect("mv present");
        assert!(gate < mv, "hash gate must run before mv");
    }

    #[test]
    fn install_script_stable_escapes_single_quotes_in_both_dirs() {
        let script = install_script_stable(
            "/tmp/o'brien/bin",
            "/tmp/o'brien/marker",
            "aa",
            "dv",
            "dv.sha256",
        );
        assert!(script.contains(r"mkdir -p '/tmp/o'\''brien/bin' '/tmp/o'\''brien/marker'"));
    }

    // --- CLI_SPEC: StablePath path resolution -------------------------------

    #[test]
    fn cli_spec_uses_a_stable_path_with_a_separate_marker_dir() {
        assert_eq!(CLI_SPEC.sidecar_filename, "dv-linux-x64");
        assert_eq!(CLI_SPEC.sidecar_env, "DV_CLI_SIDECAR");
        assert_eq!(CLI_SPEC.dev_override_env, "DV_CLI_PATH");
        assert_eq!(CLI_SPEC.marker_filename, "dv.sha256");
        let InstallLayout::StablePath {
            bin_dir,
            binary_filename,
            marker_dir,
        } = CLI_SPEC.layout
        else {
            panic!("CLI_SPEC must use InstallLayout::StablePath");
        };
        assert_eq!(bin_dir, ".local/bin");
        assert_eq!(binary_filename, "dv");
        assert_eq!(marker_dir, ".local/share/dv/cli");
    }

    #[test]
    fn cli_spec_resolves_stable_bin_and_marker_paths() {
        let _guard = test_env_lock();
        // SAFETY: test-only env mutation; guarded by test_env_lock above.
        unsafe {
            std::env::remove_var("DV_HOST_INSTALL_ROOT");
        }
        let (bin_dir, marker_dir) =
            stable_dirs_from_home("/home/kyle", ".local/bin", ".local/share/dv/cli");
        assert_eq!(bin_dir, "/home/kyle/.local/bin");
        assert_eq!(format!("{bin_dir}/dv"), "/home/kyle/.local/bin/dv");
        assert_eq!(marker_dir, "/home/kyle/.local/share/dv/cli");
        assert_eq!(
            format!("{marker_dir}/dv.sha256"),
            "/home/kyle/.local/share/dv/cli/dv.sha256"
        );
    }

    #[test]
    fn cli_spec_marker_dir_honors_install_root_override_but_bin_dir_never_does() {
        let _guard = test_env_lock();
        // SAFETY: test-only env mutation; guarded by test_env_lock above.
        unsafe {
            std::env::set_var("DV_HOST_INSTALL_ROOT", "/tmp/dv-test-throwaway");
        }
        let (bin_dir, marker_dir) =
            stable_dirs_from_home("/home/kyle", ".local/bin", ".local/share/dv/cli");
        // bin_dir is ALWAYS the real, on-PATH stable location — never
        // redirected by the test-seam override (InstallLayout::StablePath's
        // doc: that's the whole point of a stable path).
        assert_eq!(bin_dir, "/home/kyle/.local/bin");
        // marker_dir DOES honor the override, matching resolve_dir_generic's
        // HashDir behavior for DV_HOST_INSTALL_ROOT.
        assert_eq!(marker_dir, "/tmp/dv-test-throwaway");
        unsafe {
            std::env::remove_var("DV_HOST_INSTALL_ROOT");
        }
    }

    // --- is_drift: missing/mismatched marker is always drift, never --------
    // --- assumed-good (S8c binding (4)) -------------------------------------

    #[test]
    fn missing_marker_is_drift() {
        assert!(is_drift("", "deadbeefdeadbeef"));
    }

    #[test]
    fn mismatched_marker_is_drift() {
        assert!(is_drift("stalehash", "deadbeefdeadbeef"));
    }

    #[test]
    fn matching_marker_is_not_drift() {
        assert!(!is_drift("deadbeefdeadbeef", "deadbeefdeadbeef"));
    }

    // --- InstallError::Display: component-labeled, not hardcoded "dv-host" -
    // --- (S8c review, P2/P3) ------------------------------------------------

    #[test]
    fn no_sidecar_display_names_the_failing_component() {
        assert_eq!(
            InstallError::NoSidecar {
                component: "dv-host"
            }
            .to_string(),
            "no dv-host sidecar configured"
        );
        assert_eq!(
            InstallError::NoSidecar {
                component: "dv-cli"
            }
            .to_string(),
            "no dv-cli sidecar configured"
        );
    }

    #[test]
    fn verify_failed_display_names_the_failing_component() {
        let err = InstallError::VerifyFailed {
            component: "dv-cli",
            expected: "aa".to_string(),
            actual: "bb".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "dv-cli install verification failed: expected sha256 aa, found \"bb\""
        );
    }

    #[test]
    fn host_and_cli_spec_carry_distinct_component_labels() {
        assert_eq!(HOST_SPEC.component, "dv-host");
        assert_eq!(CLI_SPEC.component, "dv-cli");
    }

    #[test]
    fn empty_home_message_names_the_failing_component() {
        assert_eq!(
            empty_home_message("dv-host"),
            "dv-host install: \"$HOME\" resolved empty inside the distro"
        );
        assert_eq!(
            empty_home_message("dv-cli"),
            "dv-cli install: \"$HOME\" resolved empty inside the distro"
        );
    }

    // --- read_marker_and_binary_presence: StablePath's combined marker + ---
    // --- binary-presence probe (S8c review, P2 — a present marker with an --
    // --- absent binary must read as drift, not "installed"). ---------------

    #[test]
    fn marker_and_binary_probe_script_exact_text() {
        let script = marker_and_binary_probe_script(
            "/home/kyle/.local/bin",
            "dv",
            "/home/kyle/.local/share/dv/cli",
            "dv.sha256",
        );
        assert_eq!(
            script,
            "m=$(cat '/home/kyle/.local/share/dv/cli/dv.sha256' 2>/dev/null || true); \
[ -x '/home/kyle/.local/bin/dv' ] && b=1 || b=0; printf '%s\\n%s' \"$m\" \"$b\""
        );
    }

    #[test]
    fn marker_and_binary_probe_script_escapes_single_quotes() {
        let script = marker_and_binary_probe_script(
            "/tmp/o'brien/bin",
            "dv",
            "/tmp/o'brien/marker",
            "dv.sha256",
        );
        assert!(script.contains(r"'/tmp/o'\''brien/marker/dv.sha256'"));
        assert!(script.contains(r"'/tmp/o'\''brien/bin/dv'"));
    }

    #[test]
    fn parse_marker_and_binary_probe_reads_both_lines() {
        assert_eq!(
            parse_marker_and_binary_probe("deadbeef\n1"),
            ("deadbeef".to_string(), true)
        );
        assert_eq!(
            parse_marker_and_binary_probe("deadbeef\n0"),
            ("deadbeef".to_string(), false)
        );
    }

    #[test]
    fn parse_marker_and_binary_probe_treats_empty_marker_as_no_marker() {
        // The marker line is empty (no marker file) but the binary line
        // still says present/absent independently — the two signals are
        // orthogonal, exactly the "matching-marker survives a binary-only
        // deletion" scenario this probe exists to catch (and its mirror,
        // "binary present but never provisioned via dv").
        assert_eq!(parse_marker_and_binary_probe("\n0"), (String::new(), false));
        assert_eq!(parse_marker_and_binary_probe("\n1"), (String::new(), true));
    }

    #[test]
    fn parse_marker_and_binary_probe_missing_second_line_is_absent() {
        // Defensive: a truncated/short read (should never happen given the
        // script always prints both lines) must not be misread as present.
        assert_eq!(
            parse_marker_and_binary_probe("deadbeef"),
            ("deadbeef".to_string(), false)
        );
    }
}
