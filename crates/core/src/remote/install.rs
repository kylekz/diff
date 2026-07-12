//! Sidecar resolution + install/upgrade flow for `dv-host`
//! (docs/phase-5-implementation-plan.md §5, §8 S3). Consumed by
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

use std::path::PathBuf;

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

/// Every way [`ensure_installed`]/[`force_reinstall`] can fail.
#[derive(Debug)]
pub enum InstallError {
    /// No sidecar configured: no `dv-host-linux-x64` next to the running
    /// `dv.exe`, and no `DV_HOST_SIDECAR` override either.
    ///
    /// [`super::manager::client_for`] treats this exactly like today's
    /// missing `DV_HOST_PATH`: silent, no `Failed` registry entry, so a
    /// sidecar (or env var) that appears later works on the very next call
    /// instead of being stuck behind a stale cool-down.
    NoSidecar,
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
    VerifyFailed { expected: String, actual: String },
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::NoSidecar => write!(f, "no dv-host sidecar configured"),
            InstallError::Io(err) => write!(f, "{err:#}"),
            InstallError::VerifyFailed { expected, actual } => write!(
                f,
                "dv-host install verification failed: expected sha256 {expected}, found {actual:?}"
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
    if let Ok(over) = std::env::var("DV_HOST_SIDECAR")
        && !over.is_empty()
    {
        let path = PathBuf::from(over);
        if !path.is_file() {
            // An explicitly-set override pointing nowhere is a config
            // mistake, not the ordinary "no sidecar shipped" case — say so
            // once instead of silently degrading (S3 review, P3). Once per
            // process: the NoSidecar path is retried per command by design,
            // and this must not become per-command spam.
            static LOGGED: std::sync::Once = std::sync::Once::new();
            LOGGED.call_once(|| {
                eprintln!(
                    "[dv-host] DV_HOST_SIDECAR is set but not a file: {}",
                    path.display()
                );
            });
            return None;
        }
        return Some(path);
    }
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join(SIDECAR_FILENAME);
    candidate.is_file().then_some(candidate)
}

/// Resolve `distro`'s runnable `dv-host` binary, installing/upgrading it
/// first if necessary. See the module doc for the overall flow and the
/// content-hash-directory deviation from plan §5.
pub fn ensure_installed(distro: &str) -> Result<HostBinary, InstallError> {
    if let Some(path) = dev_override_path() {
        return Ok(HostBinary {
            path,
            source: HostBinarySource::DevOverride,
        });
    }

    let (bytes, hash) = locate_and_hash_sidecar()?;
    let builder = spawn_only_builder(distro);
    let dir = resolve_dir(&builder, &hash)?;

    let marker = read_marker(&builder, &dir)?;
    if marker == hash {
        return Ok(binary_at(&dir));
    }

    install_and_verify(&builder, &dir, &bytes, &hash)?;
    Ok(binary_at(&dir))
}

/// Force a fresh install even if the on-disk marker currently matches:
/// delete the marker, then run the exact same install-and-verify path
/// [`ensure_installed`] uses. [`super::manager::client_for`] calls this
/// exactly once per handshake proto mismatch against a
/// [`HostBinarySource::Managed`] binary (plan §2) — never for a
/// `DevOverride` path, which has no marker to invalidate in the first
/// place.
pub fn force_reinstall(distro: &str) -> Result<HostBinary, InstallError> {
    let (bytes, hash) = locate_and_hash_sidecar()?;
    let builder = spawn_only_builder(distro);
    let dir = resolve_dir(&builder, &hash)?;

    delete_marker(&builder, &dir)?;
    install_and_verify(&builder, &dir, &bytes, &hash)?;
    Ok(binary_at(&dir))
}

fn dev_override_path() -> Option<String> {
    std::env::var("DV_HOST_PATH").ok().filter(|v| !v.is_empty())
}

fn spawn_only_builder(distro: &str) -> CommandBuilder {
    CommandBuilder::new_spawn_only(RepoLocation::Wsl {
        distro: distro.to_string(),
        path: "/".to_string(),
    })
}

fn locate_and_hash_sidecar() -> Result<(Vec<u8>, String), InstallError> {
    let sidecar = sidecar_path().ok_or(InstallError::NoSidecar)?;
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

/// The install directory for a given sidecar hash: `<root>/<hash-prefix>`,
/// where `<root>` is [`install_root`] and `<hash-prefix>` is the first 16
/// hex characters of `hash` (see the module doc's content-hash-directory
/// deviation note).
fn resolve_dir(builder: &CommandBuilder, hash: &str) -> Result<String, InstallError> {
    let root = install_root(builder)?;
    Ok(format!("{}/{}", root.trim_end_matches('/'), &hash[..16]))
}

/// The base directory holding every installed version's own subdirectory —
/// `~/.local/share/dv/host` by default, or `DV_HOST_INSTALL_ROOT` verbatim
/// when set (test seam: an absolute path to a throwaway root instead of the
/// real user directory, so tests never touch it).
///
/// `$HOME` is resolved by asking the remote shell directly (one Stage-A
/// round trip: `printf %s "$HOME"`) rather than embedding an unexpanded
/// `$HOME` in every later script: the path this module ultimately returns
/// is handed to `HostClient::spawn_wsl`'s `--exec`, which runs with **no
/// shell** and would never expand it. Once resolved, every subsequent
/// script embeds the fully-resolved directory, single-quote-escaped, the
/// same way `review::io::StoreIo::write_atomic`'s WSL branch already
/// handles arbitrary absolute paths.
fn install_root(builder: &CommandBuilder) -> Result<String, InstallError> {
    if let Ok(root) = std::env::var("DV_HOST_INSTALL_ROOT")
        && !root.is_empty()
    {
        return Ok(root);
    }
    let home = builder
        .run_text("sh", &["-c", home_resolve_script()])
        .map_err(InstallError::Io)?;
    let home = home.trim();
    if home.is_empty() {
        return Err(InstallError::Io(anyhow!(
            "dv-host install: \"$HOME\" resolved empty inside the distro"
        )));
    }
    Ok(format!(
        "{}/.local/share/dv/host",
        home.trim_end_matches('/')
    ))
}

fn binary_at(dir: &str) -> HostBinary {
    HostBinary {
        path: format!("{dir}/{BINARY_FILENAME}"),
        source: HostBinarySource::Managed,
    }
}

/// `cat <dir>/dv-host.sha256`, tolerating "doesn't exist yet" as an empty
/// string rather than an error — `2>/dev/null || true` keeps the shell's own
/// exit code always 0, so there's no need for `is_missing_path_error`-style
/// stderr sniffing here.
fn read_marker(builder: &CommandBuilder, dir: &str) -> Result<String, InstallError> {
    let script = marker_read_script(dir);
    let out = builder
        .run_text("sh", &["-c", &script])
        .map_err(InstallError::Io)?;
    Ok(out.trim().to_string())
}

fn delete_marker(builder: &CommandBuilder, dir: &str) -> Result<(), InstallError> {
    let script = marker_delete_script(dir);
    builder
        .run("sh", &["-c", &script])
        .map_err(InstallError::Io)?;
    Ok(())
}

/// Stream `bytes` into `<dir>/dv-host` (plan §5's exact pipeline: mkdir-p +
/// stdin-to-tmp + chmod +x + rename + marker write) and then re-read the
/// marker to confirm it landed correctly.
fn install_and_verify(
    builder: &CommandBuilder,
    dir: &str,
    bytes: &[u8],
    hash: &str,
) -> Result<(), InstallError> {
    let script = install_script(dir, hash);
    builder
        .run_with_stdin("sh", &["-c", &script], bytes)
        .map_err(InstallError::Io)?;

    let verify = read_marker(builder, dir)?;
    if verify != hash {
        return Err(InstallError::VerifyFailed {
            expected: hash.to_string(),
            actual: verify,
        });
    }
    Ok(())
}

fn home_resolve_script() -> &'static str {
    "printf %s \"$HOME\""
}

fn marker_read_script(dir: &str) -> String {
    format!(
        "cat '{}/{MARKER_FILENAME}' 2>/dev/null || true",
        sh_escape(dir)
    )
}

fn marker_delete_script(dir: &str) -> String {
    format!("rm -f '{}/{MARKER_FILENAME}'", sh_escape(dir))
}

/// The stream-install script: create the version dir, pipe the sidecar
/// bytes to a temp file via stdin, verify the received bytes hash to the
/// expected digest, mark it executable, rename into place, then write the
/// marker. Same mkdir + tmp-file + rename shape as
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
fn install_script(dir: &str, hash: &str) -> String {
    let d = sh_escape(dir);
    let tmp = format!("{BINARY_FILENAME}.tmp.{}", std::process::id());
    format!(
        "mkdir -p '{d}' && cat > '{d}/{tmp}' && [ \"$(sha256sum < '{d}/{tmp}' | cut -d' ' -f1)\" = '{hash}' ] && chmod +x '{d}/{tmp}' && mv '{d}/{tmp}' '{d}/{BINARY_FILENAME}' && printf %s '{hash}' > '{d}/{MARKER_FILENAME}'"
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
}
