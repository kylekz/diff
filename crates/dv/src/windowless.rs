//! `dv` and `dv open` in the headless build (docs/PLAN.md §13.1): no window to open, so they
//! exit 2 and say where to open the repository instead. Inside a WSL distro that is the
//! Windows app, given the repository's path as Windows spells it (`$WSL_DISTRO_NAME` names
//! the distro; a Windows drive under `/mnt/<letter>` by its drive), quoted so that it pastes
//! into PowerShell and cmd (`dv_cli::output::windows_arg`, the rule every command dv suggests
//! to paste on Windows follows).

use std::path::Path;

use dv_cli::output::windows_arg;
use dv_cli::{CliError, GlobalArgs};
use dv_core::Location;
use dv_core::location::{drive_path, mount_path};

use crate::cli::OpenArgs;

/// The error `dv` or `dv open` (`open`'s arguments) exits with when run in `cwd`, inside the
/// WSL distro `distro` when that is set; `exists` tells an argument that is a path from one
/// that is a pull request ([`OpenArgs::pr`]).
pub fn refusal(
    global: &GlobalArgs,
    open: Option<&OpenArgs>,
    cwd: &Path,
    distro: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
) -> CliError {
    let distro = distro.map(str::trim).filter(|distro| !distro.is_empty());
    let Some(distro) = distro else {
        return CliError::unsupported(
            "this build of dv has no window: it runs the commands and `dv backend --stdio`; \
             open the repository with a dv that has one",
        );
    };
    let named = open
        .and_then(|open| open.path(cwd, exists))
        .or(global.repo.as_deref());
    let path = windows_path(&cwd.to_string_lossy(), named, global.wsl.as_deref(), distro);
    // A pull request opens in its repository: where that is, then the reference as given.
    let what = match open.and_then(|open| open.pr(cwd, exists)) {
        Some(reference) => format!(
            "{} --pr {}",
            windows_arg(&path),
            windows_arg(reference.trim())
        ),
        None => windows_arg(&path),
    };
    CliError::unsupported(format!(
        "this is dv's Linux build without a window; open the repository from Windows: \
         `dv open {what}`"
    ))
}

/// The repository as Windows spells it, run in `cwd` inside `distro`: `--wsl <distro>:<path>`
/// (`wsl`) when given; else what was named — a path through a distro's share (any distro's)
/// or a Windows path as it is, a POSIX path (absolute, or relative to `cwd`) in this distro;
/// else `cwd`. Something that is no location at all falls back to `cwd`, which the command
/// would read too. A POSIX path on a Windows drive mounted at WSL's default automount root
/// (`/mnt/d/code/api`) is that drive's path (`D:\code\api`, [`drive_path`]), which the app
/// opens as a local repository rather than through the distro.
fn windows_path(cwd: &str, named: Option<&str>, wsl: Option<&str>, distro: &str) -> String {
    let in_distro = |distro: &str, path: String| {
        drive_path(&path).unwrap_or_else(|| {
            Location::Wsl {
                distro: distro.to_string(),
                path,
            }
            .unc_path()
            .unwrap_or_default()
        })
    };
    let here = |path: Option<&str>| in_distro(distro, posix_path(cwd, path));
    if let Some(Ok(Location::Wsl { distro, path })) = wsl.map(Location::parse_wsl) {
        return in_distro(&distro, posix_path("/", Some(&path)));
    }
    let Some(named) = named else {
        return here(None);
    };
    match Location::parse(named) {
        Ok(location @ Location::Wsl { .. }) => location.unc_path().unwrap_or_default(),
        Ok(Location::Local(_)) if is_windows_path(named) => named.to_string(),
        Ok(Location::Local(_)) => here(Some(named)),
        Err(_) => here(None),
    }
}

/// Whether `path` is spelled for Windows: a drive's path (`D:\code`, `D:/code`, `D:`, which
/// [`mount_path`] reads) or a UNC share (`\\server\share`), which a POSIX path never starts
/// with.
fn is_windows_path(path: &str) -> bool {
    mount_path(path).is_some() || path.starts_with(r"\\")
}

/// `named` (absolute, or relative to `cwd`) or else `cwd`, as an absolute POSIX path with `.`
/// and `..` resolved lexically.
fn posix_path(cwd: &str, named: Option<&str>) -> String {
    let joined = match named {
        Some(named) if named.starts_with('/') => named.to_string(),
        Some(named) => format!("{cwd}/{named}"),
        None => cwd.to_string(),
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use dv_cli::Exit;
    use pretty_assertions::assert_eq;

    use super::*;

    fn message(
        target: Option<&str>,
        repo: Option<&str>,
        wsl: Option<&str>,
        cwd: &str,
        distro: Option<&str>,
    ) -> String {
        let global = GlobalArgs {
            repo: repo.map(str::to_string),
            wsl: wsl.map(str::to_string),
            ..GlobalArgs::default()
        };
        let open = OpenArgs {
            target: target.map(str::to_string),
            pr: None,
        };
        let error = refusal(&global, Some(&open), Path::new(cwd), distro, &|_| false);
        assert_eq!(error.exit, Exit::Usage);
        assert_eq!(error.code, "unsupported");
        error.message
    }

    #[test]
    fn inside_a_distro_it_names_the_repository_as_windows_spells_it() {
        let lead = "this is dv's Linux build without a window; open the repository from \
                    Windows: ";
        let cases = [
            (
                None,
                None,
                None,
                "/home/kyle/api",
                r"`dv open \\wsl.localhost\Ubuntu\home\kyle\api`",
            ),
            (
                Some("../web"),
                None,
                None,
                "/home/kyle/api",
                r"`dv open \\wsl.localhost\Ubuntu\home\kyle\web`",
            ),
            (
                Some("/srv/x/"),
                None,
                None,
                "/home/kyle",
                r"`dv open \\wsl.localhost\Ubuntu\srv\x`",
            ),
            (
                None,
                Some("./sub/./dir"),
                None,
                "/home/kyle",
                r"`dv open \\wsl.localhost\Ubuntu\home\kyle\sub\dir`",
            ),
            (None, None, None, "/", r"`dv open \\wsl.localhost\Ubuntu\`"),
            // A path through a distro's share is that path, whichever distro, whatever the
            // working directory.
            (
                Some(r"\\wsl.localhost\Ubuntu\tmp\repo"),
                None,
                None,
                "/tmp/repo/src",
                r"`dv open \\wsl.localhost\Ubuntu\tmp\repo`",
            ),
            (
                None,
                Some(r"\\wsl$\ubuntu\srv\x\"),
                None,
                "/home/kyle",
                r"`dv open \\wsl.localhost\ubuntu\srv\x`",
            ),
            (
                Some(r"\\wsl.localhost\Debian\srv\web"),
                None,
                None,
                "/home/kyle",
                r"`dv open \\wsl.localhost\Debian\srv\web`",
            ),
            // A Windows path is already Windows'.
            (
                Some(r"D:\code\api"),
                None,
                None,
                "/home/kyle",
                r"`dv open D:\code\api`",
            ),
            (
                None,
                Some("D:/code/api"),
                None,
                "/home/kyle",
                "`dv open D:/code/api`",
            ),
            // `--wsl` names the repository too.
            (
                None,
                None,
                Some("Ubuntu:/tmp/dv-repo"),
                "/home/kyle",
                r"`dv open \\wsl.localhost\Ubuntu\tmp\dv-repo`",
            ),
            // A Windows drive, mounted at WSL's default automount root, is that drive's path:
            // the app opens it as a local repository, not through the distro.
            (
                None,
                None,
                None,
                "/mnt/d/Software/diff",
                r"`dv open D:\Software\diff`",
            ),
            (None, None, None, "/mnt/c", r"`dv open C:\`"),
            (
                Some("../api"),
                None,
                None,
                "/mnt/d/code/web",
                r"`dv open D:\code\api`",
            ),
            (
                None,
                None,
                Some("Ubuntu:/mnt/e/x"),
                "/home/kyle",
                r"`dv open E:\x`",
            ),
            // A pull request, in the repository it is of (`#` starts a comment in
            // PowerShell, so it is quoted).
            (
                Some("#482"),
                None,
                None,
                "/home/kyle/api",
                r##"`dv open \\wsl.localhost\Ubuntu\home\kyle\api --pr "#482"`"##,
            ),
            (
                Some("https://github.com/acme/api/pull/482"),
                Some("/mnt/d/code/api"),
                None,
                "/home/kyle",
                r"`dv open D:\code\api --pr https://github.com/acme/api/pull/482`",
            ),
            // Anything else under `/mnt` is the distro's.
            (
                None,
                None,
                None,
                "/mnt/wsl/x",
                r"`dv open \\wsl.localhost\Ubuntu\mnt\wsl\x`",
            ),
            (
                None,
                None,
                None,
                "/mnt",
                r"`dv open \\wsl.localhost\Ubuntu\mnt`",
            ),
        ];
        for (target, repo, wsl, cwd, expected) in cases {
            assert_eq!(
                message(target, repo, wsl, cwd, Some("Ubuntu")),
                format!("{lead}{expected}"),
                "{target:?} {repo:?} {wsl:?} {cwd}"
            );
        }
    }

    #[test]
    fn the_path_pastes_into_powershell_and_cmd() {
        let lead = "this is dv's Linux build without a window; open the repository from \
                    Windows: ";
        for (cwd, expected) in [
            (
                "/home/kyle/my repo",
                r#"`dv open "\\wsl.localhost\Ubuntu\home\kyle\my repo"`"#,
            ),
            (
                "/home/kyle/it's here",
                r#"`dv open "\\wsl.localhost\Ubuntu\home\kyle\it's here"`"#,
            ),
            // PowerShell expands `$x` and backticks even inside double quotes.
            (
                "/tmp/my repo $x",
                r"`dv open '\\wsl.localhost\Ubuntu\tmp\my repo $x'`",
            ),
            ("/tmp/a$x", r"`dv open '\\wsl.localhost\Ubuntu\tmp\a$x'`"),
            (
                "/tmp/it's `x`",
                r"`dv open '\\wsl.localhost\Ubuntu\tmp\it''s `x`'`",
            ),
        ] {
            assert_eq!(
                message(None, None, None, cwd, Some("Ubuntu")),
                format!("{lead}{expected}"),
                "{cwd}"
            );
        }
    }

    #[test]
    fn elsewhere_it_says_there_is_no_window() {
        for distro in [None, Some(""), Some("  ")] {
            let text = message(None, None, None, "/home/kyle/api", distro);
            assert!(text.starts_with("this build of dv has no window"), "{text}");
        }
        // Bare `dv` reads the same.
        let error = refusal(&GlobalArgs::default(), None, Path::new("/x"), None, &|_| {
            false
        });
        assert!(error.message.starts_with("this build of dv has no window"));
    }

    #[test]
    fn a_pull_request_is_named_by_its_flag() {
        let suggestion = |open: OpenArgs, exists: &dyn Fn(&Path) -> bool| {
            refusal(
                &GlobalArgs::default(),
                Some(&open),
                Path::new("/home/kyle/api"),
                Some("Ubuntu"),
                exists,
            )
            .message
        };
        let lead = "this is dv's Linux build without a window; open the repository from \
                    Windows: ";
        // `--pr` as given, beside the path the line names or the working directory.
        assert_eq!(
            suggestion(
                OpenArgs {
                    target: None,
                    pr: Some("482".into()),
                },
                &|_| false
            ),
            format!(r"{lead}`dv open \\wsl.localhost\Ubuntu\home\kyle\api --pr 482`")
        );
        assert_eq!(
            suggestion(
                OpenArgs {
                    target: Some("../web".into()),
                    pr: Some("#7".into()),
                },
                &|_| false
            ),
            format!(r##"{lead}`dv open \\wsl.localhost\Ubuntu\home\kyle\web --pr "#7"`"##)
        );
        // A folder named as a pull request is written is the folder.
        assert_eq!(
            suggestion(
                OpenArgs {
                    target: Some("#482".into()),
                    pr: None,
                },
                &|path| path == Path::new("/home/kyle/api/#482")
            ),
            format!(r##"{lead}`dv open "\\wsl.localhost\Ubuntu\home\kyle\api\#482"`"##)
        );
    }
}
