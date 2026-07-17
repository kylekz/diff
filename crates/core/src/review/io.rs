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
                if let Some(result) = self.try_host_fs(|client| client.fs_read(root, rel)) {
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
                    self.try_host_fs(|client| client.fs_write_atomic(root, rel, bytes))
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
                if let Some(result) = self.try_host_fs(|client| client.fs_list(root, rel_dir)) {
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
                if let Some(result) = self.try_host_fs(|client| client.fs_remove(root, rel)) {
                    return result;
                }
                let full = self.wsl_path(rel)?;
                // `rm -f` already treats a missing target as success.
                self.builder.run("rm", &["-f", "--", &full])?;
                Ok(())
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

    /// Attempt `op` against a live, `fs`-capable `dv-host` connection for
    /// this builder's route. `None` means "no such connection exists (no
    /// host, or the host doesn't advertise `fs`, or the channel just died)"
    /// — the caller must fall back to the `sh -c`/`cat`/`ls`/`rm` arm below,
    /// exactly as if S5 had never shipped. `Some(result)` means the host
    /// actually answered (successfully or not) — that result is FINAL and
    /// must be returned as-is, never silently retried.
    ///
    /// Only a [`RequestFailure::is_connection_failure`] (the channel itself
    /// is dead) triggers the fallback — mirrors
    /// [`CommandBuilder::run`]'s own contract exactly (see that method's
    /// doc comment). Re-running the SAME op via `sh -c` after a connection
    /// failure is safe here specifically because every `fs/*` op is
    /// idempotent by construction: `read`/`list` are pure reads,
    /// `write_atomic` is tmp-then-rename (redoing it just clobbers the same
    /// destination with identical bytes a second time), and `remove` is
    /// already `rm -f`-equivalent (removing an already-removed file is a
    /// no-op). A `Timeout` or an `Rpc` error is a COMPLETED round trip and
    /// must surface as-is instead (see [`RequestFailure`]'s doc) — falling
    /// back on either of those would risk running the op concurrently with
    /// a still-in-flight original, or masking a real structured failure.
    fn try_host_fs<T>(
        &self,
        op: impl FnOnce(&Arc<HostClient>) -> anyhow::Result<T>,
    ) -> Option<Result<T>> {
        let client = self.builder.host_client()?;
        if !client.has_cap("fs") {
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
