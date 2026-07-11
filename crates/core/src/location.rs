//! Where a repository lives: on the local filesystem or inside a WSL distro.

use std::path::PathBuf;

use anyhow::Result;

/// Location of a repository. Everything downstream (git commands, file
/// reads) routes through [`crate::CommandBuilder`] based on this, which is
/// what makes WSL support a command prefix instead of a parallel code path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RepoLocation {
    /// A path on the local (Windows/macOS) filesystem.
    Local(PathBuf),
    /// A path inside a WSL distro. `path` is an absolute POSIX path
    /// (`/home/kyle/proj`), never a UNC path.
    Wsl { distro: String, path: String },
}

impl RepoLocation {
    /// Interpret a user-supplied path argument.
    ///
    /// - `\\wsl.localhost\<distro>\<rest>` and `\\wsl$\<distro>\<rest>`
    ///   (forward-slash variants too) parse to [`RepoLocation::Wsl`] with
    ///   `<rest>` converted to an absolute POSIX path (`/` when empty).
    /// - Any other UNC path (`\\server\share`) is an error — dv does not
    ///   support generic network paths.
    /// - Everything else is [`RepoLocation::Local`] verbatim (may be
    ///   relative; [`crate::GitRepo::open`] normalizes to the repo root).
    pub fn from_path_arg(arg: &str) -> Result<Self> {
        let _ = arg;
        todo!("implemented in phase 1")
    }

    /// Interpret a `--wsl <distro>:<posix-path>` argument, e.g.
    /// `Ubuntu:/home/kyle/proj`. The path must be absolute (start with `/`).
    pub fn from_wsl_arg(arg: &str) -> Result<Self> {
        let _ = arg;
        todo!("implemented in phase 1")
    }

    /// Human-readable form for titles and lists, e.g. `D:\code\proj` or
    /// `Ubuntu:/home/kyle/proj`.
    pub fn display_name(&self) -> String {
        todo!("implemented in phase 1")
    }
}

/// List installed WSL distro names via `wsl.exe --list --quiet`.
///
/// Returns an empty list on non-Windows platforms (must still compile on
/// macOS — no unconditional Windows-only API use anywhere in this crate).
pub fn list_wsl_distros() -> Result<Vec<String>> {
    todo!("implemented in phase 1")
}
