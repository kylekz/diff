//! Filesystem/WSL I/O for the review store. Every path passed in is
//! relative to the repository's real `.git` directory — reviews live at
//! `.git/dv/reviews/<id>.json`, deliberately outside the worktree so they
//! never show up in `git status` or a diff.
//!
//! Mirrors [`CommandBuilder`]'s local/WSL split: local repos use
//! `std::fs` directly, WSL repos route every operation through
//! `CommandBuilder` with plain POSIX tools (`cat`, `sh`, `ls`, `rm`) so the
//! exact same invocations run whether `wsl.exe` is fronting them or not.
//! This file must compile unconditionally on macOS CI too — no
//! Windows-only API outside `#[cfg(windows)]` (there is none needed here).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};

use crate::command::CommandBuilder;
use crate::location::RepoLocation;

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
                match std::fs::read(&path) {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
                }
            }
            RepoLocation::Wsl { .. } => {
                let full = self.wsl_path(rel)?;
                match self.builder.run("cat", &["--", &full]) {
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
                    .run_text("ls", &["-la", "--time-style=full-iso", "--", &dir])
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
                let parent = path
                    .parent()
                    .ok_or_else(|| anyhow!("path {} has no parent directory", path.display()))?;
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
                let tmp = parent.join(tmp_name());
                std::fs::write(&tmp, bytes)
                    .with_context(|| format!("writing {}", tmp.display()))?;
                std::fs::rename(&tmp, &path)
                    .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
                Ok(())
            }
            RepoLocation::Wsl { .. } => {
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
                match std::fs::read_dir(&path) {
                    Ok(entries) => {
                        let mut names = Vec::new();
                        for entry in entries {
                            let entry = entry.with_context(|| {
                                format!("reading directory entry in {}", path.display())
                            })?;
                            names.push(entry.file_name().to_string_lossy().into_owned());
                        }
                        Ok(names)
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                    Err(err) => Err(err).with_context(|| format!("listing {}", path.display())),
                }
            }
            RepoLocation::Wsl { .. } => {
                let full = self.wsl_path(rel_dir)?;
                match self.builder.run_text("ls", &["-1", "--", &full]) {
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
                match std::fs::remove_file(&path) {
                    Ok(()) => Ok(()),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
                }
            }
            RepoLocation::Wsl { .. } => {
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
}

fn tmp_name() -> String {
    format!(
        ".tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
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
    match builder.run_text("cat", &["--", &dot_git]) {
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
}
