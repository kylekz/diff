//! The processes a `LocalBackend` spawns on this machine (`super`'s module docs, PLAN §18.3),
//! started through `toolbox_process`: each runs in a tree of its own (a process group on Unix,
//! a job created before the process runs on Windows, which also dies with dv), killed with the
//! process, when its handle drops and when the process exits. What dv adds is the spec's check
//! (an absolute program, no batch file, a working directory inside the worktree) and its
//! events' shape: stderr lines cut at [`STDERR_LINE_MAX`], an exit code rather than a status.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use toolbox_process::{Command, Event, Handle, ProcessError, Stop};

use super::{
    ProcessControl, ProcessEvent, ProcessHandle, ProcessSink, ProcessSpec, bounded_line, ended,
};
use crate::backend::{BackendError, paths};

/// Starts `spec` in the worktree at `toplevel` (`super`'s module docs).
pub(in crate::backend) fn spawn(
    toplevel: &Path,
    spec: &ProcessSpec,
    mut sink: ProcessSink,
) -> Result<ProcessHandle, BackendError> {
    let (program, cwd) = check(toplevel, spec)?;
    let command = spec.env.iter().fold(
        Command::new(program).args(&spec.args).current_dir(cwd),
        |command, (name, value)| match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        },
    );
    let handle = toolbox_process::spawn_streaming(&command, move |event| {
        sink(match event {
            Event::Stdout(bytes) => ProcessEvent::Stdout(bytes),
            Event::Stderr(line) => {
                ProcessEvent::Stderr(bounded_line(String::from_utf8_lossy(&line).into_owned()))
            }
            Event::Exit(status) => ProcessEvent::Exit(status.and_then(|status| status.code())),
        })
    })
    .map_err(|error| BackendError::StartProcess {
        program: spec.program.clone(),
        source: match error {
            ProcessError::Start { source, .. } => source,
            other => io::Error::other(other),
        },
    })?;
    Ok(ProcessHandle::new(
        handle.pid(),
        LocalProcess {
            program: spec.program.clone(),
            handle,
        },
    ))
}

/// The executable and the working directory `spec` names, checked (`ProcessSpec`'s docs).
fn check(toplevel: &Path, spec: &ProcessSpec) -> Result<(PathBuf, PathBuf), BackendError> {
    let refuse = |reason: &'static str| BackendError::InvalidProcess {
        program: spec.program.clone(),
        reason: reason.into(),
    };
    if spec.program.contains('\0') {
        return Err(refuse("the program's path contains NUL"));
    }
    let program = PathBuf::from(&spec.program);
    if !program.is_absolute() {
        return Err(refuse("the program must be an absolute path"));
    }
    if cfg!(windows) && is_batch_file(&program) {
        return Err(refuse(
            "a batch file runs only through cmd.exe, and dv runs no shell",
        ));
    }
    if spec.args.iter().any(|arg| arg.contains('\0')) {
        return Err(refuse("an argument contains NUL"));
    }
    for (name, value) in &spec.env {
        if name.is_empty() || name.contains(['=', '\0']) {
            return Err(refuse(
                "an environment variable's name is empty or has '=' or NUL",
            ));
        }
        if value.as_ref().is_some_and(|value| value.contains('\0')) {
            return Err(refuse("an environment variable's value contains NUL"));
        }
    }
    let cwd = if spec.cwd.is_empty() {
        toplevel.to_path_buf()
    } else {
        paths::worktree_path(toplevel, &spec.cwd)?
    };
    match fs::metadata(&cwd) {
        Ok(metadata) if metadata.is_dir() => Ok((program, cwd)),
        Ok(_) => Err(BackendError::io(
            "starting a process in",
            &cwd,
            io::Error::new(io::ErrorKind::NotADirectory, "not a directory"),
        )),
        Err(error) => Err(BackendError::io("starting a process in", &cwd, error)),
    }
}

/// Whether Windows would run `program` through `cmd.exe`: a `.bat` or `.cmd` file, also spelled
/// with trailing dots or spaces, which Windows drops from a file name.
fn is_batch_file(program: &Path) -> bool {
    let Some(name) = program.file_name() else {
        return false;
    };
    let name = name.to_string_lossy();
    let name = name.trim_end_matches(['.', ' ']).to_ascii_lowercase();
    name.ends_with(".bat") || name.ends_with(".cmd")
}

/// A local process's [`ProcessControl`]. Dropping the handle kills its tree and stops the
/// delivery to the sink.
struct LocalProcess {
    /// As the spec named it, for errors.
    program: String,
    handle: Handle,
}

impl ProcessControl for LocalProcess {
    fn write(&self, bytes: &[u8]) -> Result<(), BackendError> {
        self.handle.write(bytes).map_err(|error| match error {
            ProcessError::Write { source, .. } => BackendError::Io {
                op: "writing to".into(),
                path: self.program.clone(),
                source,
            },
            _ => ended(&self.program),
        })
    }

    fn kill(&self) {
        self.handle.stop(Stop::Kill);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_is_checked_before_anything_starts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("file"), "").unwrap();
        let absolute = if cfg!(windows) {
            r"C:\Windows\System32\where.exe"
        } else {
            "/bin/true"
        };
        let reason = |spec: ProcessSpec| match check(dir.path(), &spec) {
            Err(BackendError::InvalidProcess { reason, .. }) => reason.into_owned(),
            other => panic!("{spec:?}: {other:?}"),
        };
        assert_eq!(
            reason(ProcessSpec::new("tsc")),
            "the program must be an absolute path"
        );
        assert_eq!(
            reason(ProcessSpec::new(absolute).args(["a\0b"])),
            "an argument contains NUL"
        );
        assert_eq!(
            reason(ProcessSpec::new(absolute).env("A=B", Some("x"))),
            "an environment variable's name is empty or has '=' or NUL"
        );
        if cfg!(windows) {
            for program in [
                r"C:\tools\tsc.CMD",
                r"C:\tools\tsc.bat",
                r"C:\tools\tsc.cmd.",
                r"C:\tools\tsc.cmd ",
                r"C:\tools\tsc.bat. .",
            ] {
                assert_eq!(
                    reason(ProcessSpec::new(program)),
                    "a batch file runs only through cmd.exe, and dv runs no shell",
                    "{program:?}"
                );
            }
        }
        assert!(is_batch_file(Path::new("/x/tsc.cmd.")));
        assert!(!is_batch_file(Path::new("/x/tsc.exe")));
        assert!(!is_batch_file(Path::new("/x/cmd")));
        assert!(matches!(
            check(dir.path(), &ProcessSpec::new(absolute).cwd("../x")),
            Err(BackendError::InvalidPath { .. })
        ));
        assert!(matches!(
            check(dir.path(), &ProcessSpec::new(absolute).cwd("missing")),
            Err(BackendError::Io { .. })
        ));
        assert!(matches!(
            check(dir.path(), &ProcessSpec::new(absolute).cwd("file")),
            Err(BackendError::Io { .. })
        ));
        let (program, cwd) = check(dir.path(), &ProcessSpec::new(absolute).cwd("sub")).unwrap();
        assert_eq!(program, PathBuf::from(absolute));
        assert_eq!(cwd, dir.path().join("sub"));
        assert_eq!(
            check(dir.path(), &ProcessSpec::new(absolute)).unwrap().1,
            dir.path()
        );
    }
}
