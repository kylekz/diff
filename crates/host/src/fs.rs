//! `fs/read`|`fs/write_atomic`|`fs/list`|`fs/remove` (plan §8 S5): replaces
//! `dv_core::review::io::StoreIo`'s WSL fallback path (`sh -c`/`cat`/`ls`/
//! `rm`, with its locale-fragile "No such file or directory"/"Is a
//! directory" stderr string-matching) with plain `std::fs` calls running
//! host-side, inside the distro.
//!
//! Every method here takes `(root, rel)` — `root` the absolute in-distro
//! repo root, `rel` a path relative to that repo's real `.git` directory —
//! and resolves `root` to the actual gitdir via
//! `dv_core::review::resolve_local_git_dir` itself (linked-worktree
//! `gitdir:` files included), the exact same helper `watch.rs`'s
//! `subscribe_store` already reuses for the same reason: this process runs
//! LOCAL to the files it's serving, so gitdir resolution is cheap, exact,
//! and locale-proof here — see `dv_core::remote::proto`'s `fs/*` section
//! doc for why the WIRE shape is `{root, rel}` rather than the plan §2
//! table's single absolute `path` (the design deviation this slice
//! documents).
//!
//! Unlike `blob.rs` (which hand-rolls its own `cat-file --batch` protocol
//! because this crate avoids depending on dv-core for wire shapes — see
//! `main.rs`'s module doc), the actual file I/O below is NOT reimplemented:
//! it calls `dv_core::review::{read_file_at, write_file_atomic_at,
//! list_dir_names, remove_file_at}` directly, the same free functions
//! `StoreIo`'s own LOCAL arm calls — one implementation of "read/write/
//! list/remove a file" shared by both the Windows-side local-repo path and
//! this host's remote-repo path, rather than two copies that could drift.

use std::path::Path;

/// `fs/read`: resolve `root`'s gitdir, then read `<gitdir>/<rel>`.
/// `Ok((false, vec![]))` when the file doesn't exist — not an error.
pub fn read(root: &str, rel: &str) -> anyhow::Result<(bool, Vec<u8>)> {
    let path = resolve(root, rel)?;
    match dv_core::review::read_file_at(&path)? {
        Some(bytes) => Ok((true, bytes)),
        None => Ok((false, Vec::new())),
    }
}

/// `fs/write_atomic`: resolve `root`'s gitdir, then atomically write
/// `bytes` to `<gitdir>/<rel>` (mkdir -p the parent, tmp file, rename).
pub fn write_atomic(root: &str, rel: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let path = resolve(root, rel)?;
    dv_core::review::write_file_atomic_at(&path, bytes)
}

/// `fs/list`: resolve `root`'s gitdir, then list file names directly
/// inside `<gitdir>/<rel_dir>` (no recursion). `[]` when the directory
/// doesn't exist.
pub fn list(root: &str, rel_dir: &str) -> anyhow::Result<Vec<String>> {
    let path = resolve(root, rel_dir)?;
    dv_core::review::list_dir_names(&path)
}

/// `fs/remove`: resolve `root`'s gitdir, then remove `<gitdir>/<rel>`.
/// Not an error if it's already gone.
pub fn remove(root: &str, rel: &str) -> anyhow::Result<()> {
    let path = resolve(root, rel)?;
    dv_core::review::remove_file_at(&path)
}

/// `fs/create_exclusive` (durable-concurrency slice, docs/backlog.md
/// review-store-locking item): resolve `root`'s gitdir, then create
/// `<gitdir>/<rel>` with `bytes` ONLY if it doesn't already exist.
/// `Ok(true)` when this call actually created it, `Ok(false)` (not an
/// error) when something was already there — mirrors
/// `dv_core::review::create_exclusive_at`'s contract exactly, since that's
/// literally what this calls.
pub fn create_exclusive(root: &str, rel: &str, bytes: &[u8]) -> anyhow::Result<bool> {
    let path = resolve(root, rel)?;
    dv_core::review::create_exclusive_at(&path, bytes)
}

/// Resolve `root` (an absolute in-distro repo root) to its real gitdir and
/// join `rel` onto it. Shared by all four ops above.
fn resolve(root: &str, rel: &str) -> anyhow::Result<std::path::PathBuf> {
    let gitdir = dv_core::review::resolve_local_git_dir(Path::new(root))?;
    Ok(gitdir.join(rel))
}
