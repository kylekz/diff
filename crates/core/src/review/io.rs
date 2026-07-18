//! Filesystem/WSL I/O for the review store. Every path passed in is
//! relative to the repository's real `.git` directory — reviews live at
//! `.git/dv/reviews/<id>.json`, deliberately outside the worktree so they
//! never show up in `git status` or a diff.
//!
//! Mirrors [`CommandBuilder`]'s local/WSL split, with a THIRD tier S5 adds
//! on top: local repos always use `std::fs` directly (via the free
//! [`read_file_at`]/[`write_file_atomic_at`]/[`list_dir_names`]/
//! [`remove_file_at`] helpers below); WSL repos with a live, `fs`-capable
//! `dv-host` connection route through `fs/*` RPCs (see
//! [`StoreIo::try_host_fs`]), which — because the host runs INSIDE the
//! distro — call those exact same `std::fs` helpers locally there too, no
//! shell involved; only a WSL repo with NO such host connection falls back
//! to the original plain-POSIX-tool path (`cat`, `sh`, `ls`, `rm`) so the
//! exact same invocations run whether `wsl.exe` is fronting them or not.
//! This file must compile unconditionally on macOS CI too — no
//! Windows-only API outside `#[cfg(windows)]` (there is none needed here).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};

use crate::command::CommandBuilder;
use crate::location::RepoLocation;
use crate::remote::client::{HostClient, RequestFailure};
use crate::remote::manager;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) struct StoreIo {
    location: RepoLocation,
    builder: CommandBuilder,
}

impl StoreIo {
    pub(crate) fn new(location: RepoLocation) -> Self {
        let builder = CommandBuilder::new(location.clone());
        Self { location, builder }
    }

    /// Read `rel` (a path relative to `.git`). `Ok(None)` if it doesn't
    /// exist.
    pub(crate) fn read(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        match &self.location {
            RepoLocation::Local(_) => {
                let path = self.local_path(rel)?;
                read_file_at(&path)
            }
            RepoLocation::Wsl { path: root, .. } => {
                if let Some(result) = self.try_host_fs("fs", |client| client.fs_read(root, rel)) {
                    return result;
                }
                let full = self.wsl_path(rel)?;
                match self.builder.run_c_locale("cat", &["--", &full]) {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(err) if is_missing_path_error(&err) => Ok(None),
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// A cheap change digest of a directory under `.git` — file names,
    /// sizes, and mtimes — for the WSL polling watcher (one `ls -la` per
    /// poll instead of reading every review file). Absent dir digests to
    /// an empty string.
    pub(crate) fn digest_dir(&self, rel_dir: &str) -> Result<String> {
        match &self.location {
            RepoLocation::Local(_) => {
                let dir = self.local_path(rel_dir)?;
                let entries = match std::fs::read_dir(&dir) {
                    Ok(entries) => entries,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(String::new());
                    }
                    Err(err) => {
                        return Err(err).with_context(|| format!("listing {}", dir.display()));
                    }
                };
                let mut parts: Vec<String> = Vec::new();
                for entry in entries.flatten() {
                    let meta = entry.metadata();
                    let (len, mtime) = meta
                        .map(|m| {
                            let mtime = m
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_millis())
                                .unwrap_or_default();
                            (m.len(), mtime)
                        })
                        .unwrap_or_default();
                    parts.push(format!("{}:{len}:{mtime}", entry.file_name().display()));
                }
                parts.sort_unstable();
                Ok(parts.join("\n"))
            }
            RepoLocation::Wsl { .. } => {
                let dir = self.wsl_path(rel_dir)?;
                match self
                    .builder
                    .run_text_c_locale("ls", &["-la", "--time-style=full-iso", "--", &dir])
                {
                    Ok(listing) => Ok(listing),
                    Err(err) if is_missing_path_error(&err) => Ok(String::new()),
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// Write `bytes` to `rel` atomically (temp file in the same directory,
    /// then rename over the destination), creating parent directories as
    /// needed.
    pub(crate) fn write_atomic(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        match &self.location {
            RepoLocation::Local(_) => {
                let path = self.local_path(rel)?;
                write_file_atomic_at(&path, bytes)
            }
            RepoLocation::Wsl { path: root, .. } => {
                if let Some(result) =
                    self.try_host_fs("fs", |client| client.fs_write_atomic(root, rel, bytes))
                {
                    return result;
                }
                let full = self.wsl_path(rel)?;
                let dir = posix_parent(&full);
                let tmp = format!("{dir}/{}", tmp_name());
                // Every interpolated path is single-quote-escaped for `sh`:
                // repo paths (unlike branch names) can contain spaces,
                // quotes, or other shell metacharacters, and this runs
                // through a real shell (`sh -c`), unlike the argv-only
                // commands elsewhere in this file.
                let script = format!(
                    "mkdir -p '{}' && cat > '{}' && mv '{}' '{}'",
                    sh_escape(&dir),
                    sh_escape(&tmp),
                    sh_escape(&tmp),
                    sh_escape(&full),
                );
                self.builder.run_with_stdin("sh", &["-c", &script], bytes)?;
                Ok(())
            }
        }
    }

    /// File names directly inside `rel_dir` (no recursion, no path
    /// prefix). `[]` if the directory doesn't exist.
    pub(crate) fn list(&self, rel_dir: &str) -> Result<Vec<String>> {
        match &self.location {
            RepoLocation::Local(_) => {
                let path = self.local_path(rel_dir)?;
                list_dir_names(&path)
            }
            RepoLocation::Wsl { path: root, .. } => {
                if let Some(result) = self.try_host_fs("fs", |client| client.fs_list(root, rel_dir))
                {
                    return result;
                }
                let full = self.wsl_path(rel_dir)?;
                match self.builder.run_text_c_locale("ls", &["-1", "--", &full]) {
                    Ok(text) if text.is_empty() => Ok(Vec::new()),
                    Ok(text) => Ok(text.lines().map(str::to_string).collect()),
                    Err(err) if is_missing_path_error(&err) => Ok(Vec::new()),
                    Err(err) => Err(err),
                }
            }
        }
    }

    /// Remove `rel`. Not an error if it's already gone.
    pub(crate) fn remove(&self, rel: &str) -> Result<()> {
        match &self.location {
            RepoLocation::Local(_) => {
                let path = self.local_path(rel)?;
                remove_file_at(&path)
            }
            RepoLocation::Wsl { path: root, .. } => {
                if let Some(result) = self.try_host_fs("fs", |client| client.fs_remove(root, rel)) {
                    return result;
                }
                let full = self.wsl_path(rel)?;
                // `rm -f` already treats a missing target as success.
                self.builder.run("rm", &["-f", "--", &full])?;
                Ok(())
            }
        }
    }

    /// Remove `rel` ONLY IF its current content is exactly `expected` —
    /// the compare-and-remove primitive [`super::lock`]'s
    /// verify-before-remove break (and token-checked release) is built on.
    /// Best-effort like [`Self::remove`]: a no-op (not an error) when the
    /// file is missing or holds different content. The LOCAL arm is fully
    /// race-free via an atomic rename-claim
    /// ([`remove_file_if_matches_at`]); the WSL shell fallback runs the
    /// compare and remove as a SINGLE `sh -c` invocation (one-op window);
    /// only the host-RPC arm is two ops (read then remove) — the residual
    /// sliver that leaves is what `lock::acquire`'s read-back-after-create
    /// verification exists to absorb.
    pub(crate) fn remove_if_matches(&self, rel: &str, expected: &[u8]) -> Result<()> {
        match &self.location {
            RepoLocation::Local(_) => {
                let path = self.local_path(rel)?;
                remove_file_if_matches_at(&path, expected)
            }
            RepoLocation::Wsl { .. } => {
                // With a live fs-capable host, both ops run distro-side as
                // cheap RPCs; without one, a single shell invocation keeps
                // the compare-and-remove window one op wide. The host-arm
                // detection mirrors `try_host_fs`'s cap gate — a plain
                // "does a live fs-capable client exist" probe; the actual
                // read/remove below route through the ordinary methods
                // (host first, shell fallback) either way.
                let host_has_fs = self
                    .builder
                    .host_client()
                    .is_some_and(|client| client.has_cap("fs"));
                if host_has_fs {
                    if self.read(rel)?.as_deref() == Some(expected) {
                        self.remove(rel)?;
                    }
                    return Ok(());
                }
                let full = self.wsl_path(rel)?;
                // `$(cat)` reads the expected payload from stdin; command
                // substitution strips trailing newlines from BOTH sides
                // identically, and lock payloads (the only caller) carry
                // none. Exit 0 whether or not the compare matched — the
                // caller treats this as best-effort, same as `remove`.
                let script = format!(
                    "if [ \"$(cat -- '{p}' 2>/dev/null)\" = \"$(cat)\" ]; then rm -f -- '{p}'; fi",
                    p = sh_escape(&full),
                );
                self.builder
                    .run_with_stdin("sh", &["-c", &script], expected)?;
                Ok(())
            }
        }
    }

    /// Atomically create `rel` with `bytes` ONLY IF it doesn't already
    /// exist — `Ok(true)` when this call actually created it, `Ok(false)`
    /// (not an error) when something is already there. The primitive
    /// [`super::lock`] builds mutual exclusion on top of: unlike
    /// [`Self::write_atomic`] (tmp-file-then-rename, which always
    /// succeeds by *replacing* whatever's there), this is a real
    /// create-if-absent — `O_CREAT|O_EXCL` locally (`create_new`, atomic
    /// on both Windows and Linux), a POSIX `noclobber` redirect for the
    /// WSL shell fallback (equally atomic — `set -C` uses the same
    /// `O_EXCL` open under the hood), and a dedicated `fs/create_exclusive`
    /// RPC gated on the `fs_lock` capability for a live host connection —
    /// gated SEPARATELY from the general `fs` cap so an already-installed
    /// older host (which answers `fs/read`|`fs/write_atomic`|`fs/list`|
    /// `fs/remove` but predates this method) falls back to the shell arm
    /// instead of getting a `bad_request` for a method it's never heard of.
    pub(crate) fn create_exclusive(&self, rel: &str, bytes: &[u8]) -> Result<bool> {
        match &self.location {
            RepoLocation::Local(_) => {
                let path = self.local_path(rel)?;
                create_exclusive_at(&path, bytes)
            }
            RepoLocation::Wsl { path: root, .. } => {
                if let Some(result) = self.try_host_fs("fs_lock", |client| {
                    client.fs_create_exclusive(root, rel, bytes)
                }) {
                    return result;
                }
                let full = self.wsl_path(rel)?;
                let dir = posix_parent(&full);
                // `set -C` (noclobber) makes the `>` redirect fail (via
                // `O_EXCL` under the hood) if `full` already exists — the
                // shell-only equivalent of `create_new`. Content is small
                // (an owner-token lock payload — see `lock.rs`) and under
                // our control, so it's piped through stdin rather than
                // interpolated into the script, same as `write_atomic`'s
                // `cat > tmp` above.
                //
                // A failed noclobber write is NOT assumed to mean "already
                // exists" (capstone P3: read-only filesystem, permission
                // denial, and a full disk all fail the redirect too, and
                // reporting those as contention would spin the lock's
                // acquire loop until its timeout with a misleading
                // "another process is using this store" error): the script
                // re-probes with `[ -e ]` after the failure and only then
                // reports EXISTS; any other failure surfaces as `ERR:` with
                // the write's captured stderr, which
                // [`parse_create_exclusive_shell_output`] turns into a real
                // error.
                let script = format!(
                    "mkdir -p '{dir}' || {{ echo 'ERR:mkdir failed'; exit 0; }}; \
                     msg=$( {{ ( set -C; cat > '{full}' ); }} 2>&1 ); \
                     if [ $? -eq 0 ]; then echo CREATED; \
                     elif [ -e '{full}' ]; then echo EXISTS; \
                     else echo \"ERR:$msg\"; fi",
                    dir = sh_escape(&dir),
                    full = sh_escape(&full),
                );
                let out = self.builder.run_with_stdin("sh", &["-c", &script], bytes)?;
                parse_create_exclusive_shell_output(&crate::command::decode_output(&out))
            }
        }
    }

    /// Resolve `rel` to a local filesystem path under the repo's real
    /// `.git` directory, following a linked worktree's `gitdir:` pointer
    /// file when `.git` is a file rather than a directory.
    fn local_path(&self, rel: &str) -> Result<PathBuf> {
        let root = match &self.location {
            RepoLocation::Local(root) => root,
            RepoLocation::Wsl { .. } => unreachable!("local_path called for a WSL location"),
        };
        Ok(resolve_local_git_dir(root)?.join(rel))
    }

    /// Same as [`Self::local_path`], for a WSL repo: resolve to an
    /// absolute POSIX path under the real `.git` directory inside the
    /// distro.
    fn wsl_path(&self, rel: &str) -> Result<String> {
        let repo_root = match &self.location {
            RepoLocation::Wsl { path, .. } => path.as_str(),
            RepoLocation::Local(_) => unreachable!("wsl_path called for a local location"),
        };
        let git_dir = resolve_wsl_git_dir(&self.builder, repo_root)?;
        Ok(format!("{}/{rel}", git_dir.trim_end_matches('/')))
    }

    /// Attempt `op` against a live `dv-host` connection advertising `cap`
    /// for this builder's route. `None` means "no such connection exists (no
    /// host, or the host doesn't advertise `cap`, or the channel just died)"
    /// — the caller must fall back to the `sh -c`/`cat`/`ls`/`rm` arm below,
    /// exactly as if S5 had never shipped. `Some(result)` means the host
    /// actually answered (successfully or not) — that result is FINAL and
    /// must be returned as-is, never silently retried.
    ///
    /// `cap` is checked separately from a blanket `"fs"` so a NEW method
    /// added to the `fs/*` family (`fs/create_exclusive`, gated on
    /// `"fs_lock"`) can be rolled out without breaking an already-installed
    /// older host that answers the original four but has never heard of the
    /// new one — it simply doesn't advertise the new cap, and callers fall
    /// back to the shell arm exactly as if no host were connected at all.
    ///
    /// Only a [`RequestFailure::is_connection_failure`] (the channel itself
    /// is dead) triggers the fallback — mirrors
    /// [`CommandBuilder::run`]'s own contract exactly (see that method's
    /// doc comment). Re-running the SAME op via `sh -c` after a connection
    /// failure is safe here specifically because every `fs/*` op is
    /// idempotent by construction: `read`/`list` are pure reads,
    /// `write_atomic` is tmp-then-rename (redoing it just clobbers the same
    /// destination with identical bytes a second time), `remove` is already
    /// `rm -f`-equivalent (removing an already-removed file is a no-op), and
    /// `create_exclusive` redone after a lost response degrades to, at
    /// worst, this caller seeing its OWN just-created lock as "contended"
    /// and retrying the wait loop once more — never a false success. A
    /// `Timeout` or an `Rpc` error is a COMPLETED round trip and must
    /// surface as-is instead (see [`RequestFailure`]'s doc) — falling back
    /// on either of those would risk running the op concurrently with a
    /// still-in-flight original, or masking a real structured failure.
    fn try_host_fs<T>(
        &self,
        cap: &str,
        op: impl FnOnce(&Arc<HostClient>) -> anyhow::Result<T>,
    ) -> Option<Result<T>> {
        let client = self.builder.host_client()?;
        if !client.has_cap(cap) {
            return None;
        }
        match op(&client) {
            Ok(value) => Some(Ok(value)),
            Err(err) if RequestFailure::is_connection_failure(&err) => {
                // Channel dead: start the distro's cool-down and fall back
                // to `sh -c` — safe to re-run, see this method's doc.
                if let RepoLocation::Wsl { distro, .. } = &self.location {
                    manager::note_spawn_fallback(&self.location, &format!("fs op: {err:#}"));
                    manager::mark_dead(distro, &client);
                }
                None
            }
            // Rpc/Timeout: a completed round trip (or one still in flight)
            // — surface it, never silently re-run via `sh -c`.
            Err(err) => Some(Err(err)),
        }
    }
}

/// Interpret the WSL-shell `create_exclusive` script's one-line verdict:
/// `CREATED` → created, `EXISTS` → genuine contention (the path was
/// verifiably present after the failed noclobber write), `ERR:<msg>` → a
/// real failure (read-only fs, permissions, mkdir failure, ...) surfaced
/// as an error rather than mislabeled contention. Anything else is a
/// protocol violation and also an error.
fn parse_create_exclusive_shell_output(text: &str) -> Result<bool> {
    let verdict = text.trim_end();
    match verdict {
        "CREATED" => Ok(true),
        "EXISTS" => Ok(false),
        other => {
            if let Some(msg) = other.strip_prefix("ERR:") {
                Err(anyhow!("creating lock file over WSL shell failed: {msg}"))
            } else {
                Err(anyhow!(
                    "unexpected create_exclusive shell output: {other:?}"
                ))
            }
        }
    }
}

fn tmp_name() -> String {
    format!(
        ".tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Read `path`. `Ok(None)` if it doesn't exist. Free function (rather than
/// a `StoreIo` method) so both the LOCAL arm here and `dv-host`'s `fs/read`
/// handler (`crates/host/src/main.rs`) share the exact same semantics —
/// re-exported as [`crate::review::read_file_at`] for the host to reach.
pub fn read_file_at(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Create `path` with `bytes` ONLY IF it doesn't already exist —
/// `Ok(true)` when this call actually created it, `Ok(false)` (not an
/// error) when something is already there. `OpenOptions::create_new` is
/// `O_CREAT|O_EXCL` on Linux and `CREATE_NEW` on Windows — atomic on both,
/// so this is the one primitive [`super::lock`]'s mutual exclusion can be
/// built on top of (unlike [`write_file_atomic_at`], which always
/// succeeds by replacing whatever's there). Shared by the LOCAL arm here
/// and `dv-host`'s `fs/create_exclusive` handler — re-exported as
/// [`crate::review::create_exclusive_at`].
// The retry loop only ever loops on Windows (the pending-delete arm below);
// on other platforms every arm returns on the first pass, which is exactly
// the intent — silence clippy's never_loop there rather than fork the body.
#[cfg_attr(not(windows), allow(clippy::never_loop))]
pub fn create_exclusive_at(path: &Path, bytes: &[u8]) -> Result<bool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Windows-only quirk: NTFS briefly holds a just-removed file in a
    // "pending delete" state, and a concurrent `CREATE_NEW` racing that
    // window sees `ERROR_ACCESS_DENIED` (`PermissionDenied`), not
    // `AlreadyExists` — observed directly under this module's own hammer
    // test (two threads rapid-fire create/remove-cycling the exact same
    // lock path). That case is functionally "something's there"
    // (contention, `Ok(false)`) — but a GENUINE ACL denial raises the same
    // error kind and must not be masked as contention forever (capstone
    // P3: the lock's acquire loop would spin to its timeout with a
    // misleading "another process is using this store" error). The two are
    // told apart by a metadata probe + a couple of bounded retries — see
    // [`classify_permission_denied`]. `#[cfg(windows)]` because on
    // Linux/dv-host (which calls this same function — see the doc comment)
    // a real `PermissionDenied` always means a genuine ACL problem and
    // must keep surfacing as an error.
    #[cfg(windows)]
    let mut pd_retries = 0u32;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                file.write_all(bytes)
                    .with_context(|| format!("writing {}", path.display()))?;
                return Ok(true);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            #[cfg(windows)]
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                match classify_permission_denied(
                    std::fs::symlink_metadata(path)
                        .map(|_| ())
                        .map_err(|e| e.kind()),
                    pd_retries,
                ) {
                    PermissionDeniedClass::Contention => return Ok(false),
                    PermissionDeniedClass::RetryCreate => {
                        pd_retries += 1;
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    PermissionDeniedClass::GenuineDenial => {
                        return Err(err).with_context(|| {
                            format!(
                                "creating {} (persistent permission denial — not the \
                                 transient NTFS pending-delete race)",
                                path.display()
                            )
                        });
                    }
                }
            }
            Err(err) => return Err(err).with_context(|| format!("creating {}", path.display())),
        }
    }
}

/// What a `CREATE_NEW` `PermissionDenied` on Windows actually means, given
/// a follow-up metadata probe of the same path (pure and unit-testable —
/// see [`create_exclusive_at`]'s comment for the scenario):
///
/// - probe sees the file (or the probe itself is denied — a pending-delete
///   entry blocks metadata access too): something IS there → contention.
/// - probe says the path is gone: either the pending delete completed in
///   the gap (a retry of the create will now succeed) or this is a genuine
///   ACL denial where the file never existed (every retry fails the same
///   way). A couple of bounded, millisecond-spaced retries separates the
///   two: transient races clear on the first retry; a persistent denial
///   exhausts the budget and surfaces as the real error it is.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionDeniedClass {
    Contention,
    RetryCreate,
    GenuineDenial,
}

#[cfg(windows)]
const PERMISSION_DENIED_MAX_RETRIES: u32 = 3;

#[cfg(windows)]
fn classify_permission_denied(
    probe: std::result::Result<(), std::io::ErrorKind>,
    retries_so_far: u32,
) -> PermissionDeniedClass {
    match probe {
        Ok(()) => PermissionDeniedClass::Contention,
        Err(std::io::ErrorKind::PermissionDenied) => PermissionDeniedClass::Contention,
        Err(_) if retries_so_far < PERMISSION_DENIED_MAX_RETRIES => {
            PermissionDeniedClass::RetryCreate
        }
        Err(_) => PermissionDeniedClass::GenuineDenial,
    }
}

/// Write `bytes` to `path` atomically (temp file in the same directory,
/// then rename over the destination), creating parent directories as
/// needed. Shared by the LOCAL arm here and `dv-host`'s `fs/write_atomic`
/// handler — re-exported as [`crate::review::write_file_atomic_at`].
pub fn write_file_atomic_at(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path {} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = parent.join(tmp_name());
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

/// File names directly inside `path` (no recursion, no path prefix). `[]`
/// if the directory doesn't exist. Shared by the LOCAL arm here and
/// `dv-host`'s `fs/list` handler — re-exported as
/// [`crate::review::list_dir_names`].
pub fn list_dir_names(path: &Path) -> Result<Vec<String>> {
    match std::fs::read_dir(path) {
        Ok(entries) => {
            let mut names = Vec::new();
            for entry in entries {
                let entry = entry
                    .with_context(|| format!("reading directory entry in {}", path.display()))?;
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
            Ok(names)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("listing {}", path.display())),
    }
}

/// Remove `path` ONLY IF its content is exactly `expected`, atomically
/// with respect to concurrent breakers: a naive read→compare→remove pair
/// lets a second breaker's remove land AFTER the first breaker has already
/// broken and a winner has re-created the lock — deleting a LIVE lock (the
/// exact two-holder race dv-core's own two-contender lock test reproduced
/// deterministically). Instead the removal is CLAIMED first by an atomic
/// `rename` to a unique sibling name: exactly one contender's rename can
/// succeed (the loser's fails with NotFound and is a no-op), and once
/// renamed the claimant owns the file exclusively — nothing else writes to
/// the claim path — so the content check that follows is race-free.
///
/// On a content mismatch (the claimed file was NOT the expected payload —
/// only reachable if the lock changed hands in the sliver between the
/// caller's last read and this rename), the file is restored via
/// `hard_link` (atomic, fails-if-target-exists on both Windows and Linux,
/// so a successor's fresh lock is never clobbered) + claim cleanup; if the
/// filesystem refuses hard links the claim file is simply left behind as
/// inert debris (`dv/.lock.break-*` — a sibling of `dv/reviews`, so
/// invisible to the store watchers, same placement argument as the lock
/// itself). A claimant crashing mid-break leaves the same inert debris,
/// never a stuck lock.
pub fn remove_file_if_matches_at(path: &Path, expected: &[u8]) -> Result<()> {
    let claim = match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => parent.join(format!(
            "{}.break-{}-{}",
            name.display(),
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        )),
        // A bare/rootless path can't occur for the lock's real callers;
        // degrade to the (non-atomic) read-compare-remove rather than fail.
        _ => {
            if read_file_at(path)?.as_deref() == Some(expected) {
                remove_file_at(path)?;
            }
            return Ok(());
        }
    };
    if std::fs::rename(path, &claim).is_err() {
        // Missing (already released/broken) or lost the claim race —
        // either way there is nothing left that we are entitled to remove.
        return Ok(());
    }
    let content = std::fs::read(&claim).unwrap_or_default();
    if content == expected {
        let _ = std::fs::remove_file(&claim);
    } else {
        // Claimed a lock that is NOT the one the caller observed — put it
        // back without clobbering any successor (see doc comment).
        if std::fs::hard_link(&claim, path).is_ok() {
            let _ = std::fs::remove_file(&claim);
        }
    }
    Ok(())
}

/// Remove `path`. Not an error if it's already gone. Shared by the LOCAL
/// arm here and `dv-host`'s `fs/remove` handler — re-exported as
/// [`crate::review::remove_file_at`].
pub fn remove_file_at(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

/// `<root>/.git` normally; for a linked worktree (the main dv tree itself
/// uses these, for agent worktrees), `.git` is a *file* containing a
/// single `gitdir: <path>` line pointing at the real per-worktree git dir
/// (typically `<main-repo>/.git/worktrees/<name>`) — follow it.
///
/// `pub` (re-exported as [`crate::review::resolve_local_git_dir`]) rather
/// than the module-private helper it started as: `crates/host/src/watch.rs`
/// (S4) needs the exact same gitdir resolution — the host process runs
/// LOCAL to whatever repo it's serving, so this is the correct helper to
/// reuse there too rather than hand-rolling a second copy. Everything else
/// in this module (`StoreIo` itself, the WSL-side `resolve_wsl_git_dir`)
/// stays private — see the module doc for why.
pub fn resolve_local_git_dir(root: &Path) -> Result<PathBuf> {
    let dot_git = root.join(".git");
    let metadata = std::fs::symlink_metadata(&dot_git)
        .with_context(|| format!("no .git at {}", dot_git.display()))?;
    if metadata.is_dir() {
        return Ok(dot_git);
    }

    let contents = std::fs::read_to_string(&dot_git)
        .with_context(|| format!("reading gitlink file {}", dot_git.display()))?;
    let gitdir_line = contents
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("gitdir:"))
        .ok_or_else(|| {
            anyhow!(
                "{} is not a git directory or gitlink file",
                dot_git.display()
            )
        })?
        .trim();

    let linked = PathBuf::from(gitdir_line);
    if linked.is_absolute() {
        Ok(linked)
    } else {
        Ok(root.join(linked))
    }
}

/// WSL counterpart of [`resolve_local_git_dir`]. `cat`-ing a directory
/// fails with a message containing "Is a directory" on every POSIX `cat`
/// this targets (GNU coreutils, busybox) — cheaper than a separate
/// `test -d` round trip, and this path is never hit by CI (no WSL tests;
/// see the module doc), only needs to compile there.
fn resolve_wsl_git_dir(builder: &CommandBuilder, repo_root: &str) -> Result<String> {
    let dot_git = format!("{}/.git", repo_root.trim_end_matches('/'));
    match builder.run_text_c_locale("cat", &["--", &dot_git]) {
        Ok(contents) => {
            let gitdir_line = contents
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("gitdir:"))
                .ok_or_else(|| anyhow!("{dot_git} is not a git directory or gitlink file"))?
                .trim();
            if let Some(rest) = gitdir_line.strip_prefix('/') {
                Ok(format!("/{rest}"))
            } else {
                Ok(format!("{}/{gitdir_line}", repo_root.trim_end_matches('/')))
            }
        }
        Err(err) if err.to_string().contains("Is a directory") => Ok(dot_git),
        Err(err) => Err(err).with_context(|| format!("reading {dot_git}")),
    }
}

/// Whether a command failure's stderr indicates a missing file/directory
/// (vs. a real failure) — the same heuristic
/// [`crate::git::GitRepo::working_blob`]'s WSL branch uses for `cat`.
/// English-only by design: every command whose failure feeds this runs via
/// `CommandBuilder::run_c_locale`/`run_text_c_locale`, which force
/// `LC_ALL=C` on WSL children so a non-English distro locale can't
/// localize the message out from under the match.
fn is_missing_path_error(err: &anyhow::Error) -> bool {
    err.to_string().contains("No such file or directory")
}

/// Single-quote-escape `s` for interpolation inside a `sh -c '...'`
/// argument: `'` → `'\''` (close the quote, an escaped literal quote,
/// reopen the quote).
fn sh_escape(s: &str) -> String {
    s.replace('\'', r"'\''")
}

/// The parent directory of a POSIX file path (never called with a path
/// already ending in `/`).
fn posix_parent(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(idx) => path[..idx].to_string(),
        None => ".".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_escape_handles_single_quotes() {
        assert_eq!(sh_escape("plain"), "plain");
        assert_eq!(sh_escape("it's"), r"it'\''s");
        // Built programmatically rather than as a hand-counted literal —
        // two escaped quotes back to back is exactly the 4-char escape
        // sequence repeated twice.
        assert_eq!(sh_escape("''"), r"'\''".repeat(2));
    }

    #[test]
    fn sh_escape_output_is_safe_inside_single_quotes() {
        // Simulate what write_atomic builds and check the quoting actually
        // reconstitutes the original string when a POSIX shell would
        // process it: 'literal' + '\'' (closes, escaped quote, reopens) +
        // 'literal' — verify by replaying the substitution logic that a sh
        // single-quoted string undergoes.
        let evil = "a'b\"c $(echo hi) `echo hi`\\d";
        let escaped = sh_escape(evil);
        let quoted = format!("'{escaped}'");
        assert_eq!(unquote_posix_single_quotes(&quoted), evil);
    }

    /// Minimal reimplementation of how a POSIX shell parses a string built
    /// entirely from single-quoted segments and `'\''` escapes, so the
    /// escaping test above doesn't depend on having `sh` on the test
    /// machine (CI has no WSL/POSIX shell guarantee — see the module doc).
    fn unquote_posix_single_quotes(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\'' {
                // Inside a single-quoted segment: copy verbatim until the
                // closing quote.
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    out.push(c);
                }
            } else if c == '\\' && chars.peek() == Some(&'\'') {
                out.push('\'');
                chars.next();
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn posix_parent_basic() {
        assert_eq!(posix_parent("/a/b/c.json"), "/a/b");
        assert_eq!(posix_parent("/c.json"), "/");
        assert_eq!(posix_parent("c.json"), ".");
    }

    // --- read_file_at / write_file_atomic_at / list_dir_names /
    // remove_file_at (S5): the free functions shared with `dv-host`'s
    // `fs/*` handlers. Exercised directly against a real temp directory —
    // same "no tempfile crate, just std::env::temp_dir() + a unique
    // per-test name" convention `remote::install`'s tests already use.

    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dv-io-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn read_file_at_missing_is_ok_none() {
        let dir = scratch_dir("read-missing");
        assert!(read_file_at(&dir.join("nope.json")).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_then_read_round_trips_bytes() {
        let dir = scratch_dir("write-read");
        let path = dir.join("nested").join("r-1.json");
        write_file_atomic_at(&path, b"hello world").unwrap();
        let read_back = read_file_at(&path).unwrap().expect("just written");
        assert_eq!(read_back, b"hello world");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_atomic_at_creates_parent_directories() {
        let dir = scratch_dir("write-mkdir-p");
        let path = dir.join("a").join("b").join("c.json");
        assert!(!path.parent().unwrap().exists());
        write_file_atomic_at(&path, b"{}").unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_atomic_at_overwrites_existing_file() {
        let dir = scratch_dir("write-overwrite");
        let path = dir.join("r-1.json");
        write_file_atomic_at(&path, b"first").unwrap();
        write_file_atomic_at(&path, b"second").unwrap();
        assert_eq!(read_file_at(&path).unwrap().unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_exclusive_at_creates_when_absent() {
        let dir = scratch_dir("create-exclusive-fresh");
        let path = dir.join("nested").join(".lock");
        assert!(create_exclusive_at(&path, b"owner").unwrap());
        assert_eq!(read_file_at(&path).unwrap().unwrap(), b"owner");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_exclusive_at_refuses_when_present_and_leaves_original_content() {
        let dir = scratch_dir("create-exclusive-contended");
        let path = dir.join(".lock");
        assert!(create_exclusive_at(&path, b"first-owner").unwrap());
        assert!(
            !create_exclusive_at(&path, b"second-owner").unwrap(),
            "must not clobber an existing lock file"
        );
        assert_eq!(read_file_at(&path).unwrap().unwrap(), b"first-owner");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_exclusive_at_can_recreate_after_removal() {
        let dir = scratch_dir("create-exclusive-recreate");
        let path = dir.join(".lock");
        assert!(create_exclusive_at(&path, b"first").unwrap());
        remove_file_at(&path).unwrap();
        assert!(create_exclusive_at(&path, b"second").unwrap());
        assert_eq!(read_file_at(&path).unwrap().unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_if_matches_removes_only_on_exact_content_match() {
        let dir = scratch_dir("remove-if-matches");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let io = StoreIo::new(crate::location::RepoLocation::Local(dir.clone()));
        io.write_atomic("dv/.lock", b"owner-a").unwrap();

        // Wrong expected content: file must survive untouched.
        io.remove_if_matches("dv/.lock", b"owner-b").unwrap();
        assert_eq!(
            io.read("dv/.lock").unwrap().as_deref(),
            Some(&b"owner-a"[..])
        );

        // Exact match: removed.
        io.remove_if_matches("dv/.lock", b"owner-a").unwrap();
        assert!(io.read("dv/.lock").unwrap().is_none());

        // Missing file: a no-op, not an error.
        io.remove_if_matches("dv/.lock", b"owner-a").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_create_exclusive_shell_output_distinguishes_the_three_verdicts() {
        assert!(parse_create_exclusive_shell_output("CREATED\n").unwrap());
        assert!(!parse_create_exclusive_shell_output("EXISTS\n").unwrap());
        let err = parse_create_exclusive_shell_output(
            "ERR:sh: 1: cannot create /x/.lock: Read-only file system\n",
        )
        .expect_err("a non-EXISTS failure must surface as an error, not contention");
        assert!(
            err.to_string().contains("Read-only file system"),
            "the shell's captured message should ride along: {err}"
        );
        assert!(parse_create_exclusive_shell_output("garbage").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn classify_permission_denied_probe_matrix() {
        use std::io::ErrorKind;
        // Something is there (or the probe itself is blocked by the
        // pending-delete entry): contention.
        assert_eq!(
            classify_permission_denied(Ok(()), 0),
            PermissionDeniedClass::Contention
        );
        assert_eq!(
            classify_permission_denied(Err(ErrorKind::PermissionDenied), 0),
            PermissionDeniedClass::Contention
        );
        // Path gone: bounded retries first...
        assert_eq!(
            classify_permission_denied(Err(ErrorKind::NotFound), 0),
            PermissionDeniedClass::RetryCreate
        );
        assert_eq!(
            classify_permission_denied(Err(ErrorKind::NotFound), PERMISSION_DENIED_MAX_RETRIES - 1),
            PermissionDeniedClass::RetryCreate
        );
        // ...then a persistent denial surfaces as the real error it is.
        assert_eq!(
            classify_permission_denied(Err(ErrorKind::NotFound), PERMISSION_DENIED_MAX_RETRIES),
            PermissionDeniedClass::GenuineDenial
        );
    }

    #[test]
    fn list_dir_names_missing_dir_is_empty() {
        let dir = scratch_dir("list-missing");
        assert!(list_dir_names(&dir.join("nope")).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_dir_names_lists_direct_children_only() {
        let dir = scratch_dir("list-children");
        write_file_atomic_at(&dir.join("r-1.json"), b"{}").unwrap();
        write_file_atomic_at(&dir.join("r-2.json"), b"{}").unwrap();
        let mut names = list_dir_names(&dir).unwrap();
        names.sort();
        assert_eq!(names, vec!["r-1.json".to_string(), "r-2.json".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_file_at_missing_is_ok() {
        let dir = scratch_dir("remove-missing");
        assert!(remove_file_at(&dir.join("nope.json")).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_file_at_removes_an_existing_file() {
        let dir = scratch_dir("remove-existing");
        let path = dir.join("r-1.json");
        write_file_atomic_at(&path, b"{}").unwrap();
        assert!(path.exists());
        remove_file_at(&path).unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
