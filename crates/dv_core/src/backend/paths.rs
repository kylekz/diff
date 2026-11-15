//! Validation of the relative paths [`super::Backend`] methods take (docs/PLAN.md §5.1).
//!
//! A relative path is checked before anything joins it to a root, so no caller can read or write
//! outside the worktree or the store: absolute paths, `.`/`..` and empty components, NUL and
//! backslashes are refused. Backslashes are refused on every OS, because on Windows they are
//! separators that would hide a `..`; one rule for all OSes keeps a path the local backend
//! accepts acceptable to the in-distro one, at the cost of Linux file names containing `\`
//! (which no Windows checkout can have either).

use std::path::{Path, PathBuf};

use super::BackendError;

/// Prefix of the temp files [`super::Backend::store_write_atomic`] renames into place. Store
/// paths can't name them (no component starts with `.`), and the store listing and watcher skip
/// them.
pub(crate) const STORE_TEMP_PREFIX: &str = ".dv-tmp-";

/// `rel` joined onto the worktree root, after validation.
pub(crate) fn worktree_path(root: &Path, rel: &str) -> Result<PathBuf, BackendError> {
    check_common(rel)?;
    // `:` on Windows is a drive (`C:x` is drive-relative) or an alternate data stream (`a:b`).
    if cfg!(windows) && rel.contains(':') {
        return Err(invalid(rel, "contains ':'"));
    }
    Ok(join(root, rel))
}

/// `rel` joined onto the store root, after validation. Store names are dv's own (`reviews/
/// r-<ulid>.jsonl`), so components are limited to ASCII letters, digits, `.`, `_`, `-` and
/// `%` (the escape a GitHub name is spelled with, `github/<host>/…`, PLAN §9.4), and may not
/// start with `.`. An empty `rel` is the store root itself when `allow_root` is set.
pub(crate) fn store_path(
    root: &Path,
    rel: &str,
    allow_root: bool,
) -> Result<PathBuf, BackendError> {
    if rel.is_empty() && allow_root {
        return Ok(root.to_path_buf());
    }
    check_common(rel)?;
    for component in rel.split('/') {
        if component.starts_with('.') {
            return Err(invalid(rel, "a store path component starts with '.'"));
        }
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '%');
        if !component.chars().all(plain) {
            return Err(invalid(
                rel,
                "store paths use only ASCII letters, digits, '.', '_', '-' and '%'",
            ));
        }
    }
    Ok(join(root, rel))
}

/// Checks `rel` against the rules every relative path follows, the same on every OS. The review
/// store validates the repo paths it records with this (an anchor's path, a viewed path), so a
/// log written on one OS stays readable on another (PLAN §7).
pub(crate) fn check_relative(rel: &str) -> Result<(), BackendError> {
    check_common(rel)
}

/// The rules every relative path follows.
fn check_common(rel: &str) -> Result<(), BackendError> {
    if rel.is_empty() {
        return Err(invalid(rel, "empty"));
    }
    if rel.contains('\0') {
        return Err(invalid(rel, "contains NUL"));
    }
    if rel.contains('\\') {
        return Err(invalid(rel, "contains a backslash"));
    }
    if rel.starts_with('/') {
        return Err(invalid(rel, "absolute"));
    }
    for component in rel.split('/') {
        match component {
            "" => return Err(invalid(rel, "has an empty component")),
            "." | ".." => return Err(invalid(rel, "has a '.' or '..' component")),
            _ => {}
        }
    }
    Ok(())
}

/// An absolute path on this machine, after validation: [`super::Backend::probe_path`]'s
/// argument. It must be normalised: no `.`, `..` or empty components, no NUL; on Windows a
/// drive path (`C:\x`, `C:/x`) or a UNC one (`\\server\share\x`), with no `:` past the drive
/// (an alternate data stream) and no `?` or `*` (a verbatim or device path).
pub(crate) fn absolute_path(path: &str) -> Result<PathBuf, BackendError> {
    if path.contains('\0') {
        return Err(invalid(path, "contains NUL"));
    }
    let rest = if cfg!(windows) {
        let bytes = path.as_bytes();
        let drive = bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/');
        if drive {
            &path[3..]
        } else if let Some(unc) = path.strip_prefix(r"\\").or_else(|| path.strip_prefix("//")) {
            let mut parts = unc.splitn(3, ['\\', '/']);
            match (parts.next(), parts.next()) {
                // `\\?\…` (verbatim) and `\\.\…` (a device) aren't shares.
                (Some(server), Some(share))
                    if [server, share].iter().all(|part| {
                        !part.is_empty() && *part != "." && !part.contains([':', '?', '*'])
                    }) => {}
                _ => return Err(invalid(path, "a UNC path without a share")),
            }
            parts.next().unwrap_or("")
        } else {
            return Err(invalid(path, "not an absolute path"));
        }
    } else {
        match path.strip_prefix('/') {
            Some(rest) => rest,
            None => return Err(invalid(path, "not an absolute path")),
        }
    };
    if !rest.is_empty() {
        let separators: &[char] = if cfg!(windows) { &['\\', '/'] } else { &['/'] };
        for component in rest.split(separators) {
            match component {
                "" => return Err(invalid(path, "has an empty component")),
                "." | ".." => return Err(invalid(path, "has a '.' or '..' component")),
                _ if cfg!(windows) && component.contains([':', '?', '*']) => {
                    return Err(invalid(path, "has ':', '?' or '*' in a component"));
                }
                _ => {}
            }
        }
    }
    Ok(PathBuf::from(path))
}

fn join(root: &Path, rel: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    path.extend(rel.split('/'));
    path
}

fn invalid(rel: &str, reason: &'static str) -> BackendError {
    BackendError::InvalidPath {
        path: rel.to_string(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_paths_stay_inside_the_root() {
        let root = Path::new("root");
        for ok in [
            "a.txt",
            "src/lib.rs",
            "dir with spaces/ü.txt",
            "-dash",
            ".gitignore",
        ] {
            let path = worktree_path(root, ok).unwrap();
            assert!(path.starts_with(root), "{ok}");
            assert_eq!(path.components().count(), 1 + ok.split('/').count(), "{ok}");
        }
        let mut bad = vec![
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/./b",
            ".",
            "a//b",
            "a/",
            "a\\..\\x",
            "a\0b",
        ];
        if cfg!(windows) {
            bad.extend(["C:x", "a:stream", "C:/x"]);
        }
        for rel in bad {
            assert!(
                matches!(
                    worktree_path(root, rel),
                    Err(BackendError::InvalidPath { .. })
                ),
                "{rel:?} should be refused"
            );
        }
    }

    #[test]
    fn absolute_paths_are_normalised() {
        let (ok, bad): (Vec<&str>, Vec<&str>) = if cfg!(windows) {
            (
                vec![
                    r"C:\",
                    r"C:\x\y z",
                    "D:/a/b",
                    r"\\server\share",
                    r"\\server\share\x",
                ],
                vec![
                    "",
                    "x",
                    r"C:x",
                    r"C:\a\..\b",
                    r"C:\a\.\b",
                    r"C:\a\\b",
                    r"C:\a\",
                    r"C:\a:s",
                    r"\\?\C:\x",
                    r"\\.\pipe\x",
                    r"\\server",
                    r"\\server\",
                    "/x",
                    "C:\\a\0b",
                ],
            )
        } else {
            (
                vec!["/", "/home/kyle/x y", "/a/.hidden"],
                vec!["", "x", "a/b", "/a/../b", "/a/./b", "/a//b", "/a/", "/a\0b"],
            )
        };
        for path in ok {
            assert!(absolute_path(path).is_ok(), "{path:?} should be taken");
        }
        for path in bad {
            assert!(
                matches!(absolute_path(path), Err(BackendError::InvalidPath { .. })),
                "{path:?} should be refused"
            );
        }
    }

    #[test]
    fn store_paths_are_plain_names() {
        let root = Path::new("store");
        for ok in [
            "reviews/r-01hzy.jsonl",
            "reviews/r-01hzy.state.json",
            "x",
            "github/ghe.example.com%3a8443/acme/%2egithub/pr-7/kylekz.json",
        ] {
            assert!(store_path(root, ok, false).is_ok(), "{ok}");
        }
        assert_eq!(store_path(root, "", true).unwrap(), root);
        assert!(
            STORE_TEMP_PREFIX.starts_with('.'),
            "temp files must be unaddressable and skipped by the listing and the watcher"
        );
        for rel in [
            "",
            "../x",
            "/abs",
            "reviews/../x",
            "reviews\\x",
            "reviews/.dv-tmp-1",
            ".hidden",
            "a b",
            "ü",
            "a:b",
            "a//b",
        ] {
            assert!(
                matches!(
                    store_path(root, rel, false),
                    Err(BackendError::InvalidPath { .. })
                ),
                "{rel:?} should be refused"
            );
        }
    }
}
