//! Repository access. All operations shell out to `git` through
//! [`CommandBuilder`] — see CLAUDE.md architecture principle #1.

mod batch;

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};

use crate::command::CommandBuilder;
use crate::location::RepoLocation;
use crate::remote::client::RequestFailure;
use crate::remote::manager;
use batch::BlobStore;

/// What two states a diff compares. Maps 1:1 onto git invocations — see
/// `changed_files` for the exact command lines.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DiffSource {
    /// Working tree (incl. unstaged) vs HEAD: the pre-commit review case.
    WorkingTree,
    /// Index vs HEAD.
    Staged,
    /// `base..head`, or `base...head` (merge-base) when `merge_base` — the
    /// latter matches GitHub's PR "Files changed" view.
    Range {
        base: String,
        head: String,
        merge_base: bool,
    },
    /// A single commit vs its parent(s); works for root commits too.
    Commit(String),
}

/// Parse a `--range` value (`<a>..<b>` or `<a>...<b>`, the latter resolving
/// against the merge base) into a [`DiffSource::Range`]. Shared by the GUI's
/// own `--range` flag (`crates/app/src/main.rs::parse_args`) and `dv-cli`'s
/// `review create --range` (`crates/cli/src/lib.rs`) — both need the exact
/// same `a..b` / `a...b` parsing, and this returns a `DiffSource`, so
/// dv-core (not either CLI-shaped crate) is its natural home (Phase 8, S8b).
pub fn parse_range(value: &str) -> Result<DiffSource, String> {
    let (base, head, merge_base) = if let Some((b, h)) = value.split_once("...") {
        (b, h, true)
    } else if let Some((b, h)) = value.split_once("..") {
        (b, h, false)
    } else {
        return Err(format!(
            "--range expects <a>..<b> or <a>...<b>, got: {value}"
        ));
    };
    if base.is_empty() || head.is_empty() {
        return Err(format!("--range endpoints must be non-empty: {value}"));
    }
    Ok(DiffSource::Range {
        base: base.to_string(),
        head: head.to_string(),
        merge_base,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
    /// Anything else git emits (`X`, `B`) — surfaced, not hidden.
    Unknown(char),
}

/// Whole-diff insertion/deletion totals for one diff source — the sidebar
/// card's `+N −N` summary (R2). Serde-derived because
/// it's persisted inside [`crate::index::IndexEntry`]'s cache file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiffTotals {
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// Path of the file on the "new" side (for deletes: the old side).
    /// Repo-root-relative with forward slashes, exactly as git prints it.
    pub path: String,
    /// The "old" side path when it differs — renames and copies.
    pub old_path: Option<String>,
    pub status: ChangeStatus,
}

/// Which blob (one *side* of one file) to load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobSpec {
    /// `<rev>:<path>` — content at a committed revision.
    Rev { rev: String, path: String },
    /// `:0:<path>` — content in the index (stage 0).
    Index { path: String },
    /// Content in the working tree. Local: filesystem read. WSL: routed
    /// through the command layer (`cat`) — **never** a `\\wsl.localhost\`
    /// read (the no-9P rule, docs/phase-1-diff-viewer.md).
    Working { path: String },
}

/// `git hash-object -t tree /dev/null` for a SHA-1 repo — the well-known
/// empty-tree oid, stable across all git installs.
const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// Same, for a SHA-256 repo (`--object-format=sha256`).
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

/// An opened repository. Cheap to clone conceptually — hold it in an `Arc`.
pub struct GitRepo {
    location: RepoLocation,
    builder: CommandBuilder,
    blob_store: Mutex<Option<BlobStore>>,
}

/// The `-C` argument for a location: the raw local path, or the POSIX path
/// inside the distro (the distro itself is selected by [`CommandBuilder`],
/// not passed to `git`).
fn dash_c_arg(location: &RepoLocation) -> String {
    match location {
        RepoLocation::Local(path) => path.to_string_lossy().into_owned(),
        RepoLocation::Wsl { path, .. } => path.clone(),
    }
}

/// Whether a connected host advertises `blob/get` support (`Hello.caps`
/// contains `"blob"`) — a stale pre-S2 `dv-host` binary predates
/// `blob/get` entirely and would otherwise draw a `bad_request` per blob
/// lookup instead of a clean fallback (plan §8 S2 review finding P3-5).
/// Factored out as a pure predicate over `&[String]` so it's unit-testable
/// without a real `HostClient`/transport.
fn client_supports_blob_get(caps: &[String]) -> bool {
    caps.iter().any(|cap| cap == "blob")
}

impl GitRepo {
    /// Open and validate: resolves the repo root via
    /// `git rev-parse --show-toplevel` and stores the *root* as the
    /// location (the input may point anywhere inside the work tree).
    /// A non-repo path is a descriptive `Err`, not a panic.
    pub fn open(location: RepoLocation) -> Result<Self> {
        let probe_builder = CommandBuilder::new(location.clone());
        let probe_root = dash_c_arg(&location);
        let toplevel = probe_builder
            .run_text("git", &["-C", &probe_root, "rev-parse", "--show-toplevel"])
            .with_context(|| format!("not a git repository: {}", location.display_name()))?;

        let normalized = match location {
            RepoLocation::Local(_) => RepoLocation::Local(PathBuf::from(toplevel)),
            RepoLocation::Wsl { distro, .. } => RepoLocation::Wsl {
                distro,
                path: toplevel,
            },
        };
        let builder = CommandBuilder::new(normalized.clone());

        Ok(Self {
            location: normalized,
            builder,
            blob_store: Mutex::new(None),
        })
    }

    pub fn location(&self) -> &RepoLocation {
        &self.location
    }

    pub fn builder(&self) -> &CommandBuilder {
        &self.builder
    }

    fn root_arg(&self) -> String {
        dash_c_arg(&self.location)
    }

    /// Run a git subcommand scoped to this repo's root, decoding stdout as
    /// text (trimmed). Not for `-z`-delimited output — see [`Self::git_raw`].
    fn git_text(&self, args: &[&str]) -> Result<String> {
        let root = self.root_arg();
        let mut full_args = Vec::with_capacity(args.len() + 2);
        full_args.push("-C");
        full_args.push(root.as_str());
        full_args.extend_from_slice(args);
        self.builder.run_text("git", &full_args)
    }

    /// Run a git subcommand scoped to this repo's root, returning raw
    /// stdout bytes untouched by [`crate::command::decode_output`] — needed
    /// for `-z` output, which uses NUL as a field separator and would
    /// otherwise trip the UTF-16 heuristic.
    fn git_raw(&self, args: &[&str]) -> Result<Vec<u8>> {
        let root = self.root_arg();
        let mut full_args = Vec::with_capacity(args.len() + 2);
        full_args.push("-C");
        full_args.push(root.as_str());
        full_args.extend_from_slice(args);
        self.builder.run("git", &full_args)
    }

    /// Current branch name, or short SHA when detached.
    pub fn head_label(&self) -> Result<String> {
        match self.git_text(&["symbolic-ref", "--short", "-q", "HEAD"]) {
            Ok(name) => Ok(name),
            Err(_) => self.git_text(&["rev-parse", "--short", "HEAD"]),
        }
    }

    /// `git rev-parse --verify` → full SHA. Errors on unknown revs.
    pub fn resolve(&self, rev: &str) -> Result<String> {
        self.git_text(&["rev-parse", "--verify", rev])
            .with_context(|| format!("unknown revision: {rev}"))
    }

    /// `git merge-base <a> <b>` → full SHA of the best common ancestor.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<String> {
        self.git_text(&["merge-base", a, b])
            .with_context(|| format!("no merge base between {a} and {b}"))
    }

    /// `git remote get-url <name>` — the configured URL for a remote.
    /// [`crate::github`] uses this against `"origin"` to derive the GitHub
    /// repo slug; `gh` itself never runs against a WSL location, but the
    /// remote URL is read through the normal routed git layer so it works
    /// identically for local and WSL repos.
    pub fn remote_url(&self, name: &str) -> Result<String> {
        self.git_text(&["remote", "get-url", name])
            .with_context(|| format!("reading url of remote {name:?}"))
    }

    /// `git fetch origin pull/<n>/head` — fetches a PR's head commit (by
    /// GitHub's synthetic `pull/<n>/head` ref) so its blobs exist locally,
    /// without creating or updating any local branch. Routed, so this works
    /// for WSL repos exactly like every other git call here.
    pub fn fetch_pr_head(&self, number: u64) -> Result<()> {
        let root = self.root_arg();
        self.builder
            .run(
                "git",
                &[
                    "-C",
                    &root,
                    "fetch",
                    "origin",
                    "--",
                    &format!("pull/{number}/head"),
                ],
            )
            .with_context(|| format!("fetching pull/{number}/head from origin"))?;
        Ok(())
    }

    /// `git fetch origin <refname>` — used to make sure a PR's base branch
    /// tip is present locally before diffing or computing a merge-base.
    pub fn fetch_ref(&self, refname: &str) -> Result<()> {
        let root = self.root_arg();
        self.builder
            .run("git", &["-C", &root, "fetch", "origin", "--", refname])
            .with_context(|| format!("fetching {refname} from origin"))?;
        Ok(())
    }

    /// Whether `oid` names an object already present in the local object
    /// database — `git cat-file -e`, which (unlike `rev-parse --verify`)
    /// actually checks object existence rather than just revision syntax.
    /// Used to skip a redundant fetch when the PR's head/base is already
    /// local.
    pub fn has_object(&self, oid: &str) -> bool {
        let root = self.root_arg();
        self.builder
            .run("git", &["-C", &root, "cat-file", "-e", oid])
            .is_ok()
    }

    /// `git rev-parse --abbrev-ref HEAD` — the current branch name, or
    /// `None` when `HEAD` is detached (git prints the literal string
    /// `"HEAD"` in that case). Used by `dv pr create` to refuse opening a PR
    /// from a detached checkout.
    pub fn current_branch(&self) -> Result<Option<String>> {
        let name = self.git_text(&["rev-parse", "--abbrev-ref", "HEAD"])?;
        Ok(if name == "HEAD" { None } else { Some(name) })
    }

    /// The remote's configured default branch (e.g. `"main"`), read from
    /// `origin/HEAD`'s symbolic ref — set by `git clone` or `git remote
    /// set-head origin -a`. `None` (rather than an `Err`) when it isn't set
    /// (a shallow or manually-configured checkout, say): callers treat this
    /// as best-effort, not a hard requirement.
    pub fn default_branch(&self) -> Option<String> {
        let full = self
            .git_text(&["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"])
            .ok()?;
        full.strip_prefix("origin/").map(str::to_string)
    }

    /// `git config --get <key>` scoped to the repo root. `Ok(None)`
    /// (returned as a plain `None`, not an `Err`) both when the key is
    /// genuinely unset (git's exit 1 with empty stderr) and on any other
    /// failure reading it — the only callers ([`crate::github`]'s author
    /// resolution, by way of the app crate) are meant to degrade silently
    /// through a chain of fallbacks rather than surface a config-reading
    /// error to the user.
    pub fn config(&self, key: &str) -> Option<String> {
        let root = self.root_arg();
        self.builder
            .run_text("git", &["-C", &root, "config", "--get", key])
            .ok()
            .filter(|v| !v.is_empty())
    }

    /// Whether `HEAD` doesn't resolve yet — a freshly `git init`ed repo with
    /// no commits. Paid unconditionally by [`Self::diff_root`]'s callers
    /// (`WorkingTree`/`Staged`): one cheap subprocess, simpler branching
    /// than trying to special-case the failure of the real diff command.
    fn head_is_unborn(&self) -> bool {
        self.git_text(&["rev-parse", "--verify", "-q", "HEAD"])
            .is_err()
    }

    /// The tree-ish `WorkingTree`/`Staged` diff against: `HEAD` normally,
    /// or the empty-tree oid when `HEAD` is unborn (no commits yet) — so a
    /// brand-new repo reports every working/staged file as `Added` instead
    /// of failing with git's "ambiguous argument HEAD" (exit 128).
    fn diff_root(&self) -> Result<String> {
        if self.head_is_unborn() {
            let format = self
                .git_text(&["rev-parse", "--show-object-format"])
                .context("determining object format for unborn-HEAD diff")?;
            let oid = match format.as_str() {
                "sha1" => EMPTY_TREE_SHA1,
                "sha256" => EMPTY_TREE_SHA256,
                other => return Err(anyhow!("unsupported git object format: {other}")),
            };
            Ok(oid.to_string())
        } else {
            Ok("HEAD".to_string())
        }
    }

    /// Changed files for a diff source, in git's output order, with rename
    /// detection (`-M`) on. For [`DiffSource::WorkingTree`], also appends
    /// never-added (untracked) files as synthetic `Added` entries, since
    /// `git diff` never reports them. `WorkingTree`/`Staged` fall back to
    /// diffing against the empty tree when `HEAD` is unborn (no commits
    /// yet) instead of failing.
    pub fn changed_files(&self, source: &DiffSource) -> Result<Vec<ChangedFile>> {
        match source {
            DiffSource::WorkingTree => {
                let root = self.diff_root()?;
                let bytes = self.git_raw(&["diff", &root, "--name-status", "-z", "-M"])?;
                let mut files = parse_name_status_z(&bytes)?;
                self.append_untracked(&mut files)?;
                Ok(files)
            }
            DiffSource::Staged => {
                let root = self.diff_root()?;
                let bytes =
                    self.git_raw(&["diff", "--cached", &root, "--name-status", "-z", "-M"])?;
                parse_name_status_z(&bytes)
            }
            DiffSource::Range {
                base,
                head,
                merge_base,
            } => {
                let range = if *merge_base {
                    format!("{base}...{head}")
                } else {
                    format!("{base}..{head}")
                };
                let bytes = self.git_raw(&["diff", "--name-status", "-z", "-M", range.as_str()])?;
                parse_name_status_z(&bytes)
            }
            DiffSource::Commit(sha) => {
                // The single-arg diff-tree form prints NOTHING for a merge
                // commit; diff explicitly against the first parent instead
                // (matching the `{sha}^` old side the UI loads). Root
                // commits have no parent and keep the --root form.
                let parent = format!("{sha}^");
                let bytes = if self
                    .git_text(&["rev-parse", "--verify", "-q", &parent])
                    .is_ok()
                {
                    self.git_raw(&[
                        "diff-tree",
                        "-r",
                        "--no-commit-id",
                        "--name-status",
                        "-z",
                        "-M",
                        parent.as_str(),
                        sha.as_str(),
                    ])?
                } else {
                    self.git_raw(&[
                        "diff-tree",
                        "-r",
                        "--root",
                        "--no-commit-id",
                        "--name-status",
                        "-z",
                        "-M",
                        sha.as_str(),
                    ])?
                };
                parse_name_status_z(&bytes)
            }
        }
    }

    /// Whole-diff `+/−` line totals for a diff source, via `--numstat`
    /// (never `--shortstat`: its "N insertions(+)" sentence is localized
    /// porcelain — the same locale trap Phase 5 hit with `sh -c` — while
    /// numstat is stable tab-separated numbers). Binary files (numstat's
    /// `-\t-`) contribute nothing. Mirrors [`Self::changed_files`]'s
    /// per-source command shapes exactly, including the unborn-HEAD
    /// empty-tree fallback and the merge-commit first-parent rule — but
    /// NOT its untracked-files append: `git diff` doesn't count untracked
    /// content and neither does this (matching `git diff --stat`'s own
    /// behavior for a working-tree diff).
    pub fn diffstat(&self, source: &DiffSource) -> Result<DiffTotals> {
        let text = match source {
            DiffSource::WorkingTree => {
                let root = self.diff_root()?;
                self.git_text(&["diff", &root, "--numstat", "-M"])?
            }
            DiffSource::Staged => {
                let root = self.diff_root()?;
                self.git_text(&["diff", "--cached", &root, "--numstat", "-M"])?
            }
            DiffSource::Range {
                base,
                head,
                merge_base,
            } => {
                let range = if *merge_base {
                    format!("{base}...{head}")
                } else {
                    format!("{base}..{head}")
                };
                self.git_text(&["diff", "--numstat", "-M", range.as_str()])?
            }
            DiffSource::Commit(sha) => {
                let parent = format!("{sha}^");
                if self
                    .git_text(&["rev-parse", "--verify", "-q", &parent])
                    .is_ok()
                {
                    self.git_text(&[
                        "diff-tree",
                        "-r",
                        "--no-commit-id",
                        "--numstat",
                        "-M",
                        parent.as_str(),
                        sha.as_str(),
                    ])?
                } else {
                    self.git_text(&[
                        "diff-tree",
                        "-r",
                        "--root",
                        "--no-commit-id",
                        "--numstat",
                        "-M",
                        sha.as_str(),
                    ])?
                }
            }
        };
        Ok(parse_numstat_totals(&text))
    }

    /// Appends untracked (never-added) working-tree files — from
    /// `git ls-files --others --exclude-standard -z` — to `files` as
    /// synthetic `Added` entries, skipping any path already present.
    /// Paths are NUL-separated; a trailing empty token (from the final
    /// separator) is ignored.
    fn append_untracked(&self, files: &mut Vec<ChangedFile>) -> Result<()> {
        let bytes = self.git_raw(&["ls-files", "--others", "--exclude-standard", "-z"])?;
        let text = String::from_utf8_lossy(&bytes);
        let mut tokens: Vec<&str> = text.split('\0').collect();
        if tokens.last() == Some(&"") {
            tokens.pop();
        }

        // `--others` already excludes tracked paths, so overlap with the diff
        // list is not expected — but guard against it without an O(n·m) scan.
        let existing: std::collections::HashSet<String> =
            files.iter().map(|f| f.path.clone()).collect();
        for path in tokens {
            if existing.contains(path) {
                continue;
            }
            files.push(ChangedFile {
                path: path.to_string(),
                old_path: None,
                status: ChangeStatus::Added,
            });
        }

        Ok(())
    }

    /// Load one side's content. `Ok(None)` when the blob doesn't exist
    /// (deleted file's new side, added file's old side, missing working
    /// file). Committed/index content flows through a persistent
    /// `git cat-file --batch` child process, not one spawn per file.
    pub fn blob_bytes(&self, spec: &BlobSpec) -> Result<Option<Vec<u8>>> {
        match spec {
            BlobSpec::Rev { rev, path } => self.batch_request(&format!("{rev}:{path}")),
            BlobSpec::Index { path } => self.batch_request(&format!(":0:{path}")),
            BlobSpec::Working { path } => self.working_blob(path),
        }
    }

    /// Route to `blob/get` when a host is connected AND advertises the
    /// `blob` capability — a stale pre-S2 `dv-host` binary predates
    /// `blob/get` entirely and would otherwise draw a `bad_request` per
    /// blob lookup instead of a clean fallback (plan §8 S2 review finding
    /// P3-5). Any failure — Connection, Timeout, or a structured Rpc error
    /// alike — falls back transparently to [`Self::batch_request_local`]
    /// for THIS call only: unlike `CommandBuilder::run`'s stricter "only a
    /// Connection-level failure falls back" rule (a `proc/exec` command
    /// might not be idempotent), a blob READ is always safe to retry via
    /// the local path no matter why the host attempt failed. Downgrading
    /// the host to `Dead`, though, is gated exactly the way
    /// `CommandBuilder` gates it: only a Connection-level failure from the
    /// SAME client instance currently registered as alive does that (see
    /// `manager::mark_dead`'s doc comment) — a structured Rpc error from a
    /// healthy host, or a stale client a long-lived `GitRepo` still holds,
    /// must never downgrade the connection.
    fn batch_request(&self, spec: &str) -> Result<Option<Vec<u8>>> {
        if let Some(client) = self.builder.host_client() {
            if client_supports_blob_get(client.caps()) {
                match client.blob_get(&self.root_arg(), spec) {
                    Ok(result) => return Ok(result),
                    Err(err) if RequestFailure::is_connection_failure(&err) => {
                        manager::note_host_connection_lost(
                            &self.location,
                            &client,
                            &format!("blob/get: {err:#}"),
                        );
                    }
                    Err(err) => {
                        // A structured Rpc error (or a Timeout) from an
                        // otherwise-healthy host — surfaced/counted the same
                        // as before, but never a reason to start this
                        // distro's cool-down (see `note_host_connection_lost`'s
                        // doc: only a connection-level failure does that).
                        manager::note_spawn_fallback(&self.location, &format!("blob/get: {err:#}"));
                    }
                }
            } else {
                manager::note_spawn_fallback(&self.location, "host lacks blob capability");
            }
        }
        self.batch_request_local(spec)
    }

    /// The Stage-A path: a persistent `git cat-file --batch` child, one per
    /// repo, respawned on any protocol error.
    fn batch_request_local(&self, spec: &str) -> Result<Option<Vec<u8>>> {
        let mut guard = self.blob_store.lock().unwrap_or_else(|e| e.into_inner());

        if guard.is_none() {
            *guard = Some(BlobStore::spawn(&self.builder, &self.root_arg())?);
        }

        // Just populated above if it was empty, so this is always `Some`.
        let store = guard.as_mut().unwrap();
        match store.request(spec) {
            Ok(value) => Ok(value),
            Err(err) => {
                // Drop the (possibly wedged) child; the next call respawns.
                *guard = None;
                Err(err)
            }
        }
    }

    /// The blob object id of one side of a file, for anchoring review
    /// comments to `(path, blob_sha, side, line range)` — see
    /// `docs/phase-2-review-layer.md`. `Ok(None)` when the path doesn't
    /// exist on that side; a genuine git failure (bad rev, ...) stays
    /// `Err`.
    pub fn blob_sha(&self, spec: &BlobSpec) -> Result<Option<String>> {
        match spec {
            BlobSpec::Rev { rev, path } => self.blob_sha_rev(rev, path),
            BlobSpec::Index { path } => self.blob_sha_index(path),
            BlobSpec::Working { path } => self.blob_sha_working(path),
        }
    }

    /// `git ls-tree <rev> -- <path>` and parse the sha field, rather than
    /// `git rev-parse <rev>:<path>` and try to tell "path missing" apart
    /// from a real failure by grepping stderr — `ls-tree` prints nothing
    /// (exit 0) for a missing path, so absence and failure are already
    /// distinguished by the command layer's own success/error split.
    fn blob_sha_rev(&self, rev: &str, path: &str) -> Result<Option<String>> {
        let root = self.root_arg();
        let output = self
            .builder
            .run_text("git", &["-C", &root, "ls-tree", rev, "--", path])
            .with_context(|| format!("git ls-tree {rev} -- {path}"))?;
        Ok(parse_ls_tree_sha(&output))
    }

    /// `git ls-files -s -- <path>` and parse the stage-0 sha. Empty output
    /// (untracked or nonexistent path) → `None`. A path stuck at stages
    /// 1-3 (unresolved merge conflict) has no stage-0 entry either, and
    /// also reports `None` — there is no single "the" blob to anchor to.
    fn blob_sha_index(&self, path: &str) -> Result<Option<String>> {
        let root = self.root_arg();
        let output = self
            .builder
            .run_text("git", &["-C", &root, "ls-files", "-s", "--", path])
            .with_context(|| format!("git ls-files -s -- {path}"))?;
        Ok(parse_ls_files_stage0_sha(&output))
    }

    /// `git hash-object -- <path>` against the working file (`-C` scopes it
    /// to the repo root the same way every other git call here does, so
    /// `path` stays repo-relative for both Local and Wsl). A missing file
    /// fails the command; that failure is `Ok(None)` when it names a
    /// missing path, `Err` for anything else (permissions, not a repo,
    /// ...).
    fn blob_sha_working(&self, path: &str) -> Result<Option<String>> {
        let root = self.root_arg();
        match self
            .builder
            .run_text("git", &["-C", &root, "hash-object", "--", path])
        {
            Ok(sha) => Ok(Some(sha)),
            Err(err) if err.to_string().contains("No such file or directory") => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn working_blob(&self, path: &str) -> Result<Option<Vec<u8>>> {
        match &self.location {
            RepoLocation::Local(root) => match std::fs::read(root.join(path)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err).with_context(|| format!("failed to read {path}")),
            },
            RepoLocation::Wsl { path: root, .. } => {
                let full_path = join_posix(root, path);
                match self.builder.run("cat", &["--", &full_path]) {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(err) if err.to_string().contains("No such file or directory") => Ok(None),
                    Err(err) => Err(err),
                }
            }
        }
    }
}

fn join_posix(root: &str, rel: &str) -> String {
    if root.ends_with('/') {
        format!("{root}{rel}")
    } else {
        format!("{root}/{rel}")
    }
}

/// Parse `git diff --name-status -z -M` (or `diff-tree`'s equivalent)
/// output. Fields are NUL-separated; a rename/copy status is followed by
/// two path fields (old, new) instead of one.
fn parse_name_status_z(bytes: &[u8]) -> Result<Vec<ChangedFile>> {
    let text = String::from_utf8_lossy(bytes);
    let mut tokens: Vec<&str> = text.split('\0').collect();
    if tokens.last() == Some(&"") {
        tokens.pop();
    }

    let mut files = Vec::new();
    let mut tokens = tokens.into_iter();
    while let Some(status_token) = tokens.next() {
        let status_char = status_token
            .chars()
            .next()
            .ok_or_else(|| anyhow!("malformed name-status output: empty status token"))?;

        let status = match status_char {
            'A' => ChangeStatus::Added,
            'M' => ChangeStatus::Modified,
            'D' => ChangeStatus::Deleted,
            'R' => ChangeStatus::Renamed,
            'C' => ChangeStatus::Copied,
            'T' => ChangeStatus::TypeChanged,
            'U' => ChangeStatus::Unmerged,
            other => ChangeStatus::Unknown(other),
        };

        if matches!(status_char, 'R' | 'C') {
            let old_path = tokens.next().ok_or_else(|| {
                anyhow!("malformed name-status output: missing old path after {status_token:?}")
            })?;
            let new_path = tokens.next().ok_or_else(|| {
                anyhow!("malformed name-status output: missing new path after {status_token:?}")
            })?;
            files.push(ChangedFile {
                path: new_path.to_string(),
                old_path: Some(old_path.to_string()),
                status,
            });
        } else {
            let path = tokens.next().ok_or_else(|| {
                anyhow!("malformed name-status output: missing path after {status_token:?}")
            })?;
            files.push(ChangedFile {
                path: path.to_string(),
                old_path: None,
                status,
            });
        }
    }

    Ok(files)
}

/// Parse the sha field out of one `git ls-tree <rev> -- <path>` line
/// (`<mode> <type> <sha>\t<path>`). Empty input (path absent at `rev`) →
/// `None`.
fn parse_ls_tree_sha(output: &str) -> Option<String> {
    let line = output.lines().next()?;
    let (info, _path) = line.split_once('\t')?;
    let sha = info.split_whitespace().nth(2)?;
    Some(sha.to_string())
}

/// Parse the stage-0 sha out of `git ls-files -s -- <path>` output
/// (`<mode> <sha> <stage>\t<path>`, one line per stage present). `None` if
/// there is no stage-0 line (path not in the index, or stuck at stages
/// 1-3 from an unresolved merge conflict).
fn parse_ls_files_stage0_sha(output: &str) -> Option<String> {
    for line in output.lines() {
        let (info, _path) = line.split_once('\t')?;
        let mut parts = info.split_whitespace();
        let _mode = parts.next()?;
        let sha = parts.next()?;
        let stage = parts.next()?;
        if stage == "0" {
            return Some(sha.to_string());
        }
    }
    None
}

/// Sum a `--numstat` body into whole-diff totals. Each line is
/// `<added>\t<deleted>\t<path>`; binary files print `-` for both counts
/// and are skipped (as are any malformed lines — totals are a summary
/// hint, not something worth failing a hydration pass over).
fn parse_numstat_totals(output: &str) -> DiffTotals {
    let mut totals = DiffTotals {
        additions: 0,
        deletions: 0,
    };
    for line in output.lines() {
        let mut fields = line.split('\t');
        let (Some(added), Some(deleted)) = (fields.next(), fields.next()) else {
            continue;
        };
        let (Ok(added), Ok(deleted)) = (added.parse::<u64>(), deleted.parse::<u64>()) else {
            continue;
        };
        totals.additions += added;
        totals.deletions += deleted;
    }
    totals
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_range (moved from crates/app/src/main.rs, Phase 8 S8b) -----

    #[test]
    fn parse_range_two_dot() {
        assert_eq!(
            parse_range("main..feature"),
            Ok(DiffSource::Range {
                base: "main".to_string(),
                head: "feature".to_string(),
                merge_base: false,
            })
        );
    }

    #[test]
    fn parse_range_three_dot_is_merge_base() {
        assert_eq!(
            parse_range("main...feature"),
            Ok(DiffSource::Range {
                base: "main".to_string(),
                head: "feature".to_string(),
                merge_base: true,
            })
        );
    }

    #[test]
    fn parse_range_rejects_missing_separator() {
        assert!(parse_range("main").is_err());
    }

    #[test]
    fn parse_range_rejects_empty_endpoints() {
        assert!(parse_range("..feature").is_err());
        assert!(parse_range("main..").is_err());
    }

    #[test]
    fn parses_simple_statuses() {
        let raw = b"M\0a.txt\0A\0b.txt\0D\0c.txt\0";
        let files = parse_name_status_z(raw).unwrap();
        assert_eq!(
            files,
            vec![
                ChangedFile {
                    path: "a.txt".to_string(),
                    old_path: None,
                    status: ChangeStatus::Modified,
                },
                ChangedFile {
                    path: "b.txt".to_string(),
                    old_path: None,
                    status: ChangeStatus::Added,
                },
                ChangedFile {
                    path: "c.txt".to_string(),
                    old_path: None,
                    status: ChangeStatus::Deleted,
                },
            ]
        );
    }

    #[test]
    fn parses_rename_with_similarity_score() {
        let raw = b"R100\0old.txt\0new.txt\0";
        let files = parse_name_status_z(raw).unwrap();
        assert_eq!(
            files,
            vec![ChangedFile {
                path: "new.txt".to_string(),
                old_path: Some("old.txt".to_string()),
                status: ChangeStatus::Renamed,
            }]
        );
    }

    #[test]
    fn empty_output_is_no_files() {
        assert_eq!(parse_name_status_z(b"").unwrap(), Vec::new());
    }

    #[test]
    fn malformed_status_without_path_errors_not_panics() {
        let raw = b"M";
        assert!(parse_name_status_z(raw).is_err());
    }

    #[test]
    fn malformed_rename_missing_new_path_errors() {
        let raw = b"R100\0old.txt\0";
        assert!(parse_name_status_z(raw).is_err());
    }

    #[test]
    fn parses_ls_tree_sha_line() {
        let line = "100644 blob e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\tfoo.txt";
        assert_eq!(
            parse_ls_tree_sha(line),
            Some("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".to_string())
        );
    }

    #[test]
    fn empty_ls_tree_output_is_none() {
        assert_eq!(parse_ls_tree_sha(""), None);
    }

    #[test]
    fn parses_ls_files_stage0_sha_line() {
        let output = "100644 e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 0\tfoo.txt";
        assert_eq!(
            parse_ls_files_stage0_sha(output),
            Some("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".to_string())
        );
    }

    #[test]
    fn ls_files_stage0_skips_conflict_stages() {
        // An unresolved merge conflict has stages 1/2/3 but no stage 0.
        let output = "\
100644 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1\tfoo.txt
100644 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 2\tfoo.txt
100644 cccccccccccccccccccccccccccccccccccccccc 3\tfoo.txt";
        assert_eq!(parse_ls_files_stage0_sha(output), None);
    }

    #[test]
    fn ls_files_stage0_finds_it_among_other_stages() {
        let output = "\
100644 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1\tfoo.txt
100644 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 0\tfoo.txt";
        assert_eq!(
            parse_ls_files_stage0_sha(output),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string())
        );
    }

    #[test]
    fn empty_ls_files_output_is_none() {
        assert_eq!(parse_ls_files_stage0_sha(""), None);
    }

    // --- client_supports_blob_get: the blob/get caps gate -----------------
    // (plan §8 S2 review finding P3-5)

    #[test]
    fn client_supports_blob_get_true_when_cap_present() {
        assert!(client_supports_blob_get(&[
            "exec".to_string(),
            "blob".to_string()
        ]));
    }

    #[test]
    fn client_supports_blob_get_false_for_a_stale_pre_s2_host() {
        // A pre-S2 dv-host binary only ever advertised "exec" — no
        // "blob" cap, since blob/get didn't exist yet.
        assert!(!client_supports_blob_get(&["exec".to_string()]));
    }

    #[test]
    fn client_supports_blob_get_false_for_no_caps_at_all() {
        assert!(!client_supports_blob_get(&[]));
    }

    // --- parse_numstat_totals (sidebar card diffstat, R2) -----------------

    #[test]
    fn numstat_totals_sums_lines() {
        let output = "10\t2\tsrc/a.rs\n0\t7\tsrc/b.rs\n3\t3\tREADME.md\n";
        assert_eq!(
            parse_numstat_totals(output),
            DiffTotals {
                additions: 13,
                deletions: 12,
            }
        );
    }

    #[test]
    fn numstat_totals_skips_binary_and_malformed_lines() {
        // Binary files print `-` for both counts; a rename line's path
        // field can itself contain tabs under -z (not used here) — either
        // way a non-numeric count must be skipped, not summed or panicked.
        let output = "-\t-\tassets/icon.png\n5\t1\tsrc/a.rs\nnot-a-numstat-line\n";
        assert_eq!(
            parse_numstat_totals(output),
            DiffTotals {
                additions: 5,
                deletions: 1,
            }
        );
    }

    #[test]
    fn numstat_totals_empty_output_is_zero() {
        assert_eq!(
            parse_numstat_totals(""),
            DiffTotals {
                additions: 0,
                deletions: 0,
            }
        );
    }
}
