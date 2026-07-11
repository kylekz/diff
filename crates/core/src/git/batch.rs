//! A persistent `git cat-file --batch` child process, used by
//! [`super::GitRepo::blob_bytes`] so blob reads don't pay a process-spawn
//! cost per file.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::command::CommandBuilder;

pub(super) struct BlobStore {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl BlobStore {
    pub(super) fn spawn(builder: &CommandBuilder, root: &str) -> Result<Self> {
        let mut cmd = builder.command("git", &["-C", root, "cat-file", "--batch"]);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());

        let mut child = cmd
            .spawn()
            .context("failed to spawn git cat-file --batch")?;
        let stdin = child
            .stdin
            .take()
            .context("git cat-file --batch: missing stdin handle")?;
        let stdout = child
            .stdout
            .take()
            .context("git cat-file --batch: missing stdout handle")?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    /// Request one object. `spec` is anything `git cat-file --batch`
    /// accepts on a line (`<rev>:<path>`, `:0:<path>`, an oid, …).
    ///
    /// Protocol: write `<spec>\n`; read one header line, either
    /// `<oid> missing` or `<oid> <type> <size>`; for a present blob, read
    /// exactly `<size>` content bytes followed by the single `\n` git
    /// always appends after the object payload.
    pub(super) fn request(&mut self, spec: &str) -> Result<Option<Vec<u8>>> {
        self.stdin.write_all(spec.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        let mut header = String::new();
        let read = self.stdout.read_line(&mut header)?;
        if read == 0 {
            bail!("git cat-file --batch: unexpected EOF reading header for {spec}");
        }
        let header = header.trim_end_matches(['\n', '\r']);

        if header.ends_with(" missing") {
            return Ok(None);
        }

        let mut parts = header.splitn(3, ' ');
        let _oid = parts
            .next()
            .ok_or_else(|| anyhow!("git cat-file --batch: malformed header {header:?}"))?;
        let obj_type = parts
            .next()
            .ok_or_else(|| anyhow!("git cat-file --batch: malformed header {header:?}"))?;
        let size: usize = parts
            .next()
            .ok_or_else(|| anyhow!("git cat-file --batch: malformed header {header:?}"))?
            .parse()
            .map_err(|_| anyhow!("git cat-file --batch: malformed size in header {header:?}"))?;

        if obj_type != "blob" {
            let mut drain = vec![0u8; size + 1];
            self.stdout.read_exact(&mut drain)?;
            bail!("git cat-file --batch: {spec} is not a blob (type {obj_type})");
        }

        let mut content = vec![0u8; size];
        self.stdout.read_exact(&mut content)?;
        let mut trailing_newline = [0u8; 1];
        self.stdout.read_exact(&mut trailing_newline)?;

        Ok(Some(content))
    }
}

impl Drop for BlobStore {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
