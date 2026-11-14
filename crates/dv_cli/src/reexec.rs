//! A command for a repository inside a WSL distro runs there (docs/PLAN.md §10.4).
//!
//! On Windows, a command that reads a repository (`dv status`, `dv review …`, `dv comment …`)
//! whose repository is a WSL location (`--wsl <distro>:<path>`, `--repo \\wsl.localhost\…`, or a
//! working directory in a distro, PLAN §10.1) crosses into the distro once, as this app's own
//! Linux build, and passes its output and exit code through: one `wsl.exe` for the command,
//! never one per git call (PLAN D6). Inside a distro dv is that Linux build already and crosses
//! nothing; `setup`, `doctor` and `backend` never cross, and `--help` and `--version` never reach
//! a command.
//!
//! - **The line:** `wsl.exe -d <distro> --cd / --exec [/usr/bin/env HOME=<DV_WSL_HOME>] /bin/sh
//!   -c <RUN> sh <x86_64 hash|-> <aarch64 hash|-> <folder> <args…>` (`dv_backend::wsl::RUN`): it
//!   runs `~/.local/share/dv/bin/dv-<hash>`, this build exactly (never whatever `~/.local/bin/dv`
//!   is), in `<folder>`: the working directory when it is in that distro, else the
//!   repository's ([`workdir`]). Its exit code is the command's. It dies with this process
//!   (`dv_core::job`), so a `dv` that is killed leaves no `wsl.exe` behind, nor the distro's
//!   `dv review wait`. The distro is named as given (`wsl.exe` takes names without case): no
//!   `wsl.exe --list` runs first.
//! - **Its output** ([`shares_console`]): when stdout and stderr are both terminals the child
//!   shares this process's console, so the distro's dv sees a terminal. Otherwise (a pipe, a
//!   file, an agent's shell) dv pipes the child's stdout and stderr and copies them into its
//!   own: `wsl.exe` relays the two streams with a write position each, so a caller sending both
//!   to one file (`> out 2>&1`) would get them written over each other. stderr is copied as it
//!   arrives; stdout whole once the child exits (no dv command streams its stdout), and only
//!   when the distro's dv ran: a code outside 0–255 is `wsl.exe`'s own failure, whose words
//!   (UTF-16LE) on stdout become the error's detail rather than reaching stdout, which under
//!   `--json` holds the error document alone.
//! - **The arguments** ([`forwarded`]) are the command line as given, the repository first by
//!   its path in the distro (`--repo <path>`, the `--repo` and `--wsl` given dropped), a `--file`
//!   as one word, `--file=<path>` (so a path starting with `-` stays its value there), a path
//!   spelled through `\\wsl.localhost\<distro>\…` as its path there, and a `--body-file` or
//!   `--note-file` read here (stdin for `-`, else a file relative to this working directory,
//!   which must be UTF-8: checked here, so the error names the file) and handed to the child's
//!   stdin as `-`, so a Windows path works and the second run after an install loses nothing;
//!   with nothing to read the child's stdin is empty.
//! - **Who it acts as** crosses too: `CLAUDECODE`, `DV_AUTHOR`, `DV_MODEL` and `DV_REVIEW`, those set, join
//!   `WSLENV` as `NAME/u` after whatever else it lists, replacing an entry it has for the same
//!   name in any case or with other flags, so each crosses verbatim ([`environment`]).
//! - **Missing pieces:** the build not installed there (`RUN`'s 120) is installed
//!   (`dv_backend::install`, "dv: installing dv in Ubuntu…" on stderr, and a warning when it
//!   couldn't point `~/.local/bin/dv` at it) and the command runs once more. WSL, the distro or
//!   this app's Linux build unavailable, an install that fails, and `wsl.exe` failing to start
//!   the distro are exit 4 (`backend_unavailable`) with what to do.
//!   The Linux build and `DV_WSL_HOME` are checked before any `wsl.exe` starts; only when
//!   `wsl.exe` couldn't start the distro does `wsl.exe --list` run, to say why (no such distro,
//!   WSL not installed).
//! - `DV_WSL_HOME` (a POSIX path) stands in for the distro user's home, as everywhere dv reaches
//!   into a distro (tests and agents' runs; CLAUDE.md).

use std::ffi::OsString;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use dv_backend::install::{self, BuildState, LINUX_DIR_VAR, LinuxBuilds, WSL_HOME_VAR};
use dv_backend::wsl::{self, Arch, RUN, RUN_MISSING, RUN_NO_BUILD, Shell, WslError};
use dv_core::location::Location;

use crate::args::{Command, GlobalArgs, ReviewCommand};
use crate::body;
use crate::context::{Context, Env};
use crate::error::{CliError, Exit, code};
use crate::output;
use crate::repo;

/// The variables that cross into the distro with a command: who it acts as, on which model,
/// and which review it means (PLAN §2.5, §10.2).
pub(crate) const CROSSING: [&str; 4] = ["CLAUDECODE", "DV_AUTHOR", "DV_MODEL", "DV_REVIEW"];

/// The distro and path of the repository `command` runs on when it runs inside a WSL distro
/// (module docs); `None` when it runs here.
pub(crate) fn wsl_repository(
    global: &GlobalArgs,
    command: &Command,
    ctx: &Context,
) -> Result<Option<(String, String)>, CliError> {
    if !cfg!(windows) || !reads_repository(command) {
        return Ok(None);
    }
    // On Windows, which is no distro's.
    Ok(match repo::location(global, &ctx.cwd, None)? {
        Location::Wsl { distro, path } => Some((distro, path)),
        Location::Local(_) => None,
    })
}

/// Whether `command` reads a repository: all but `setup`, `doctor` and `backend`, and `review
/// new --pr`, which is refused before any repository is read.
pub(crate) fn reads_repository(command: &Command) -> bool {
    match command {
        Command::Status | Command::Comment(_) => true,
        Command::Review(ReviewCommand::New(args)) => args.pr.is_none(),
        Command::Review(_) => true,
        Command::Setup(_) | Command::Doctor | Command::Backend(_) => false,
    }
}

/// Runs the command line (`ctx.args`) inside `distro`, on the repository at `path` there
/// (module docs).
pub(crate) fn run(distro: &str, path: &str, ctx: &mut Context) -> Result<Exit, CliError> {
    let builds =
        LinuxBuilds::find(&install::linux_dir(ctx.env.var(LINUX_DIR_VAR))).map_err(unavailable)?;
    if builds.is_empty() {
        return Err(unavailable(builds.missing(Arch::host())));
    }
    let home = wsl::sandbox(ctx.env.var_lossy(WSL_HOME_VAR)).map_err(unavailable)?;
    let shell = Shell::wsl(distro.to_string(), home).map_err(unavailable)?;
    run_in(&shell, &builds, distro, path, ctx)
}

/// [`run`] where `shell` runs, with `builds`: the distro, or this machine's `/bin/sh` in a
/// scratch home (the tests on Linux).
pub(crate) fn run_in(
    shell: &Shell,
    builds: &LinuxBuilds,
    distro: &str,
    path: &str,
    ctx: &mut Context,
) -> Result<Exit, CliError> {
    if ctx.args.is_empty() {
        return Err(CliError::new(
            Exit::Failure,
            code::INTERNAL,
            format!(
                "the command line isn't known, so it can't run in {}",
                shell.name()
            ),
        ));
    }
    let forwarded = forwarded(&ctx.args, distro, path);
    let input = match &forwarded.text_file {
        Some(file) => Some(body::forwarded(file, ctx)?),
        None => None,
    };
    let hashes = builds.hash_args();
    let mut line: Vec<OsString> = hashes.into_iter().map(OsString::from).collect();
    line.push(workdir(&ctx.cwd, distro, path).into());
    line.extend(forwarded.args);
    let env = environment(&ctx.env);
    let mut installed = false;
    loop {
        let ran = run_once(shell, &line, &env, input.as_deref(), ctx)?;
        match ran.status.code() {
            Some(RUN_MISSING) if !installed => {
                install_build(shell, builds, ctx)?;
                installed = true;
            }
            Some(code) if code == RUN_MISSING || RUN_NO_BUILD.contains(&code) => {
                return Err(missing(shell, builds, code));
            }
            _ => return finish(shell, ran, ctx),
        }
    }
}

/// What the distro's dv gets (module docs): its arguments, and the `--body-file` or
/// `--note-file` named, which is read here and handed to it on stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Forwarded {
    pub args: Vec<OsString>,
    pub text_file: Option<PathBuf>,
}

/// The arguments the distro's dv gets for the command line `args` (the program name first,
/// as parsed) on the repository at `repo` in `distro` (module docs). The line parsed, so an
/// option's value is the word after it or after its `=`; only `--body` and `--note` take a
/// value that looks like an option (`args::parse_text`), so the word after them is skipped,
/// and a lone `--` ends the options.
pub(crate) fn forwarded(args: &[OsString], distro: &str, repo: &str) -> Forwarded {
    let mut out: Vec<OsString> = vec!["--repo".into(), repo.into()];
    let mut text_file = None;
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        let Some(text) = arg.to_str() else {
            out.push(arg.clone());
            continue;
        };
        if text == "--" {
            out.push(arg.clone());
            out.extend(rest.cloned());
            break;
        }
        let (name, inline) = match text.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value)),
            _ => (text, None),
        };
        match name {
            "--repo" | "--wsl" => {
                value(inline, &mut rest);
            }
            "--body-file" | "--note-file" => {
                text_file = value(inline, &mut rest).map(PathBuf::from);
                out.extend([OsString::from(name), OsString::from("-")]);
            }
            // One word, so a value that starts with `-` stays the option's value there too.
            "--file" => match value(inline, &mut rest) {
                Some(path) => {
                    let mut word = OsString::from("--file=");
                    word.push(in_distro(path, distro));
                    out.push(word);
                }
                None => out.push(name.into()),
            },
            "--body" | "--note" if inline.is_none() => {
                out.push(arg.clone());
                out.extend(rest.next().cloned());
            }
            _ => out.push(arg.clone()),
        }
    }
    Forwarded {
        args: out,
        text_file,
    }
}

/// An option's value: the text after its `=`, else the next word.
fn value<'a>(
    inline: Option<&str>,
    rest: &mut impl Iterator<Item = &'a OsString>,
) -> Option<OsString> {
    match inline {
        Some(value) => Some(value.into()),
        None => rest.next().cloned(),
    }
}

/// `path` as the distro spells it when Windows spells it through the distro's share
/// (`\\wsl.localhost\Ubuntu\home\kyle\api\src\x.rs` → `/home/kyle/api/src/x.rs`); as given
/// otherwise.
fn in_distro(path: OsString, distro: &str) -> OsString {
    match path.to_str().map(Location::parse) {
        Some(Ok(Location::Wsl {
            distro: there,
            path,
        })) if there.eq_ignore_ascii_case(distro) => path.into(),
        _ => path,
    }
}

/// The folder the distro's dv starts in: the working directory `cwd` when it is in `distro`
/// (`\\wsl.localhost\Ubuntu\home\kyle\api\src` → `/home/kyle/api/src`), else the repository's
/// path there, `repo`.
pub(crate) fn workdir(cwd: &Path, distro: &str, repo: &str) -> String {
    match Location::parse(&cwd.to_string_lossy()) {
        Ok(Location::Wsl {
            distro: there,
            path,
        }) if there.eq_ignore_ascii_case(distro) => path,
        _ => repo.to_string(),
    }
}

/// The environment `wsl.exe` gets (module docs): each of [`CROSSING`] as `env` has it (set, or
/// removed), and `WSLENV` with the entries it has, in order, then those set as `NAME/u`. An
/// entry it has for one of those set goes, whatever its case and flags (`CLAUDECODE/w` wouldn't
/// pass the value into the distro, `DV_AUTHOR/p` would translate it as a path, `dv_author`
/// would export it under a name dv doesn't read), so each crosses verbatim under its own name.
/// `WSLENV` is removed when it would be empty.
pub(crate) fn environment(env: &Env) -> Vec<(String, Option<String>)> {
    let mut changes = Vec::new();
    let mut entries: Vec<String> = env
        .var("WSLENV")
        .unwrap_or_default()
        .split(':')
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect();
    let mut crossing = Vec::new();
    for name in CROSSING {
        let value = env.var(name);
        if value.is_some() {
            entries.retain(|entry| {
                entry
                    .split('/')
                    .next()
                    .is_none_or(|listed| !listed.eq_ignore_ascii_case(name))
            });
            crossing.push(format!("{name}/u"));
        }
        changes.push((name.to_string(), value));
    }
    entries.extend(crossing);
    changes.push((
        "WSLENV".to_string(),
        (!entries.is_empty()).then(|| entries.join(":")),
    ));
    changes
}

/// Whether the run shares this process's console rather than dv copying its output (module
/// docs): only when stdout and stderr are this process's own and both terminals.
pub(crate) fn shares_console(ctx: &Context) -> bool {
    ctx.process_stdio && ctx.stdout_is_terminal && ctx.stderr_is_terminal
}

/// A finished run of [`RUN`].
struct Ran {
    status: ExitStatus,
    /// Its stdout, when dv piped it (module docs): `None` when the child wrote to the console
    /// itself.
    stdout: Option<Vec<u8>>,
}

/// Runs [`RUN`] with `line` once: `input` on its stdin (else stdin is empty), until it exits.
/// Its output goes to the console, or dv copies it (module docs): stderr into the context's
/// as it arrives, stdout kept for [`finish`].
fn run_once(
    shell: &Shell,
    line: &[OsString],
    env: &[(String, Option<String>)],
    input: Option<&[u8]>,
    ctx: &mut Context,
) -> Result<Ran, CliError> {
    let mut command = shell.console_command(RUN, line);
    for (name, value) in env {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    if !shares_console(ctx) {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command.spawn().map_err(|error| {
        CliError::unavailable(format!(
            "couldn't run dv in {} ({program}: {error})",
            shell.name()
        ))
    })?;
    dv_core::job::tie_to_this_process(&child);
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let err = &mut ctx.stderr;
    let (status, stdout) = std::thread::scope(|scope| {
        if let (Some(mut pipe), Some(input)) = (stdin, input) {
            // The child may stop reading early (an error before it reads its text): a broken
            // pipe then is its business, reported in its own output.
            scope.spawn(move || {
                let _ = pipe.write_all(input);
            });
        }
        let collected = stdout.map(|mut from| {
            scope.spawn(move || {
                let mut bytes = Vec::new();
                let _ = from.read_to_end(&mut bytes);
                bytes
            })
        });
        if let Some(mut from) = stderr {
            scope.spawn(move || {
                let _ = io::copy(&mut from, err);
                let _ = err.flush();
            });
        }
        let status = child.wait();
        let stdout = collected.map(|thread| thread.join().unwrap_or_default());
        (status, stdout)
    });
    let status = status.map_err(|error| {
        CliError::unavailable(format!("dv in {} didn't finish: {error}", shell.name()))
    })?;
    Ok(Ran { status, stdout })
}

/// Installs this app's build where `shell` runs, saying so on stderr: exit 4 when it can't.
fn install_build(shell: &Shell, builds: &LinuxBuilds, ctx: &mut Context) -> Result<(), CliError> {
    let probe = install::probe(shell, builds).map_err(unavailable)?;
    if probe.build == BuildState::Ok {
        // Installed meanwhile (the app, or another command): run again.
        return Ok(());
    }
    let build = builds
        .get(probe.arch)
        .ok_or_else(|| unavailable(builds.missing(probe.arch)))?;
    output::notice(ctx, &format!("installing dv in {}…", shell.name()));
    let installed = install::install(shell, build, &|_, _| {}).map_err(|error| {
        CliError::unavailable(format!("couldn't install dv in {}: {error}", shell.name()))
    })?;
    for warning in &installed.warnings {
        output::warning(ctx, warning)?;
    }
    Ok(())
}

/// The error for [`RUN`] finding no build to run (`code`): still missing after an install, or
/// none for the distro's architecture, which the probe names.
fn missing(shell: &Shell, builds: &LinuxBuilds, code: i32) -> CliError {
    if code == RUN_MISSING {
        return CliError::unavailable(format!(
            "dv is still missing in {} after installing it",
            shell.name()
        ));
    }
    match install::probe(shell, builds) {
        Err(error) => unavailable(error),
        Ok(probe) if builds.get(probe.arch).is_none() => unavailable(builds.missing(probe.arch)),
        Ok(_) => CliError::unavailable(format!(
            "dv couldn't start in {} (exit code {code})",
            shell.name()
        )),
    }
}

/// What a finished run means (module docs): the distro's dv's own exit code, passed through,
/// with the stdout dv kept written out; a code no Linux program exits with is `wsl.exe`'s own
/// failure (it couldn't start the distro: -1), exit 4, worded from `wsl.exe --list` when that
/// tells why, what `wsl.exe` printed on stdout its detail and never on stdout.
fn finish(shell: &Shell, ran: Ran, ctx: &mut Context) -> Result<Exit, CliError> {
    let exit = match ran.status.code() {
        Some(code) => match u8::try_from(code) {
            Ok(code) => Exit::from_code(code),
            Err(_) => return Err(wsl_failed(shell, code, ran.stdout.as_deref())),
        },
        // Ended by a signal (this machine's shell, in tests).
        None => Exit::Failure,
    };
    if let Some(stdout) = &ran.stdout {
        ctx.stdout
            .write_all(stdout)
            .and_then(|()| ctx.stdout.flush())
            .map_err(|error| {
                CliError::new(
                    Exit::Failure,
                    code::IO,
                    format!("writing the output: {error}"),
                )
            })?;
    }
    Ok(exit)
}

/// The exit-4 error for `wsl.exe` failing with `code` before the distro's dv ran ([`finish`]):
/// why, from `wsl.exe --list` when it tells (no such distro, WSL not installed), and what
/// `wsl.exe` said on `stdout`, when dv kept it, as one line of detail.
pub(crate) fn wsl_failed(shell: &Shell, code: i32, stdout: Option<&[u8]>) -> CliError {
    let error = match shell.distro().map(wsl::find_distro) {
        Some(Err(error)) => unavailable(error),
        _ => CliError::unavailable(format!(
            "wsl.exe couldn't run dv in {} (exit code {code})",
            shell.name()
        )),
    };
    let said = stdout
        .map(|bytes| {
            wsl::decode(bytes)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|said| !said.is_empty());
    error.with_detail(said.map(|said| format!("wsl.exe said: {said}")))
}

/// WSL, the distro or this app's Linux build unavailable: exit 4.
fn unavailable(error: WslError) -> CliError {
    CliError::unavailable(error.to_string())
}
