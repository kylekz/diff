//! Counting the git processes the code under test starts ([`GitTrace`]): git's own trace
//! (`GIT_TRACE`), written to a file of the fixture's, so a test can say what a command costs
//! (docs/PLAN.md §9.6: placing GitHub's threads reads a file once, never once per thread).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// git's trace of every process started with [`GitTrace::env`] in its environment.
#[derive(Debug)]
pub struct GitTrace {
    path: PathBuf,
}

impl GitTrace {
    /// A trace kept in `dir`, a directory outside every repository.
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("git-trace.log"),
        }
    }

    /// The variable that makes git append to the trace, for an environment overlay beside
    /// `FixtureRepo::env_overlay`.
    pub fn env(&self) -> (OsString, Option<OsString>) {
        (
            OsString::from("GIT_TRACE"),
            Some(self.path.clone().into_os_string()),
        )
    }

    /// The git commands run since the last call, each as git names it (`git cat-file -t
    /// …`), oldest first.
    pub fn take(&self) -> Vec<String> {
        let traced = std::fs::read_to_string(&self.path).unwrap_or_default();
        // Nothing traced yet leaves no file.
        let _ = std::fs::remove_file(&self.path);
        traced
            .lines()
            .filter_map(|line| line.split_once("trace: built-in: "))
            .map(|(_, command)| command.to_string())
            .collect()
    }
}
