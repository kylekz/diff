//! WSL seen from Windows (docs/PLAN.md D6, §5.2): the distros there are ([`distros`]), the
//! architectures dv builds for ([`Arch`]), how dv's scripts run inside a distro ([`Shell`]),
//! and the start of a distro's `dv backend --stdio` ([`launch`]), which installs this app's
//! Linux build first when the distro lacks it (`crate::install`).
//!
//! **Scripts.** Everything dv does inside a distro is one `/bin/sh` script run as `wsl.exe -d
//! <distro> --cd / --exec /bin/sh -c <script> sh <args…>`: no shell on the Windows side and no
//! login shell inside, so nothing from the user's profile runs and the arguments arrive as they
//! were passed (Rust quotes them for `CreateProcess` and `wsl.exe` hands them on intact: spaces,
//! `$`, quotes, backslashes and newlines, checked by `tests/wsl.rs` against a real distro).
//! `--cd /` keeps `wsl.exe` from translating this process's working directory, which it warns
//! about when it can't. With the sandbox seam `DV_WSL_HOME` (a POSIX path) set, the line is
//! `… --exec /usr/bin/env HOME=<it> /bin/sh -c …`, so tests and agents' runs install into and
//! run from a scratch home, never the distro user's own (CLAUDE.md); [`sandbox`] alone decides
//! what its value means.
//!
//! **No console window.** Every `wsl.exe` dv starts is made by one builder ([`wsl_exe`]),
//! which creates it without a console window: the app runs detached, and a console child of
//! it would flash one. The CLI's run inside a distro alone shares its caller's console
//! ([`Shell::console_command`]). Each also dies with this process (`dv_core::job`).
//!
//! **The launch** ([`LAUNCH`], args: this app's x86_64 and aarch64 build hashes, `-` for a
//! build it lacks) picks the hash for `uname -m` and, when `$HOME/.local/share/dv/bin/dv-<hash>`
//! is there, prints `dv-exec <arch>` and execs it as `backend --stdio`. Otherwise it prints one
//! line and exits: `dv-missing <arch>` (120: install it, then launch again), `dv-arch <uname
//! -m>` (121: an architecture dv has no build for) or `dv-no-build <arch>` (122: no build of
//! this app for that architecture). The marker line lets [`launch`] tell a server that started
//! from `wsl.exe` failing to start the distro (no such distro, WSL not installed), whose own
//! message is the error then.
//!
//! **The CLI's run** ([`RUN`], PLAN §10.4: `dv …` on Windows for a WSL repository) makes the
//! same choice of build, then runs it as `dv <args…>` in a given folder with the caller's
//! console ([`Shell::console_command`]); it prints nothing of its own, so the exit code alone
//! says the build is missing (120) or there is none for the machine (121, 122).

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, BufRead as _, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::install::{self, LinuxBuilds};
use crate::launch::Transport;

/// The program every WSL command runs through.
pub const WSL_EXE: &str = "wsl.exe";

/// How long a launch may take to say it starts dv: a cold distro boots in a few seconds.
pub const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long `wsl.exe --list` results are reused (PLAN §12.2: listing never boots a distro, and
/// the switcher asks often).
const LIST_CACHE: Duration = Duration::from_secs(2);

/// What [`LAUNCH`] and its kin begin with, a literal for `concat!`: `say` (defined before it)
/// reports, `$1` and `$2` are the x86_64 and aarch64 build hashes (`-`: none). It leaves the
/// architecture in `$a`, the hash in `$h` and the installed build's path in `$b`, or exits 121
/// or 122 (module docs).
macro_rules! select_build {
    () => {
        r#"m=$(uname -m)
case "$m" in
x86_64|amd64) a=x86_64; h=$1 ;;
aarch64|arm64) a=aarch64; h=$2 ;;
*) say "dv-arch $m"; exit 121 ;;
esac
if [ "$h" = - ]; then say "dv-no-build $a"; exit 122; fi
b="$HOME/.local/share/dv/bin/dv-$h"
"#
    };
}
pub(crate) use select_build;

/// Starts this app's build as `dv backend --stdio`, or says why not (module docs). Args: the
/// x86_64 and aarch64 build hashes.
pub const LAUNCH: &str = concat!(
    "say() { echo \"$*\"; }\n",
    select_build!(),
    r#"if [ -f "$b" ] && [ -x "$b" ]; then say "dv-exec $a"; exec "$b" backend --stdio; fi
say "dv-missing $a"
exit 120
"#
);

/// Runs this app's build as the CLI (PLAN §10.4): `dv <args…>` in the folder `$3` (`/` when
/// that folder isn't there: the arguments name the repository by its absolute path), its
/// stdio and exit code the caller's. Args: the x86_64 and aarch64 build hashes, the folder,
/// then dv's arguments. Unlike [`LAUNCH`] it prints nothing of its own, since its output is
/// the command's: the build missing is exit 120, an architecture dv has no build for 121 and
/// one this app has none for 122 ([`crate::install::probe`] then says which).
pub const RUN: &str = concat!(
    "say() { :; }\n",
    select_build!(),
    r#"cd "$3" 2> /dev/null || cd /
[ -f "$b" ] && [ -x "$b" ] || exit 120
shift 3
exec "$b" "$@"
"#
);

/// [`RUN`]'s exit code when this app's build isn't installed in the distro.
pub const RUN_MISSING: i32 = 120;

/// [`RUN`]'s exit codes for an architecture without a build: dv has none for it (121), or
/// this app has none (122).
pub const RUN_NO_BUILD: [i32; 2] = [121, 122];

/// What went wrong reaching WSL or doing something inside a distro.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WslError {
    /// `wsl.exe` isn't there (WSL isn't installed, or this isn't Windows).
    #[error("WSL isn't installed here: {0}")]
    NotInstalled(String),
    /// No distro has the name.
    #[error("there is no WSL distro named {name}{}", known_list(.known))]
    NoDistro { name: String, known: Vec<String> },
    /// `DV_WSL_HOME` isn't an absolute POSIX path.
    #[error("DV_WSL_HOME must be an absolute path inside the distro, not {0:?}")]
    BadHome(String),
    /// This app has no Linux build for the distro's architecture: `path` is missing.
    #[error("no Linux build of dv for {arch}: {} is missing (script/build-linux{} makes it)",
        .path.display(), if *.arch == Arch::Aarch64 { " --aarch64" } else { "" })]
    NoBuild { arch: Arch, path: PathBuf },
    /// The distro runs on an architecture dv has no build for (`uname -m`).
    #[error("dv has no build for {0} machines (only x86_64 and aarch64)")]
    UnknownArch(String),
    /// `wsl.exe` or a script inside the distro failed: what it said.
    #[error("{0}")]
    Failed(String),
}

impl WslError {
    /// Whether trying again can't help without someone changing something first.
    pub fn permanent(&self) -> bool {
        !matches!(self, WslError::Failed(_))
    }
}

fn known_list(known: &[String]) -> String {
    if known.is_empty() {
        " (wsl.exe --list shows none)".into()
    } else {
        format!(" (installed: {})", known.join(", "))
    }
}

/// A CPU architecture dv has a Linux build for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub const ALL: [Arch; 2] = [Arch::X86_64, Arch::Aarch64];

    /// Its name, as Rust's target triples and `uname -m` on Linux spell it.
    pub fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }

    /// The architecture `name` means (`uname -m`, Rust's `ARCH`, and their other spellings).
    pub fn from_name(name: &str) -> Option<Arch> {
        match name.trim() {
            "x86_64" | "amd64" => Some(Arch::X86_64),
            "aarch64" | "arm64" => Some(Arch::Aarch64),
            _ => None,
        }
    }

    /// The file holding this app's Linux build for it, beside `dv.exe` (PLAN §13.2).
    pub fn file_name(self) -> String {
        format!("dv-linux-{}", self.name())
    }

    /// This machine's architecture: a distro's, unless it runs under emulation.
    pub fn host() -> Arch {
        if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            Arch::X86_64
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An installed distro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distro {
    pub name: String,
    /// Running now. A failure to tell counts as not running, so nothing treats a distro as
    /// up that may not be (PLAN §5.2's connecting rule).
    pub running: bool,
}

/// What [`distros`] last listed, and when.
type Listed = Option<(Instant, Result<Vec<Distro>, WslError>)>;

/// [`distros`]' cache, which [`running_now`] refreshes too. Held while `wsl.exe` lists, so
/// everyone asking meanwhile waits for that one listing and shares its answer.
static LISTED: Mutex<Listed> = Mutex::new(None);

/// The installed distros and which run, from `wsl.exe --list --quiet` and `wsl.exe --list
/// --running --quiet` (never the localised `--verbose` table), reused for 2 s; callers asking
/// while a listing runs share it. Listing boots no distro. Blocks while `wsl.exe` runs; never
/// call it on a UI thread.
pub fn distros() -> Result<Vec<Distro>, WslError> {
    let mut cache = LISTED.lock();
    if let Some((at, listed)) = &*cache
        && at.elapsed() < LIST_CACHE
    {
        return listed.clone();
    }
    let listed = list_distros();
    *cache = Some((Instant::now(), listed.clone()));
    listed
}

/// Whether the distro named `name` (without case) runs now, listed afresh rather than from
/// [`distros`]' cache: what an automatic start asks first (PLAN §5.2), right after a death the
/// cache may predate. A distro that isn't installed, or a listing that fails, counts as not
/// running. Blocks while `wsl.exe` runs.
pub fn running_now(name: &str) -> bool {
    let listed = list_distros();
    let running = runs(&listed, name);
    *LISTED.lock() = Some((Instant::now(), listed));
    running
}

/// Whether the distro named `name` (without case) runs, as [`distros`] lists it: an answer up
/// to 2 s old, shared by everyone asking at once. What a listing asks of a connection already
/// stopped or failed (PLAN §5.2), so that a distro's rows don't each list WSL afresh; a
/// distro started within those 2 s lists as stopped once more. Blocks while `wsl.exe` runs.
pub fn running(name: &str) -> bool {
    runs(&distros(), name)
}

/// Whether `listed` has the distro `name` running.
fn runs(listed: &Result<Vec<Distro>, WslError>, name: &str) -> bool {
    listed.as_ref().is_ok_and(|all| {
        all.iter()
            .any(|distro| distro.running && distro.name.eq_ignore_ascii_case(name))
    })
}

/// The distro named `name` (without case, as `wsl.exe` takes names), by its own spelling.
pub fn find_distro(name: &str) -> Result<Distro, WslError> {
    let all = distros()?;
    all.iter()
        .find(|distro| distro.name.eq_ignore_ascii_case(name))
        .cloned()
        .ok_or_else(|| WslError::NoDistro {
            name: name.to_string(),
            known: all.into_iter().map(|distro| distro.name).collect(),
        })
}

fn list_distros() -> Result<Vec<Distro>, WslError> {
    let all = run_wsl(&["--list", "--quiet"])?;
    if !all.status.success() {
        return Err(WslError::Failed(format!(
            "wsl.exe --list failed ({}): {}",
            all.status,
            words(&all)
        )));
    }
    let names = parse_names(&all.stdout);
    // A non-zero exit here is "none running" as often as anything else; either way none counts
    // as running. Names that aren't installed distros are a message, not a distro.
    let running = match run_wsl(&["--list", "--running", "--quiet"]) {
        Ok(output) if output.status.success() => parse_names(&output.stdout),
        _ => Vec::new(),
    };
    Ok(names
        .into_iter()
        .map(|name| Distro {
            running: running
                .iter()
                .any(|other| other.eq_ignore_ascii_case(&name)),
            name,
        })
        .collect())
}

/// `wsl.exe <args>` to its end, without a console window.
fn run_wsl(args: &[&str]) -> Result<Output, WslError> {
    let mut command = wsl_exe();
    command.args(args);
    run(command, None, Duration::from_secs(30))
}

/// `wsl.exe` as the app starts it, with no console window (module docs). Every `wsl.exe` dv
/// starts is built here, but the CLI's run inside a distro ([`Shell::console_command`]).
fn wsl_exe() -> Command {
    toolbox_process::Command::new(WSL_EXE).to_std()
}

/// The distro names `wsl.exe --list --quiet` printed: one per line, UTF-16LE (with or without
/// a byte order mark) or UTF-8, blank lines and NULs ignored.
pub fn parse_names(bytes: &[u8]) -> Vec<String> {
    decode(bytes)
        .lines()
        .map(|line| line.trim_matches(|c: char| c.is_whitespace() || c == '\0'))
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Text `wsl.exe` printed: its own messages are UTF-16LE (a byte order mark, or a NUL in every
/// other byte of ASCII text), a Linux program's output passes through as UTF-8.
pub fn decode(bytes: &[u8]) -> String {
    let utf16 = |bytes: &[u8]| {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest);
    }
    let nuls = bytes.iter().filter(|&&byte| byte == 0).count();
    if bytes.len() >= 4 && nuls > bytes.len() / 4 {
        // A line may start on the odd byte when the text was split after a `\n` (0x0A 0x00).
        let start = usize::from(bytes[0] == 0 && bytes.get(1).is_some_and(|&byte| byte != 0));
        return utf16(&bytes[start..]).replace('\0', "");
    }
    String::from_utf8_lossy(bytes).replace('\0', "")
}

/// What a finished command said, stdout then stderr, as one line.
pub(crate) fn words(output: &Output) -> String {
    let text = format!("{}\n{}", decode(&output.stdout), decode(&output.stderr));
    let words: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if words.is_empty() {
        format!("it printed nothing ({})", output.status)
    } else {
        words.join(" ")
    }
}

/// Where dv's scripts run (module docs): a distro through `wsl.exe`, or this machine's
/// `/bin/sh` (the scripts' own tests on Linux), with the home they see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    /// `None`: this machine's `/bin/sh`.
    distro: Option<String>,
    /// The home the scripts install into and run from: `DV_WSL_HOME`, else the user's own.
    home: Option<String>,
    /// More variables the scripts run with ([`Shell::with_env`]).
    env: Vec<(String, String)>,
}

impl Shell {
    /// `distro`'s `/bin/sh`, with `home` (`DV_WSL_HOME`) as `$HOME` when given.
    pub fn wsl(distro: impl Into<String>, home: Option<String>) -> Result<Self, WslError> {
        Ok(Self {
            distro: Some(distro.into()),
            home: sandbox(home)?,
            env: Vec::new(),
        })
    }

    /// This machine's `/bin/sh` with `home` as `$HOME`: the same scripts without `wsl.exe`,
    /// which their tests run on Linux.
    pub fn local(home: Option<String>) -> Result<Self, WslError> {
        Ok(Self {
            distro: None,
            home: sandbox(home)?,
            env: Vec::new(),
        })
    }

    /// `self` running its scripts with `name=value` set too (inside a distro through
    /// `/usr/bin/env`, as the home is): a test's stand-in for a program the distro's dv runs
    /// (`DV_GH`).
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    /// The distro, `None` for this machine's shell.
    pub fn distro(&self) -> Option<&str> {
        self.distro.as_deref()
    }

    /// The home the scripts see when it isn't the user's own.
    pub fn home(&self) -> Option<&str> {
        self.home.as_deref()
    }

    /// Where the scripts run, as messages name it: the distro, or "this machine".
    pub fn name(&self) -> &str {
        self.distro.as_deref().unwrap_or("this machine")
    }

    /// `script` run with `args` as `$1…` (module docs), with no console window. Its stdio is
    /// the caller's to set.
    pub fn command(&self, script: &str, args: &[&str]) -> Command {
        let program = match &self.distro {
            Some(_) => wsl_exe(),
            None => Command::new("/bin/sh"),
        };
        self.line(program, script, args)
    }

    /// [`Shell::command`] sharing this process's console, for a command whose output a person
    /// reads there: the CLI run inside the distro (PLAN §10.4), the one `wsl.exe` dv starts
    /// with a console (module docs).
    pub fn console_command<I, S>(&self, script: &str, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let program = match &self.distro {
            Some(_) => Command::new(WSL_EXE),
            None => Command::new("/bin/sh"),
        };
        self.line(program, script, args)
    }

    /// `program` (`wsl.exe`, or this machine's `/bin/sh`) given the rest of the line that runs
    /// `script` with `args` (module docs).
    fn line<I, S>(&self, mut command: Command, script: &str, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        match &self.distro {
            Some(distro) => {
                command.args(["-d", distro, "--cd", "/", "--exec"]);
                if self.home.is_some() || !self.env.is_empty() {
                    command.arg("/usr/bin/env");
                }
                if let Some(home) = &self.home {
                    command.arg(format!("HOME={home}"));
                }
                for (name, value) in &self.env {
                    command.arg(format!("{name}={value}"));
                }
                command.arg("/bin/sh");
            }
            None => {
                if let Some(home) = &self.home {
                    command.env("HOME", home);
                }
                for (name, value) in &self.env {
                    command.env(name, value);
                }
            }
        }
        command.args(["-c", script, "sh"]).args(args);
        command
    }

    /// The command line of `script` for the diagnostics, the script itself named `what`.
    pub fn describe(&self, what: &str, args: &[&str]) -> String {
        let mut line = match &self.distro {
            Some(distro) => format!("{WSL_EXE} -d {distro} --cd / --exec "),
            None => String::new(),
        };
        if self.home.is_some() || !self.env.is_empty() {
            line.push_str("/usr/bin/env ");
        }
        if let Some(home) = &self.home {
            line.push_str(&format!("HOME={home} "));
        }
        for (name, value) in &self.env {
            line.push_str(&format!("{name}={value} "));
        }
        line.push_str(&format!("/bin/sh -c <{what}> sh"));
        for arg in args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }

    /// Runs `script` to its end within `timeout` (stdin empty).
    pub fn run(&self, script: &str, args: &[&str], timeout: Duration) -> Result<Output, WslError> {
        run(self.command(script, args), None, timeout)
    }
}

/// What `DV_WSL_HOME` (`home`, as [`home_var`] reads it) means, the one place that decides it
/// (PLAN §5.2): unset is the distro user's own home (`None`); set to a plain absolute POSIX
/// path that names a folder other than `/` (no `..` part, no backslash, no line break or NUL,
/// nothing around it), that path is the scratch home, normalised: `.` parts, repeated and
/// trailing slashes dropped. Anything else it is set to — blank, whitespace, relative, `/` in
/// any spelling (`//`, `/.`, `/./`), a path Git Bash made a Windows one of (`C:/Users/…`), a
/// value that isn't Unicode — is an error, never the user's own home.
pub fn sandbox(home: Option<String>) -> Result<Option<String>, WslError> {
    let Some(home) = home else {
        return Ok(None);
    };
    let parts: Vec<&str> = home
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let fine = home.starts_with('/')
        && !parts.is_empty()
        && home.trim() == home
        && !home.contains(['\\', '\n', '\r', '\0', char::REPLACEMENT_CHARACTER])
        && !parts.contains(&"..");
    if !fine {
        return Err(WslError::BadHome(home));
    }
    Ok(Some(format!("/{}", parts.join("/"))))
}

/// `DV_WSL_HOME` as this process's environment has it, for [`sandbox`]: a value that isn't
/// Unicode reads with U+FFFD in it, which `sandbox` refuses, so it never counts as unset.
pub fn home_var() -> Option<String> {
    std::env::var_os(crate::install::WSL_HOME_VAR).map(|home| home.to_string_lossy().into_owned())
}

/// Whether `home` (`DV_WSL_HOME`'s value) names a scratch home: what an automation run needs
/// before it may reach a real distro (CLAUDE.md). Unset, or set to anything [`sandbox`]
/// refuses, it doesn't.
pub fn is_sandbox(home: Option<String>) -> bool {
    matches!(sandbox(home), Ok(Some(_)))
}

/// Input streamed to a command's stdin: `len` bytes from `reader`, `progress` hearing each
/// chunk written (bytes so far, `len`).
pub(crate) struct Input<'a> {
    pub reader: &'a mut dyn Read,
    pub len: u64,
    pub progress: &'a dyn Fn(u64, u64),
}

/// How often progress is reported while input streams.
const PROGRESS_STEP: u64 = 256 * 1024;

/// Runs `command` to its end: `input` written to its stdin (else stdin is empty), stdout and
/// stderr collected, the process killed once `timeout` has passed.
pub(crate) fn run(
    mut command: Command,
    input: Option<Input<'_>>,
    timeout: Duration,
) -> Result<Output, WslError> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .spawn()
        .map_err(|error| spawn_error(&program, &error))?;
    dv_core::job::tie_to_this_process(&child);
    let stdout = child.stdout.take().map(collect);
    let stderr = child.stderr.take().map(collect);
    let stdin = child.stdin.take();
    let child = Arc::new(Mutex::new(child));
    let (done, finished) = mpsc::channel::<()>();
    let watchdog = {
        let child = Arc::clone(&child);
        std::thread::Builder::new()
            .name("dv-wsl-watchdog".into())
            .spawn(move || match finished.recv_timeout(timeout) {
                Err(RecvTimeoutError::Timeout) => {
                    let _ = child.lock().kill();
                    true
                }
                _ => false,
            })
            .map_err(|error| WslError::Failed(format!("couldn't start a thread: {error}")))?
    };
    let mut written = Ok(());
    if let (Some(input), Some(mut stdin)) = (input, stdin) {
        written = stream(input, &mut stdin);
        drop(stdin);
    }
    let status = loop {
        match child.lock().try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => break Err(error),
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(done);
    let timed_out = watchdog.join().unwrap_or(false);
    let status = status.map_err(|error| WslError::Failed(format!("{program}: {error}")))?;
    let output = Output {
        status,
        stdout: stdout.map(joined).unwrap_or_default(),
        stderr: stderr.map(joined).unwrap_or_default(),
    };
    if timed_out {
        return Err(WslError::Failed(format!(
            "{program} didn't finish within {} s: {}",
            timeout.as_secs(),
            words(&output)
        )));
    }
    match written {
        // A command that failed before it read all its input says why itself, and its
        // status says it failed: the pipe it closed is only a consequence.
        Err(_) if !output.status.success() => Ok(output),
        Err(error) => Err(WslError::Failed(format!(
            "writing to {program} failed ({error}): {}",
            words(&output)
        ))),
        Ok(()) => Ok(output),
    }
}

/// Copies `input` to `stdin`, reporting progress.
fn stream(input: Input<'_>, stdin: &mut impl io::Write) -> io::Result<()> {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    let mut reported = 0u64;
    (input.progress)(0, input.len);
    loop {
        let read = match input.reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        stdin.write_all(&buffer[..read])?;
        done += read as u64;
        if done - reported >= PROGRESS_STEP {
            reported = done;
            (input.progress)(done, input.len.max(done));
        }
    }
    stdin.flush()?;
    if reported != done {
        (input.progress)(done, input.len.max(done));
    }
    Ok(())
}

/// A thread reading `pipe` to its end.
fn collect(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

fn joined(thread: std::thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
    thread.join().unwrap_or_default()
}

/// A program that didn't start: `wsl.exe` missing is WSL not being installed.
fn spawn_error(program: &str, error: &io::Error) -> WslError {
    if error.kind() == io::ErrorKind::NotFound && program == WSL_EXE {
        WslError::NotInstalled(format!("{WSL_EXE} wasn't found"))
    } else {
        WslError::Failed(format!("couldn't run {program}: {error}"))
    }
}

/// What [`LAUNCH`] said first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Marker {
    /// `dv-exec <arch>`: the server follows.
    Exec(Arch),
    /// `dv-missing <arch>`: this app's build isn't installed (exit 120).
    Missing(Arch),
    /// `dv-arch <uname -m>`: no build of dv for it (exit 121).
    UnknownArch(String),
    /// `dv-no-build <arch>`: no build of this app for it (exit 122).
    NoBuild(Arch),
}

/// The marker a line is, if it is one.
pub(crate) fn marker(line: &str) -> Option<Marker> {
    let (word, rest) = line.trim().split_once(' ')?;
    let arch = || Arch::from_name(rest);
    match word {
        "dv-exec" => arch().map(Marker::Exec),
        "dv-missing" => arch().map(Marker::Missing),
        "dv-no-build" => arch().map(Marker::NoBuild),
        "dv-arch" => Some(Marker::UnknownArch(rest.trim().to_string())),
        _ => None,
    }
}

/// What a launch's stdout said before its marker, or before it ended.
struct Heard {
    /// The marker, `None` when the output ended first.
    marker: Option<Marker>,
    /// Everything else it printed (`wsl.exe`'s warnings and errors), as printed.
    stray: Vec<u8>,
}

/// Starts `shell`'s `dv backend --stdio` (module docs): installs this app's build for the
/// distro's architecture first when it is missing (or, with `reinstall`, in any case), with
/// `progress` hearing the copy, and launches again. Blocks while the distro boots; never call
/// it on a UI thread.
pub fn launch(
    shell: &Shell,
    builds: &LinuxBuilds,
    reinstall: Option<Arch>,
    progress: &dyn Fn(u64, u64),
) -> Result<Transport, WslError> {
    if builds.is_empty() {
        return Err(builds.missing(Arch::host()));
    }
    let mut installed = false;
    // What an install warned about (a link it couldn't make), which the diagnostics keep.
    let mut warnings = Vec::new();
    if let Some(arch) = reinstall {
        let build = builds.get(arch).ok_or_else(|| builds.missing(arch))?;
        warnings = install::install(shell, build, progress)?.warnings;
        installed = true;
    }
    let hashes = builds.hash_args();
    let args = [hashes[0].as_str(), hashes[1].as_str()];
    loop {
        let mut command = shell.command(LAUNCH, &args);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let program = command.get_program().to_string_lossy().into_owned();
        let mut child = command
            .spawn()
            .map_err(|error| spawn_error(&program, &error))?;
        dv_core::job::tie_to_this_process(&child);
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            end(&mut child);
            return Err(WslError::Failed(format!(
                "{program}: its pipes weren't created"
            )));
        };
        let Some((reader, heard)) = listen(stdout, START_TIMEOUT) else {
            end(&mut child);
            let stderr = decode(&joined(collect(stderr))).trim().to_string();
            let mut message = format!(
                "{} didn't start dv within {} s",
                shell.name(),
                START_TIMEOUT.as_secs()
            );
            if !stderr.is_empty() {
                message.push_str(&format!(": {stderr}"));
            }
            return Err(WslError::Failed(message));
        };
        let notes: Vec<String> = warnings
            .iter()
            .map(|warning| format!("install: {warning}"))
            .chain(
                decode(&heard.stray)
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(|line| format!("stdout: {line}")),
            )
            .collect();
        match heard.marker {
            Some(Marker::Exec(arch)) => {
                return Ok(Transport {
                    reader: Box::new(reader),
                    writer: Box::new(stdin),
                    stderr: Some(Box::new(stderr)),
                    child: Some(child),
                    build: builds.get(arch).map(|build| build.hash.clone()),
                    notes,
                });
            }
            Some(Marker::Missing(arch)) if !installed => {
                drop(stdin);
                end(&mut child);
                let build = builds.get(arch).ok_or_else(|| builds.missing(arch))?;
                warnings = install::install(shell, build, progress)?.warnings;
                installed = true;
            }
            Some(Marker::Missing(arch)) => {
                end(&mut child);
                return Err(WslError::Failed(format!(
                    "dv for {arch} is still missing in {} after installing it",
                    shell.name()
                )));
            }
            Some(Marker::UnknownArch(machine)) => {
                end(&mut child);
                return Err(WslError::UnknownArch(machine));
            }
            Some(Marker::NoBuild(arch)) => {
                end(&mut child);
                return Err(builds.missing(arch));
            }
            None => {
                drop(stdin);
                let status = wait_briefly(&mut child);
                let output = Output {
                    status: status.unwrap_or_else(failed_status),
                    stdout: heard.stray,
                    stderr: joined(collect(stderr)),
                };
                return Err(WslError::Failed(format!("{program}: {}", words(&output))));
            }
        }
    }
}

/// Reads `stdout` until a marker or its end, within `timeout`; `None` when the time ran out
/// (the caller kills the process, which ends the reading thread).
fn listen(stdout: ChildStdout, timeout: Duration) -> Option<(BufReader<ChildStdout>, Heard)> {
    let (tx, rx) = mpsc::channel();
    let started = std::thread::Builder::new()
        .name("dv-wsl-launch".into())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut heard = Heard {
                marker: None,
                stray: Vec::new(),
            };
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let text = String::from_utf8_lossy(&line).replace('\0', "");
                if let Some(found) = marker(&text) {
                    heard.marker = Some(found);
                    break;
                }
                heard.stray.extend_from_slice(&line);
            }
            let _ = tx.send((reader, heard));
        });
    if started.is_err() {
        return None;
    }
    rx.recv_timeout(timeout).ok()
}

/// Kills and reaps a launch that won't be used.
fn end(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The exit status of a launch whose output ended, waiting a little for it; killed when it
/// doesn't exit.
fn wait_briefly(child: &mut Child) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    end(child);
    child.try_wait().ok().flatten()
}

/// A status for a process whose own couldn't be read.
fn failed_status() -> std::process::ExitStatus {
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt as _;
        std::process::ExitStatus::from_raw(1)
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        std::process::ExitStatus::from_raw(1 << 8)
    }
}

#[cfg(test)]
mod tests;
