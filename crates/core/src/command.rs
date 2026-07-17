//! The single choke point for running external programs against a
//! [`RepoLocation`]. Local repos run the program directly; WSL repos run it
//! as `wsl.exe -d <distro> --exec <program> <args…>`. Nothing in dv spawns a
//! repo-scoped process any other way.

use std::io::{Read, Write};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::location::RepoLocation;
use crate::remote::client::{HostClient, RequestFailure};
use crate::remote::manager;

/// `CREATE_NO_WINDOW` — suppresses the console flash every subprocess would
/// otherwise cause once dv is a windowed (non-console) binary.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Where a command actually runs (docs/phase-5-implementation-plan.md §3):
/// either the Stage-A `wsl.exe -d <distro> --exec` spawn every command has
/// always used, or a live `dv-host` connection for this location's distro.
/// Decided once, in [`CommandBuilder::new`] — nothing downstream of that
/// (`run`/`run_text`/`run_with_stdin`) has a different signature depending
/// on which arm is live.
#[derive(Clone)]
enum Route {
    Spawn,
    Host(Arc<HostClient>),
}

impl std::fmt::Debug for Route {
    // Manual impl: `HostClient` doesn't (and shouldn't) implement `Debug`,
    // so deriving here isn't an option.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Route::Spawn => f.write_str("Route::Spawn"),
            Route::Host(client) => write!(f, "Route::Host(pid={})", client.pid()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandBuilder {
    location: RepoLocation,
    route: Route,
}

impl CommandBuilder {
    /// Wsl locations consult [`manager::client_for`] for a live host
    /// connection; anything it doesn't hand back (disabled, no
    /// `DV_HOST_PATH`, cooling down after a failure, ...) falls back to
    /// `Route::Spawn` — the existing, always-correct behavior. Local
    /// locations never consult the manager at all.
    pub fn new(location: RepoLocation) -> Self {
        let route = match &location {
            RepoLocation::Wsl { distro, .. } => manager::client_for(distro)
                .map(Route::Host)
                .unwrap_or(Route::Spawn),
            RepoLocation::Local(_) => Route::Spawn,
        };
        Self { location, route }
    }

    /// Force this builder to route every call through an already-connected
    /// host client, bypassing [`manager::client_for`] entirely. Real,
    /// non-test API (a caller that already holds a client and wants to use
    /// it directly), but its primary consumer today is the cross-process
    /// test in `crates/host/tests/` that proves the Host and Spawn arms
    /// produce byte-identical error text for the same failing command —
    /// that test spins up a REAL `dv-host` binary locally (not via
    /// `wsl.exe`) and has no `RepoLocation::Wsl` to route through
    /// `client_for` with.
    pub fn with_host(location: RepoLocation, client: Arc<HostClient>) -> Self {
        Self {
            location,
            route: Route::Host(client),
        }
    }

    /// Force `Route::Spawn`, never consulting [`manager::client_for`] — the
    /// seam [`crate::remote::install`] uses for its bootstrap commands
    /// (checking/streaming the sidecar) before any `dv-host` connection
    /// exists to route through. This is load-bearing, not just a
    /// convenience: `client_for` holds a per-distro lock across its whole
    /// spawn attempt (see that function's doc comment), and
    /// `install::ensure_installed` runs INSIDE that attempt — a
    /// `CommandBuilder::new` call from there would re-enter `client_for` for
    /// the same distro on the same thread and deadlock on its own
    /// (non-reentrant) `Mutex`.
    pub(crate) fn new_spawn_only(location: RepoLocation) -> Self {
        Self {
            location,
            route: Route::Spawn,
        }
    }

    pub fn location(&self) -> &RepoLocation {
        &self.location
    }

    /// The live host client this builder routes through, if any — consulted
    /// by [`crate::git::GitRepo::batch_request`] to pick between `blob/get`
    /// and the local `BlobStore` child.
    pub(crate) fn host_client(&self) -> Option<Arc<HostClient>> {
        match &self.route {
            Route::Host(client) => Some(Arc::clone(client)),
            Route::Spawn => None,
        }
    }

    /// Construct (but do not run) a [`Command`] for `program` with `args`,
    /// routed through `wsl.exe -d <distro> --exec` for WSL locations.
    ///
    /// On Windows the command must carry `CREATE_NO_WINDOW` (0x0800_0000)
    /// via `CommandExt::creation_flags`, or every git call flashes a console
    /// window once dv is a windowed (non-console) binary.
    pub fn command(&self, program: &str, args: &[&str]) -> Command {
        // `mut` is only exercised by the cfg(windows) block below; keep
        // non-Windows clippy (the macOS CI job) quiet about it.
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut cmd = match &self.location {
            RepoLocation::Local(_) => {
                let mut cmd = Command::new(program);
                cmd.args(args);
                cmd
            }
            RepoLocation::Wsl { distro, .. } => {
                let mut cmd = Command::new("wsl.exe");
                // --exec, never --: the -- form launches through the login
                // shell, which expands $(…), `…`, $VAR, and globs in argv —
                // a repo path or working-tree FILENAME containing metachars
                // would be mis-resolved or, worse, executed.
                cmd.args(["-d", distro, "--exec", program]);
                cmd.args(args);
                cmd
            }
        };
        #[cfg(windows)]
        {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd
    }

    /// Run to completion, capturing output. Non-zero exit is an `Err`
    /// carrying the program, args, exit code, and (decoded, truncated)
    /// stderr. Stdout is returned as raw bytes, untouched — blob content
    /// must never pass through text decoding.
    ///
    /// For a `Route::Host` builder this sends `proc/exec` instead of
    /// spawning `wsl.exe`. Only a CONNECTION-level host failure (the
    /// client channel itself is dead — see
    /// [`RequestFailure::is_connection`]) is hidden from the caller: it's
    /// logged and this call transparently falls back to the Spawn arm,
    /// which RE-EXECUTES `program` from scratch. A `Timeout` surfaces as an
    /// `Err` instead — the host-side command is almost certainly still
    /// running, so falling back would run it a SECOND time, concurrently
    /// with the still-in-flight original (ref-lock contention on a fetch,
    /// e.g.). An `Rpc` error (the host answered with a structured failure —
    /// its own spawn of `program` failing, say) is likewise surfaced as-is:
    /// a completed round trip with a definitive result, exactly as final as
    /// a successful response with a non-zero exit code.
    ///
    /// The real contract this implies: every command that flows through
    /// `CommandBuilder` must tolerate being RE-EXECUTED after a connection
    /// failure, because that's the one case that transparently retries.
    /// Today's inventory qualifies — reads, convergent fetches (`git fetch`
    /// of a ref that already matches is a no-op), and same-bytes atomic
    /// writes (the review store's WSL write path: write-to-tmp + rename,
    /// safe to redo) — reviewed 2026-07-12. Anyone adding a non-idempotent
    /// command (anything that appends, increments, or has a side effect
    /// outside the repo) through this layer must revisit this contract.
    pub fn run(&self, program: &str, args: &[&str]) -> Result<Vec<u8>> {
        if let Route::Host(client) = &self.route {
            match client.exec(program, args, None) {
                Ok(outcome) => {
                    return finish(
                        program,
                        args,
                        outcome.exit_code == 0,
                        Some(outcome.exit_code),
                        outcome.stdout,
                        &outcome.stderr,
                    );
                }
                Err(err) if RequestFailure::is_connection_failure(&err) => {
                    self.note_host_failure(client, &format!("proc/exec: {err:#}"));
                }
                Err(err) => return Err(err),
            }
        } else {
            manager::note_spawn_fallback(&self.location, "no host route available");
        }
        self.run_spawn(program, args)
    }

    /// [`Self::run`] + [`decode_output`] + trailing-whitespace trim, for
    /// text-producing commands (`rev-parse`, `--name-status`, …).
    pub fn run_text(&self, program: &str, args: &[&str]) -> Result<String> {
        let bytes = self.run(program, args)?;
        Ok(decode_output(&bytes).trim_end().to_string())
    }

    /// Like [`Self::run`], but forces the C locale on WSL children by
    /// prefixing `env LC_ALL=C` at the argv level — which works identically
    /// for the `wsl.exe --exec` spawn route and the dv-host `proc/exec`
    /// route, since environment variables can't otherwise cross either
    /// boundary. Local children run unmodified (git for Windows and the
    /// bundled coreutils are English unless the user opted into a locale;
    /// WSL distros routinely aren't).
    ///
    /// For callers that classify failures by matching English error text —
    /// `"No such file or directory"`, `"Is a directory"` — a non-English
    /// distro locale would otherwise localize the message and turn a benign
    /// missing-file result into a surfaced error (docs/backlog.md blob_sha
    /// locale item).
    pub fn run_c_locale(&self, program: &str, args: &[&str]) -> Result<Vec<u8>> {
        match &self.location {
            RepoLocation::Local(_) => self.run(program, args),
            RepoLocation::Wsl { .. } => {
                let mut full: Vec<&str> = Vec::with_capacity(args.len() + 2);
                full.push("LC_ALL=C");
                full.push(program);
                full.extend_from_slice(args);
                self.run("env", &full)
            }
        }
    }

    /// [`Self::run_c_locale`] + [`decode_output`] + trim — the text sibling,
    /// mirroring [`Self::run_text`].
    pub fn run_text_c_locale(&self, program: &str, args: &[&str]) -> Result<String> {
        let bytes = self.run_c_locale(program, args)?;
        Ok(decode_output(&bytes).trim_end().to_string())
    }

    /// Like [`Self::run`], but writes `stdin_bytes` to the child's stdin
    /// before collecting output. Used by the review store's WSL write path
    /// (`sh -c 'mkdir -p … && cat > tmp && mv tmp final'`), which has no
    /// other way to get bytes into the pipeline. Routes through
    /// `Route::Host` the same way [`Self::run`] does — including the same
    /// "only a Connection-kind failure falls back; Timeout/Rpc surface
    /// as-is" contract documented there.
    pub fn run_with_stdin(
        &self,
        program: &str,
        args: &[&str],
        stdin_bytes: &[u8],
    ) -> Result<Vec<u8>> {
        if let Route::Host(client) = &self.route {
            match client.exec(program, args, Some(stdin_bytes)) {
                Ok(outcome) => {
                    return finish(
                        program,
                        args,
                        outcome.exit_code == 0,
                        Some(outcome.exit_code),
                        outcome.stdout,
                        &outcome.stderr,
                    );
                }
                Err(err) if RequestFailure::is_connection_failure(&err) => {
                    self.note_host_failure(client, &format!("proc/exec: {err:#}"));
                }
                Err(err) => return Err(err),
            }
        } else {
            manager::note_spawn_fallback(&self.location, "no host route available");
        }
        self.run_with_stdin_spawn(program, args, stdin_bytes)
    }

    /// A live `Route::Host` request just failed with a CONNECTION-level
    /// error — callers must never call this for a `Timeout` or an `Rpc`
    /// result the host successfully answered with (see [`RequestFailure`]
    /// and [`RequestFailure::is_connection_failure`], which both call sites
    /// guard on before reaching here). Thin wrapper around
    /// [`manager::note_host_connection_lost`], which centralizes the
    /// logging-with-stderr-context + cool-down-on-`Wsl` behavior this and
    /// `GitRepo::batch_request`'s Host arm both need.
    fn note_host_failure(&self, client: &Arc<HostClient>, reason: &str) {
        manager::note_host_connection_lost(&self.location, client, reason);
    }

    /// The Stage-A path: build the `wsl.exe`-prefixed (or bare, for Local)
    /// [`Command`] and run it directly. Shared error formatting with the
    /// Host arm lives in [`finish`], not here — see its doc comment.
    fn run_spawn(&self, program: &str, args: &[&str]) -> Result<Vec<u8>> {
        let output = self
            .command(program, args)
            .output()
            .with_context(|| format!("failed to run {program}: spawn failed"))?;
        finish(
            program,
            args,
            output.status.success(),
            output.status.code(),
            output.stdout,
            &output.stderr,
        )
    }

    /// The Stage-A path for [`Self::run_with_stdin`].
    ///
    /// The stdin handle is closed (dropped) before `wait_with_output`, not
    /// after: a child that consumes all of stdin before it starts writing
    /// stdout would otherwise deadlock (child blocked on a full stdout pipe
    /// no one is draining yet, us blocked on a `wait` that needs the child
    /// to exit) — same hazard `std::process::Command` docs warn about.
    fn run_with_stdin_spawn(
        &self,
        program: &str,
        args: &[&str],
        stdin_bytes: &[u8],
    ) -> Result<Vec<u8>> {
        let mut cmd = self.command(program, args);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to run {program}: spawn failed"))?;

        {
            let mut stdin = child
                .stdin
                .take()
                .with_context(|| format!("{program}: missing stdin handle"))?;
            stdin
                .write_all(stdin_bytes)
                .with_context(|| format!("failed writing to {program} stdin"))?;
        } // drop closes the pipe, signalling EOF to the child

        let output = child
            .wait_with_output()
            .with_context(|| format!("failed waiting for {program}"))?;
        finish(
            program,
            args,
            output.status.success(),
            output.status.code(),
            output.stdout,
            &output.stderr,
        )
    }

    /// Spawn-arm run with a hard wall-clock `timeout`: a wedged `wsl.exe`
    /// (stuck distro boot) is killed and reported instead of blocking
    /// forever. Used only by install bootstrap commands, which run under
    /// `manager::client_for`'s per-distro lock — an unbounded wait there
    /// wedges every future `client_for` for that distro. stdin/stdout/stderr
    /// are drained on their own threads (a full pipe must never deadlock the
    /// wait), mirroring `dv-host`'s handle_exec; completion is polled via
    /// `try_wait` on a deadline, the same shape `HostClient`'s Drop already
    /// uses.
    fn run_spawn_bounded(
        &self,
        program: &str,
        args: &[&str],
        stdin_bytes: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let mut cmd = self.command(program, args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to run {program}: spawn failed"))?;

        // Write stdin on ITS OWN detached thread, never the current one: a
        // wedged child that never drains stdin would otherwise block this
        // write and bypass the timeout below entirely (we'd be stuck here,
        // not even reached the `try_wait` poll loop yet).
        if let Some(bytes) = stdin_bytes {
            let mut stdin = child
                .stdin
                .take()
                .with_context(|| format!("{program}: missing stdin handle"))?;
            let bytes = bytes.to_vec();
            std::thread::spawn(move || {
                // A child that exits (or gets killed) before consuming all
                // of stdin closes its read end — the resulting broken-pipe
                // write error is expected and not separately actionable, so
                // it's dropped here rather than surfaced.
                let _ = stdin.write_all(&bytes);
            });
        }

        // Drain stdout/stderr on their own threads too: a full pipe must
        // never deadlock the `try_wait` poll loop below (the child would
        // block writing to a pipe nobody's reading, and we'd block waiting
        // for it to exit — classic pipe deadlock).
        let mut stdout_pipe = child
            .stdout
            .take()
            .with_context(|| format!("{program}: missing stdout handle"))?;
        let stdout_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buf);
            buf
        });
        let mut stderr_pipe = child
            .stderr
            .take()
            .with_context(|| format!("{program}: missing stderr handle"))?;
        let stderr_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf);
            buf
        });

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) => {
                    // Deadline exceeded: kill and report rather than let a
                    // wedged `wsl.exe` block the caller (and, transitively,
                    // every future `client_for` for this distro) forever.
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "{program} timed out after {}s — the WSL distro may be wedged or slow to boot",
                        timeout.as_secs()
                    );
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("failed waiting for {program}"));
                }
            }
        };

        // The child has exited (or been killed above, which already
        // returned) — its pipes are closed, so both reader threads are
        // guaranteed to unblock and finish on their own now.
        let stdout = stdout_thread.join().unwrap_or_default();
        let stderr = stderr_thread.join().unwrap_or_default();

        finish(
            program,
            args,
            status.success(),
            status.code(),
            stdout,
            &stderr,
        )
    }

    /// [`Self::run_spawn_bounded`] with no stdin, for text/byte-producing
    /// bootstrap commands.
    pub(crate) fn run_timeout(
        &self,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        self.run_spawn_bounded(program, args, None, timeout)
    }

    /// [`Self::run_timeout`] + [`decode_output`] + trailing-whitespace trim —
    /// the bounded counterpart of [`Self::run_text`].
    pub(crate) fn run_text_timeout(
        &self,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<String> {
        let bytes = self.run_timeout(program, args, timeout)?;
        Ok(decode_output(&bytes).trim_end().to_string())
    }

    /// [`Self::run_spawn_bounded`] with `stdin_bytes` piped in — the bounded
    /// counterpart of [`Self::run_with_stdin`].
    pub(crate) fn run_with_stdin_timeout(
        &self,
        program: &str,
        args: &[&str],
        stdin_bytes: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        self.run_spawn_bounded(program, args, Some(stdin_bytes), timeout)
    }
}

/// Shared success/failure formatting for BOTH routes — the single reason
/// `CommandBuilder::run`'s error text is byte-identical whether it ran via
/// `wsl.exe` spawn or a `dv-host` `proc/exec` response (plan §8 S2: "the
/// error strings on non-zero exit must be byte-identical to today's
/// format"). `code` is `Option` to preserve the Spawn arm's existing
/// "terminated by signal" text for a `None` `ExitStatus::code()`; the Host
/// arm always has a definite `i32` (the wire's `ExecResult::exit_code`), so
/// it always passes `Some`.
fn finish(
    program: &str,
    args: &[&str],
    success: bool,
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: &[u8],
) -> Result<Vec<u8>> {
    if success {
        return Ok(stdout);
    }
    let joined_args = args.join(" ");
    let code_str = match code {
        Some(code) => code.to_string(),
        None => "terminated by signal".to_string(),
    };
    let mut stderr_text = decode_output(stderr);
    truncate_lossy(&mut stderr_text, 2000);
    bail!("{program} {joined_args} failed (exit {code_str}): {stderr_text}");
}

/// Truncate `s` to at most `max` bytes, backing up to the nearest
/// preceding UTF-8 character boundary so a multi-byte character straddling
/// `max` isn't split — plain `String::truncate(max)` panics in that case.
/// Shared by [`CommandBuilder`]'s own error paths and
/// [`crate::github::client`]'s `classify_failure`.
pub(crate) fn truncate_lossy(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut boundary = max;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    s.truncate(boundary);
}

/// Decode process output that may be UTF-8 **or** UTF-16LE.
///
/// `wsl.exe` passes child-process output through as raw bytes (UTF-8 for
/// git), but its *own* messages — errors like "no such distribution", and
/// the output of `wsl.exe --list` — are UTF-16LE, sometimes without a BOM.
/// Heuristic: FF FE BOM → UTF-16LE; else if the buffer contains NUL bytes
/// in an every-other-byte pattern → UTF-16LE; else UTF-8 (lossy).
pub fn decode_output(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        return decode_utf16le(&bytes[2..]);
    }

    // Heuristic for BOM-less UTF-16LE: ASCII text in that encoding has a
    // NUL in every other byte, so a NUL density above 1/4 is a strong
    // signal (real UTF-8 text essentially never contains NUL).
    if bytes.len() >= 4 {
        let zero_count = bytes.iter().filter(|&&b| b == 0).count();
        if zero_count > bytes.len() / 4 {
            return decode_utf16le(bytes);
        }
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16le(bytes: &[u8]) -> String {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    let mut chunks = bytes.chunks_exact(2);
    for chunk in &mut chunks {
        units.push(u16::from_le_bytes([chunk[0], chunk[1]]));
    }
    // An odd trailing byte can't form a code unit; drop it.
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::RepoLocation;
    use std::path::PathBuf;

    #[test]
    fn u3_decode_output_utf8() {
        assert_eq!(decode_output(b"hello world"), "hello world");
    }

    #[test]
    fn u3_decode_output_utf16le_with_bom() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "Ubuntu".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_output(&bytes), "Ubuntu");
    }

    #[test]
    fn u3_decode_output_utf16le_without_bom() {
        let text = "Ubuntu\r\n";
        let mut bytes = Vec::new();
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_output(&bytes), text);
    }

    #[test]
    fn u3_decode_output_invalid_utf8_is_lossy() {
        let bytes = [0xFF, 0x00, 0x11, 0x22, 0x33];
        let decoded = decode_output(&bytes);
        assert!(decoded.contains('\u{FFFD}'));
    }

    #[test]
    fn u4_command_assembly_for_wsl() {
        let builder = CommandBuilder::new(RepoLocation::Wsl {
            distro: "Ubuntu".to_string(),
            path: "/x".to_string(),
        });
        let cmd = builder.command("git", &["-C", "/x", "status"]);
        assert_eq!(cmd.get_program(), "wsl.exe");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            ["-d", "Ubuntu", "--exec", "git", "-C", "/x", "status"]
        );
    }

    #[test]
    fn truncate_lossy_backs_up_to_char_boundary() {
        // Each "中" is 3 bytes in UTF-8, so byte offset 2000 (not a
        // multiple of 3) falls mid-character; naively truncating there
        // would panic `String::truncate`.
        let mut s = "中".repeat(1000); // 3000 bytes, 1000 chars
        truncate_lossy(&mut s, 2000);
        assert_eq!(s.len(), 1998); // nearest char boundary <= 2000
        assert!(s.chars().all(|c| c == '中'));
    }

    #[test]
    fn truncate_lossy_is_a_noop_under_the_limit() {
        let mut s = "short".to_string();
        truncate_lossy(&mut s, 2000);
        assert_eq!(s, "short");
    }

    #[test]
    fn u4_command_assembly_for_local() {
        let builder = CommandBuilder::new(RepoLocation::Local(PathBuf::from("D:\\x")));
        let cmd = builder.command("git", &["-C", "D:\\x", "status"]);
        assert_eq!(cmd.get_program(), "git");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args, ["-C", "D:\\x", "status"]);
    }

    // --- Route selection (plan §8 S2: "Route selection logic
    // (enabled/disabled/... states)") ------------------------------------
    //
    // Nothing in this crate's test suite ever calls the real, process-
    // global `manager::enable_hosts()` (that's load-bearing: it lets every
    // test here rely on hosts being ambiently disabled, with no ordering
    // hazard against other tests in the same binary). So a fresh
    // `CommandBuilder` for a Wsl location always resolves through
    // `manager::client_for`'s disabled-by-default path and lands on
    // `Route::Spawn` here, exactly like it would in the real headless CLI
    // (which never calls `enable_hosts()` either).

    #[test]
    fn route_for_local_is_always_spawn() {
        let builder = CommandBuilder::new(RepoLocation::Local(PathBuf::from("D:\\x")));
        assert!(matches!(builder.route, Route::Spawn));
        assert!(builder.host_client().is_none());
    }

    #[test]
    fn route_for_wsl_falls_back_to_spawn_when_hosts_disabled() {
        let builder = CommandBuilder::new(RepoLocation::Wsl {
            distro: "Ubuntu".to_string(),
            path: "/x".to_string(),
        });
        assert!(matches!(builder.route, Route::Spawn));
        assert!(builder.host_client().is_none());
    }

    #[test]
    fn new_spawn_only_is_always_spawn_even_for_a_wsl_location() {
        // Unlike `CommandBuilder::new`, this must never touch
        // `manager::client_for` at all — see this constructor's doc comment
        // for why (install.rs calls it from inside `client_for`'s own
        // per-distro lock, so re-entering would deadlock).
        let builder = CommandBuilder::new_spawn_only(RepoLocation::Wsl {
            distro: "Ubuntu".to_string(),
            path: "/x".to_string(),
        });
        assert!(matches!(builder.route, Route::Spawn));
        assert!(builder.host_client().is_none());
    }

    #[test]
    fn route_debug_does_not_require_host_client_debug() {
        // Compiles at all only because `Route`'s `Debug` impl is manual —
        // this is mostly a "does it build" assertion.
        let builder = CommandBuilder::new(RepoLocation::Local(PathBuf::from("D:\\x")));
        assert_eq!(format!("{:?}", builder.route), "Route::Spawn");
    }

    // --- finish(): the shared Spawn/Host error formatter -----------------

    #[test]
    fn finish_success_returns_stdout_untouched() {
        let out = finish("git", &["status"], true, Some(0), b"ok".to_vec(), b"").unwrap();
        assert_eq!(out, b"ok");
    }

    #[test]
    fn finish_failure_format_matches_documented_shape() {
        let err = finish(
            "git",
            &["-C", "/x", "status"],
            false,
            Some(128),
            Vec::new(),
            b"fatal: not a git repository",
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "git -C /x status failed (exit 128): fatal: not a git repository"
        );
    }

    #[test]
    fn finish_none_code_reads_terminated_by_signal() {
        let err = finish("git", &["status"], false, None, Vec::new(), b"").unwrap_err();
        assert!(err.to_string().contains("terminated by signal"));
    }

    #[test]
    fn finish_truncates_and_decodes_stderr_identically_to_the_old_inline_logic() {
        let long_stderr = "e".repeat(3000);
        let err = finish(
            "git",
            &["status"],
            false,
            Some(1),
            Vec::new(),
            long_stderr.as_bytes(),
        )
        .unwrap_err();
        let text = err.to_string();
        // 2000-byte cap (see `truncate_lossy`) plus the surrounding format.
        assert!(text.len() < long_stderr.len());
        assert!(text.contains("failed (exit 1):"));
    }

    // --- run_spawn_bounded: the install-bootstrap timeout (plan §5 / §8 S5)

    // The test binary itself always runs on Windows regardless of what
    // platform dv targets at runtime, so a portable "sleep longer than the
    // timeout" child needs a Windows-native command — `cmd /c "ping
    // 127.0.0.1 -n 3 >NUL"` burns roughly 2s (three ICMP echoes, one per
    // second, one of them skipped) with no external dependencies.
    #[cfg(windows)]
    #[test]
    fn run_spawn_bounded_times_out_on_a_wedged_child() {
        let builder = CommandBuilder::new(RepoLocation::Local(PathBuf::from(".")));
        let start = Instant::now();
        let err = builder
            .run_spawn_bounded(
                "cmd",
                &["/c", "ping 127.0.0.1 -n 3 >NUL"],
                None,
                Duration::from_millis(200),
            )
            .unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            err.to_string().contains("timed out"),
            "expected a timeout error, got: {err}"
        );
        // The whole point of the bounded wait: return close to the 200ms
        // timeout, nowhere near the ~2s the child would otherwise take.
        assert!(
            elapsed < Duration::from_secs(1),
            "expected an early return near the 200ms timeout, took {elapsed:?}"
        );
    }
}
