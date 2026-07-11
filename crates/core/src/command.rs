//! The single choke point for running external programs against a
//! [`RepoLocation`]. Local repos run the program directly; WSL repos run it
//! as `wsl.exe -d <distro> -- <program> <args…>`. Nothing in dv spawns a
//! repo-scoped process any other way.

use std::process::Command;

use anyhow::Result;

use crate::location::RepoLocation;

#[derive(Debug, Clone)]
pub struct CommandBuilder {
    location: RepoLocation,
}

impl CommandBuilder {
    pub fn new(location: RepoLocation) -> Self {
        Self { location }
    }

    pub fn location(&self) -> &RepoLocation {
        &self.location
    }

    /// Construct (but do not run) a [`Command`] for `program` with `args`,
    /// routed through `wsl.exe -d <distro> --` for WSL locations.
    ///
    /// On Windows the command must carry `CREATE_NO_WINDOW` (0x0800_0000)
    /// via `CommandExt::creation_flags`, or every git call flashes a console
    /// window once dv is a windowed (non-console) binary.
    pub fn command(&self, program: &str, args: &[&str]) -> Command {
        let _ = (program, args);
        todo!("implemented in phase 1")
    }

    /// Run to completion, capturing output. Non-zero exit is an `Err`
    /// carrying the program, args, exit code, and (decoded, truncated)
    /// stderr. Stdout is returned as raw bytes, untouched — blob content
    /// must never pass through text decoding.
    pub fn run(&self, program: &str, args: &[&str]) -> Result<Vec<u8>> {
        let _ = (program, args);
        todo!("implemented in phase 1")
    }

    /// [`Self::run`] + [`decode_output`] + trailing-whitespace trim, for
    /// text-producing commands (`rev-parse`, `--name-status`, …).
    pub fn run_text(&self, program: &str, args: &[&str]) -> Result<String> {
        let _ = (program, args);
        todo!("implemented in phase 1")
    }
}

/// Decode process output that may be UTF-8 **or** UTF-16LE.
///
/// `wsl.exe` passes child-process output through as raw bytes (UTF-8 for
/// git), but its *own* messages — errors like "no such distribution", and
/// the output of `wsl.exe --list` — are UTF-16LE, sometimes without a BOM.
/// Heuristic: FF FE BOM → UTF-16LE; else if the buffer contains NUL bytes
/// in an every-other-byte pattern → UTF-16LE; else UTF-8 (lossy).
pub fn decode_output(bytes: &[u8]) -> String {
    let _ = bytes;
    todo!("implemented in phase 1")
}
