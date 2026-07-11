//! Repository access. All operations shell out to `git` through
//! [`CommandBuilder`] — see CLAUDE.md architecture principle #1.

use anyhow::Result;

use crate::command::CommandBuilder;
use crate::location::RepoLocation;

/// What two states a diff compares. Maps 1:1 onto git invocations — see
/// `changed_files` for the exact command lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSource {
    /// Working tree (incl. unstaged) vs HEAD: the pre-commit review case.
    WorkingTree,
    /// Index vs HEAD.
    Staged,
    /// `base..head`, or `base...head` (merge-base) when `merge_base` — the
    /// latter matches GitHub's PR "Files changed" view.
    Range {
        base: String,
        head: String,
        merge_base: bool,
    },
    /// A single commit vs its parent(s); works for root commits too.
    Commit(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
    /// Anything else git emits (`X`, `B`) — surfaced, not hidden.
    Unknown(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// Path of the file on the "new" side (for deletes: the old side).
    /// Repo-root-relative with forward slashes, exactly as git prints it.
    pub path: String,
    /// The "old" side path when it differs — renames and copies.
    pub old_path: Option<String>,
    pub status: ChangeStatus,
}

/// Which blob (one *side* of one file) to load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobSpec {
    /// `<rev>:<path>` — content at a committed revision.
    Rev { rev: String, path: String },
    /// `:0:<path>` — content in the index (stage 0).
    Index { path: String },
    /// Content in the working tree. Local: filesystem read. WSL: routed
    /// through the command layer (`cat`) — **never** a `\\wsl.localhost\`
    /// read (the no-9P rule, docs/phase-1-diff-viewer.md).
    Working { path: String },
}

/// An opened repository. Cheap to clone conceptually — hold it in an `Arc`.
pub struct GitRepo {
    location: RepoLocation,
    builder: CommandBuilder,
    // implementation may add fields (e.g. the cat-file batch process)
}

impl GitRepo {
    /// Open and validate: resolves the repo root via
    /// `git rev-parse --show-toplevel` and stores the *root* as the
    /// location (the input may point anywhere inside the work tree).
    /// A non-repo path is a descriptive `Err`, not a panic.
    pub fn open(location: RepoLocation) -> Result<Self> {
        let _ = location;
        todo!("implemented in phase 1")
    }

    pub fn location(&self) -> &RepoLocation {
        &self.location
    }

    pub fn builder(&self) -> &CommandBuilder {
        &self.builder
    }

    /// Current branch name, or short SHA when detached.
    pub fn head_label(&self) -> Result<String> {
        todo!("implemented in phase 1")
    }

    /// `git rev-parse --verify` → full SHA. Errors on unknown revs.
    pub fn resolve(&self, rev: &str) -> Result<String> {
        let _ = rev;
        todo!("implemented in phase 1")
    }

    /// Changed files for a diff source, in git's output order, with rename
    /// detection (`-M`) on.
    pub fn changed_files(&self, source: &DiffSource) -> Result<Vec<ChangedFile>> {
        let _ = source;
        todo!("implemented in phase 1")
    }

    /// Load one side's content. `Ok(None)` when the blob doesn't exist
    /// (deleted file's new side, added file's old side, missing working
    /// file). Committed/index content flows through a persistent
    /// `git cat-file --batch` child process, not one spawn per file.
    pub fn blob_bytes(&self, spec: &BlobSpec) -> Result<Option<Vec<u8>>> {
        let _ = spec;
        todo!("implemented in phase 1")
    }
}
