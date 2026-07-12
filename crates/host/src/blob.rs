//! A `git cat-file --batch` pool serving `blob/get` requests, one
//! persistent child per repo root. Hand-written rather than shared with
//! `dv_core::git::batch::BlobStore` — this crate deliberately avoids a
//! dv-core dependency (see `src/main.rs`'s module doc) — but the wire-level
//! contract (missing-object handling, respawn-on-any-error) mirrors it
//! exactly, since `crates/core/src/git/batch.rs`'s own respawn logic is
//! what this module is required to match (docs/phase-5-implementation-plan.md
//! §8 S2).

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

/// One live (or formerly live) `git -C <root> cat-file --batch` child.
/// dv-host runs INSIDE the distro already, so unlike dv-core's
/// `CommandBuilder`-routed `BlobStore::spawn`, this is always a plain local
/// spawn — no `wsl.exe` prefixing needed.
struct CatFileChild {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl CatFileChild {
    fn spawn(root: &str) -> anyhow::Result<Self> {
        let mut cmd = Command::new("git");
        cmd.args(["-C", root, "cat-file", "--batch"]);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());

        let mut child = cmd.spawn().map_err(|err| {
            anyhow::anyhow!("failed to spawn git cat-file --batch for {root}: {err}")
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("git cat-file --batch: missing stdin handle"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("git cat-file --batch: missing stdout handle"))?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    /// Mirrors `dv_core::git::batch::BlobStore::request`'s protocol exactly:
    /// write `<spec>\n`; read one header line, either `<oid> missing` or
    /// `<oid> <type> <size>`; for a present blob, read exactly `<size>`
    /// content bytes followed by the single `\n` git always appends.
    fn request(&mut self, spec: &str) -> anyhow::Result<Option<Vec<u8>>> {
        self.stdin.write_all(spec.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        let mut header = String::new();
        let read = self.stdout.read_line(&mut header)?;
        if read == 0 {
            anyhow::bail!("git cat-file --batch: unexpected EOF reading header for {spec:?}");
        }
        let header = header.trim_end_matches(['\n', '\r']);

        if header.ends_with(" missing") {
            return Ok(None);
        }

        let mut parts = header.splitn(3, ' ');
        let _oid = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("git cat-file --batch: malformed header {header:?}"))?;
        let obj_type = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("git cat-file --batch: malformed header {header:?}"))?;
        let size: usize = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("git cat-file --batch: malformed header {header:?}"))?
            .parse()
            .map_err(|_| {
                anyhow::anyhow!("git cat-file --batch: malformed size in header {header:?}")
            })?;

        if obj_type != "blob" {
            let mut drain = vec![0u8; size + 1];
            self.stdout.read_exact(&mut drain)?;
            anyhow::bail!("git cat-file --batch: {spec:?} is not a blob (type {obj_type})");
        }

        let mut content = vec![0u8; size];
        self.stdout.read_exact(&mut content)?;
        let mut trailing_newline = [0u8; 1];
        self.stdout.read_exact(&mut trailing_newline)?;
        Ok(Some(content))
    }
}

impl Drop for CatFileChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Slot = Arc<Mutex<Option<CatFileChild>>>;

/// `root -> Slot`. The outer map mutex is only held long enough to
/// get-or-insert a root's slot; the (possibly first-ever, spawn-a-child)
/// work happens under the per-root mutex only — same no-thundering-herd
/// shape as `dv_core::remote::manager`'s per-distro registry.
fn pool() -> &'static Mutex<HashMap<String, Slot>> {
    static POOL: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Serve one `blob/get` request: `(found, bytes)` (`bytes` empty when
/// `!found`). Spawns a `cat-file --batch` child for `root` on first use; ANY
/// error from that child (protocol desync, EOF, wrong object type, ...)
/// drops it — the next call for the same root respawns rather than
/// retrying inline, mirroring `GitRepo::batch_request`'s existing local
/// respawn-on-error behavior byte for byte.
pub fn get(root: &str, spec: &str) -> anyhow::Result<(bool, Vec<u8>)> {
    let slot: Slot = {
        pool()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(root.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());

    if guard.is_none() {
        *guard = Some(CatFileChild::spawn(root)?);
    }

    let child = guard.as_mut().expect("just populated above");
    match child.request(spec) {
        Ok(Some(bytes)) => Ok((true, bytes)),
        Ok(None) => Ok((false, Vec::new())),
        Err(err) => {
            *guard = None;
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "dv-host-blob-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    fn git_stdout(args: &[&str], dir: &std::path::Path) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn commit_file(dir: &std::path::Path, name: &str, content: &[u8]) {
        std::fs::write(dir.join(name), content).unwrap();
        assert!(
            Command::new("git")
                .args(["add", name])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args([
                    "-c",
                    "user.email=t@t.com",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-q",
                    "-m",
                    "seed",
                ])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn found_by_rev_path_and_missing_spec_reports_not_found() {
        let dir = temp_repo("basic");
        commit_file(&dir, "a.txt", b"hello world\n");
        let root = dir.to_str().unwrap();

        let (found, bytes) = get(root, "HEAD:a.txt").unwrap();
        assert!(found);
        assert_eq!(bytes, b"hello world\n");

        let (found, bytes) = get(root, "HEAD:does-not-exist.txt").unwrap();
        assert!(!found);
        assert!(bytes.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn found_binary_blob_by_raw_oid_byte_identical() {
        let dir = temp_repo("binary");
        let mut bytes: Vec<u8> = (0..=255u8).collect();
        for i in 0..4096u32 {
            bytes.push(i.wrapping_mul(2_654_435_761).wrapping_add(i) as u8);
        }
        std::fs::write(dir.join("bin.dat"), &bytes).unwrap();
        let oid = git_stdout(&["hash-object", "-w", "--", "bin.dat"], &dir);

        let (found, got) = get(dir.to_str().unwrap(), &oid).unwrap();
        assert!(found);
        assert_eq!(got, bytes, "binary blob must round-trip byte-identical");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrong_type_error_drops_child_and_next_request_respawns() {
        let dir = temp_repo("respawn");
        commit_file(&dir, "a.txt", b"content\n");
        let root = dir.to_str().unwrap();

        // A tree is a valid cat-file spec but not a blob — `request()` must
        // error, and the pool must drop+respawn rather than wedge the pipe
        // framing for the NEXT request.
        let err = get(root, "HEAD^{tree}").unwrap_err();
        assert!(err.to_string().contains("not a blob"), "{err}");

        let (found, bytes) = get(root, "HEAD:a.txt").unwrap();
        assert!(found, "pool must have respawned a working child");
        assert_eq!(bytes, b"content\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_root_directory_errors_without_poisoning_other_roots() {
        let good = temp_repo("good-root");
        commit_file(&good, "a.txt", b"fine\n");

        let bad_root = good.join("definitely-does-not-exist");
        let err = get(bad_root.to_str().unwrap(), "HEAD:a.txt");
        assert!(err.is_err());

        // A different (valid) root must be unaffected.
        let (found, bytes) = get(good.to_str().unwrap(), "HEAD:a.txt").unwrap();
        assert!(found);
        assert_eq!(bytes, b"fine\n");

        std::fs::remove_dir_all(&good).ok();
    }
}
