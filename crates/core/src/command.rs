//! The single choke point for running external programs against a
//! [`RepoLocation`]. Local repos run the program directly; WSL repos run it
//! as `wsl.exe -d <distro> --exec <program> <args…>`. Nothing in dv spawns a
//! repo-scoped process any other way.

use std::io::Write;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::location::RepoLocation;

/// `CREATE_NO_WINDOW` — suppresses the console flash every subprocess would
/// otherwise cause once dv is a windowed (non-console) binary.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
    pub fn run(&self, program: &str, args: &[&str]) -> Result<Vec<u8>> {
        let output = self
            .command(program, args)
            .output()
            .with_context(|| format!("failed to run {program}: spawn failed"))?;

        if !output.status.success() {
            let joined_args = args.join(" ");
            let code = match output.status.code() {
                Some(code) => code.to_string(),
                None => "terminated by signal".to_string(),
            };
            let mut stderr = decode_output(&output.stderr);
            truncate_lossy(&mut stderr, 2000);
            bail!("{program} {joined_args} failed (exit {code}): {stderr}");
        }

        Ok(output.stdout)
    }

    /// [`Self::run`] + [`decode_output`] + trailing-whitespace trim, for
    /// text-producing commands (`rev-parse`, `--name-status`, …).
    pub fn run_text(&self, program: &str, args: &[&str]) -> Result<String> {
        let bytes = self.run(program, args)?;
        Ok(decode_output(&bytes).trim_end().to_string())
    }

    /// Like [`Self::run`], but writes `stdin_bytes` to the child's stdin
    /// before collecting output. Used by the review store's WSL write path
    /// (`sh -c 'mkdir -p … && cat > tmp && mv tmp final'`), which has no
    /// other way to get bytes into the pipeline.
    ///
    /// The stdin handle is closed (dropped) before `wait_with_output`, not
    /// after: a child that consumes all of stdin before it starts writing
    /// stdout would otherwise deadlock (child blocked on a full stdout pipe
    /// no one is draining yet, us blocked on a `wait` that needs the child
    /// to exit) — same hazard `std::process::Command` docs warn about.
    pub fn run_with_stdin(
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

        if !output.status.success() {
            let joined_args = args.join(" ");
            let code = match output.status.code() {
                Some(code) => code.to_string(),
                None => "terminated by signal".to_string(),
            };
            let mut stderr = decode_output(&output.stderr);
            truncate_lossy(&mut stderr, 2000);
            bail!("{program} {joined_args} failed (exit {code}): {stderr}");
        }

        Ok(output.stdout)
    }
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
}
