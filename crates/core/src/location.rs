//! Where a repository lives: on the local filesystem or inside a WSL distro.

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};

use crate::command::decode_output;

/// Location of a repository. Everything downstream (git commands, file
/// reads) routes through [`crate::CommandBuilder`] based on this, which is
/// what makes WSL support a command prefix instead of a parallel code path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum RepoLocation {
    /// A path on the local (Windows/macOS) filesystem.
    Local(PathBuf),
    /// A path inside a WSL distro. `path` is an absolute POSIX path
    /// (`/home/kyle/proj`), never a UNC path.
    Wsl { distro: String, path: String },
}

/// UNC prefixes that address a WSL distro's filesystem, in both separator
/// styles. Matched case-insensitively; the byte length is ASCII-stable so
/// slicing the original (non-lowercased) input at `prefix.len()` is safe.
const WSL_UNC_PREFIXES: &[&str] = &[
    r"\\wsl.localhost\",
    r"\\wsl$\",
    "//wsl.localhost/",
    "//wsl$/",
];

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
        if let Some(rest) = strip_wsl_unc_prefix(arg) {
            let (distro, path) = split_wsl_unc_rest(rest)?;
            return Ok(RepoLocation::Wsl { distro, path });
        }
        if arg.starts_with("\\\\") || arg.starts_with("//") {
            bail!("unsupported UNC path: {arg}");
        }
        Ok(RepoLocation::Local(PathBuf::from(arg)))
    }

    /// Interpret a `--wsl <distro>:<posix-path>` argument, e.g.
    /// `Ubuntu:/home/kyle/proj`. The path must be absolute (start with `/`).
    pub fn from_wsl_arg(arg: &str) -> Result<Self> {
        let usage = "expected <distro>:/absolute/posix/path";
        let (distro, path) = arg
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid WSL location {arg:?}: {usage}"))?;
        if distro.is_empty() || !path.starts_with('/') {
            bail!("invalid WSL location {arg:?}: {usage}");
        }
        Ok(RepoLocation::Wsl {
            distro: distro.to_string(),
            path: path.to_string(),
        })
    }

    /// Human-readable form for titles and lists, e.g. `D:\code\proj` or
    /// `Ubuntu:/home/kyle/proj`.
    pub fn display_name(&self) -> String {
        match self {
            RepoLocation::Local(path) => path.display().to_string(),
            RepoLocation::Wsl { distro, path } => format!("{distro}:{path}"),
        }
    }
}

fn strip_wsl_unc_prefix(arg: &str) -> Option<&str> {
    let lower = arg.to_lowercase();
    for prefix in WSL_UNC_PREFIXES {
        if lower.starts_with(&prefix.to_lowercase()) {
            return Some(&arg[prefix.len()..]);
        }
    }
    None
}

/// Split the text after a WSL UNC prefix into `(distro, posix_path)`. The
/// first path component is the distro name; everything after it becomes an
/// absolute POSIX path.
fn split_wsl_unc_rest(rest: &str) -> Result<(String, String)> {
    let (distro, remainder) = match rest.find(['\\', '/']) {
        Some(pos) => (&rest[..pos], &rest[pos + 1..]),
        None => (rest, ""),
    };
    if distro.is_empty() {
        bail!("missing WSL distro name in path: {rest}");
    }
    Ok((distro.to_string(), normalize_posix_path(remainder)))
}

fn normalize_posix_path(remainder: &str) -> String {
    let mut normalized = String::from("/");
    for part in remainder.split(['\\', '/']) {
        if part.is_empty() {
            continue;
        }
        normalized.push_str(part);
        normalized.push('/');
    }
    if normalized.len() > 1 {
        normalized.pop();
    }
    normalized
}

/// List installed WSL distro names via `wsl.exe --list --quiet`.
///
/// Returns an empty list on non-Windows platforms (must still compile on
/// macOS — no unconditional Windows-only API use anywhere in this crate).
pub fn list_wsl_distros() -> Result<Vec<String>> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;

        let output = Command::new("wsl.exe")
            .args(["--list", "--quiet"])
            .creation_flags(crate::command::CREATE_NO_WINDOW)
            .output();
        let output = match output {
            Ok(output) if output.status.success() => output,
            _ => return Ok(Vec::new()),
        };
        let text = decode_output(&output.stdout);
        Ok(text
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u1_from_path_arg() {
        assert_eq!(
            RepoLocation::from_path_arg(r"\\wsl.localhost\Ubuntu\home\kyle\proj").unwrap(),
            RepoLocation::Wsl {
                distro: "Ubuntu".to_string(),
                path: "/home/kyle/proj".to_string(),
            }
        );
        assert_eq!(
            RepoLocation::from_path_arg(r"\\wsl$\Debian\").unwrap(),
            RepoLocation::Wsl {
                distro: "Debian".to_string(),
                path: "/".to_string(),
            }
        );
        assert_eq!(
            RepoLocation::from_path_arg("//wsl.localhost/Ubuntu/x").unwrap(),
            RepoLocation::Wsl {
                distro: "Ubuntu".to_string(),
                path: "/x".to_string(),
            }
        );
        assert_eq!(
            RepoLocation::from_path_arg(r"\\wsl.localhost\Ubuntu/a\b").unwrap(),
            RepoLocation::Wsl {
                distro: "Ubuntu".to_string(),
                path: "/a/b".to_string(),
            }
        );

        let err = RepoLocation::from_path_arg(r"\\server\share").unwrap_err();
        assert!(err.to_string().contains("unsupported UNC"));

        assert_eq!(
            RepoLocation::from_path_arg(r"D:\code\x").unwrap(),
            RepoLocation::Local(PathBuf::from(r"D:\code\x"))
        );
        assert_eq!(
            RepoLocation::from_path_arg("./rel").unwrap(),
            RepoLocation::Local(PathBuf::from("./rel"))
        );
    }

    #[test]
    fn u2_from_wsl_arg() {
        assert_eq!(
            RepoLocation::from_wsl_arg("Ubuntu:/home/x").unwrap(),
            RepoLocation::Wsl {
                distro: "Ubuntu".to_string(),
                path: "/home/x".to_string(),
            }
        );
        assert!(RepoLocation::from_wsl_arg("Ubuntu:home").is_err());
        assert!(RepoLocation::from_wsl_arg(":/x").is_err());
        assert!(RepoLocation::from_wsl_arg("Ubuntu").is_err());
    }

    #[test]
    fn display_name() {
        assert_eq!(
            RepoLocation::Local(PathBuf::from(r"D:\code\proj")).display_name(),
            r"D:\code\proj"
        );
        assert_eq!(
            RepoLocation::Wsl {
                distro: "Ubuntu".to_string(),
                path: "/home/kyle/proj".to_string(),
            }
            .display_name(),
            "Ubuntu:/home/kyle/proj"
        );
    }
}
