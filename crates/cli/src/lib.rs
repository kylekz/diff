//! `dv review …` / `dv comment …` / `dv pr …` — the agent-facing CLI
//! (docs/phase-2-review-layer.md § Agent CLI, docs/phase-3-github.md).
//! Gpui-free by construction (a separate workspace crate, not just a module
//! under a gpui-linked binary — Phase 8 S8b promoted the earlier
//! `crates/app/src/cli.rs` module into this standalone crate so it can also
//! build as a native Linux binary; docs/phase-8-lsp-and-polish.md §
//! Distribution & first-run). Two callers reach [`run`]:
//!   - `crates/app/src/main.rs` branches here before any gpui
//!     initialization when `argv[1]` is `"review"`, `"comment"`, or `"pr"`
//!     (already validated by that dispatch check);
//!   - this crate's own `main.rs` (the `dv-cli` binary, installed WSL-side
//!     as `~/.local/bin/dv` by the S8c provisioner) passes `argv[1..]`
//!     straight through, unfiltered — so [`run`] itself has to handle
//!     `--version` and an unrecognized/missing first argument gracefully
//!     rather than assume a caller already checked.
//!
//! `pr_cmd` (the `dv pr ...` subcommands, plus `dv review submit`) lives in
//! its own child module — see its doc comment. `submit`/`pr`/`author` are
//! `pub` so the GUI (`crates/app/src/workspace.rs`) can reach the same
//! submit-validation/PR-prep/author-resolution rules through `dv_cli::`
//! instead of duplicating them.
//!
//! `dv comment list --status open --json` is the canonical "what does the
//! reviewer want from me" query for Claude Code — see CLAUDE.md § Review
//! CLI. The JSON shapes documented on each `print_*` function below are a
//! stable contract: agents parse them.
//!
//! Parsing is hand-rolled (no clap), matching the app's `main.rs` style: a
//! `while let Some(arg) = iter.next()` loop per subcommand, `--flag`
//! consuming the next token as its value.

pub mod author;
pub mod pr;
mod pr_cmd;
mod skill;
pub mod submit;
mod wait;

use std::path::PathBuf;

use dv_core::{
    Comment, CommentStatus, DiffSource, GitRepo, RepoLocation, Review, ReviewState, ReviewStore,
    Side, Verdict,
};
use serde_json::{Value, json};

const TOP_USAGE: &str = "\
usage: dv <review|comment|pr|skill> [options]
       dv --version

  review    manage local reviews (see `dv review --help`)
  comment   manage review comments (see `dv comment --help`)
  pr        GitHub PR operations (see `dv pr --help`)
  skill     install/show the dv-review agent skill (see `dv skill --help`)
  --version print the dv-cli version";

/// Whether `args` (argv[1..]) is one of the headless invocations [`run`]
/// handles end-to-end — the SINGLE routing predicate shared by the GUI
/// binary's dispatch (`crates/app/src/main.rs`) and the Windows console
/// launcher (`crates/cli/src/main.rs`), so the two can never drift.
/// Everything else is GUI-shaped: a repo path, `dv pr <number|url>`, GUI
/// flags. `--help`/`-h` is deliberately NOT headless — each binary owns
/// its own usage text (the GUI's debug build documents `--automation`).
pub fn is_headless(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("review" | "comment" | "skill" | "--version" | "-V") => true,
        Some("pr") => matches!(
            pr_first_positional(&args[1..]),
            None | Some("list" | "view" | "create" | "fetch")
        ),
        _ => false,
    }
}

/// The first non-flag token in `dv pr <...>` (everything after `"pr"`),
/// skipping `--repo <path>` / `--wsl <spec>` / `--json` exactly like the
/// (private) `extract_location_globals` does — so `dv pr --repo X list`
/// and `dv pr list --repo X` both see `"list"` here, matching whatever
/// [`run`] will itself dispatch on. `None` means every token was consumed
/// as a flag (or there were none): `dv pr` alone stays headless so [`run`]
/// prints its own "missing subcommand" usage error.
pub fn pr_first_positional(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--repo" | "--wsl" => {
                iter.next();
            }
            "--json" => {}
            other => return Some(other),
        }
    }
    None
}

/// Entry point for both `crates/app/src/main.rs`'s headless dispatch (which
/// pre-filters via [`is_headless`] before ever calling this) and the
/// `dv-cli` binary's own `main()` — see the module doc. Returns the
/// process exit code.
pub fn run(args: &[String]) -> i32 {
    // SAFETY: `AttachConsole` is documented as safe to call unconditionally
    // (it merely attaches this process to its parent console if one exists
    // and this process has none). A release build of the GUI binary is
    // `windows_subsystem = "windows"` and so starts with no console at all;
    // run from an interactive console, its stdout/stderr would otherwise go
    // nowhere. Failure (already attached — always true in debug builds,
    // which are console-subsystem — or no parent console, e.g. launched
    // from Explorer) is harmless and deliberately ignored: piped/redirected
    // stdio (the case that actually matters for agents) works regardless.
    // A no-op on the native `dv-cli` Linux binary (this whole block is
    // `#[cfg(windows)]`).
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }

    match args.first().map(String::as_str) {
        Some("review") => dispatch(&args[1..], REVIEW_USAGE, review_router),
        Some("comment") => dispatch(&args[1..], COMMENT_USAGE, comment_router),
        Some("pr") => dispatch(&args[1..], pr_cmd::PR_USAGE, pr_cmd::pr_router),
        // `skill` takes no repo location — a small bespoke arm instead of
        // `dispatch` (whose location-global extraction would silently
        // accept a meaningless `--repo`).
        Some("skill") => {
            let rest = &args[1..];
            let json = rest.iter().any(|a| a == "--json");
            let leftover: Vec<String> = rest.iter().filter(|a| *a != "--json").cloned().collect();
            let Some((sub, sub_rest)) = leftover.split_first() else {
                return report_error(usage_err("missing subcommand", skill::SKILL_USAGE), json);
            };
            match skill::skill_router(sub, sub_rest, json) {
                Ok(()) => 0,
                Err(err) => report_error(err, json),
            }
        }
        // Version-sync is by content hash, not this string (see
        // `crates/core/src/remote/install.rs`'s module doc) — `--version`
        // is purely a diagnostic nicety. It's reachable from BOTH callers:
        // the native `dv-cli` binary passes it straight through, and
        // `crates/app/src/main.rs`'s headless dispatch also routes
        // `dv.exe --version`/`-V` here before touching gpui. Print the
        // invoking binary's own name (argv[0]'s file stem) rather than a
        // hardcoded "dv-cli" so it reads correctly either way — including
        // once the S8c provisioner has installed the native binary
        // WSL-side under the `dv` filename.
        Some("--version") | Some("-V") => {
            let program = std::env::args()
                .next()
                .and_then(|arg0| {
                    PathBuf::from(arg0)
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "dv".to_string());
            println!("{program} {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some(other) => {
            let reason = format!("unknown command: {other}");
            eprintln!("{reason}\n\n{TOP_USAGE}");
            if args.iter().any(|a| a == "--json") {
                println!("{}", json!({ "error": reason }));
            }
            2
        }
        None => {
            eprintln!("{TOP_USAGE}");
            // No args at all means no `--json` either — nothing to prescan.
            2
        }
    }
}

/// A usage error (exit 2) carries the subcommand family's usage text
/// alongside the specific complaint; an operation error (exit 1) is just a
/// message (unknown id, no repo, store failure — see the module doc's exit
/// code contract).
#[derive(Debug)]
enum CliError {
    Usage {
        reason: String,
        usage: &'static str,
    },
    Op(String),
    /// Exit with this code, printing nothing — the command already produced
    /// its own output (e.g. `review wait`'s timeout report, exit 3).
    Exit(i32),
}

fn usage_err(reason: impl Into<String>, usage: &'static str) -> CliError {
    CliError::Usage {
        reason: reason.into(),
        usage,
    }
}

fn op_err(err: anyhow::Error) -> CliError {
    CliError::Op(format!("{err:#}"))
}

/// Lets `ReviewStore::with_lock`'s `E: From<anyhow::Error>` bound work with
/// `CliError` directly — a lock-acquisition failure (contention timeout,
/// an I/O error acquiring it) is exactly the same "operation error" shape
/// [`op_err`] already gives every other store failure.
impl From<anyhow::Error> for CliError {
    fn from(err: anyhow::Error) -> Self {
        op_err(err)
    }
}

/// Print the error (stderr always; `{"error":...}` on stdout too when
/// `--json` was requested — the output contract's error rule applies
/// regardless of *why* the command failed) and return the exit code.
fn report_error(err: CliError, json: bool) -> i32 {
    let (reason, code) = match err {
        CliError::Usage { reason, usage } => {
            eprintln!("{reason}\n\n{usage}");
            (reason, 2)
        }
        CliError::Op(reason) => {
            eprintln!("{reason}");
            (reason, 1)
        }
        CliError::Exit(code) => return code,
    };
    if json {
        println!("{}", json!({ "error": reason }));
    }
    code
}

type Router = fn(&str, &[String], bool, Option<RepoLocation>) -> Result<(), CliError>;

/// Shared front door for both `review` and `comment`: peel off the global
/// flags (`--repo`/`--wsl`/`--json`), then hand the leading token (the
/// actual sub-subcommand: `list`, `add`, ...) and the rest to `router`.
fn dispatch(rest: &[String], top_usage: &'static str, router: Router) -> i32 {
    let (location, json, leftover) = match extract_location_globals(rest) {
        Ok(triple) => triple,
        Err(reason) => {
            // Extraction failed before `--json` could be determined
            // properly — fall back to a plain prescan so even a malformed
            // invocation honors the output contract (the one thing the
            // prescan can get wrong here, a literal "--json" sitting in a
            // value position, also implies malformed flags anyway).
            let json = rest.iter().any(|a| a == "--json");
            return report_error(usage_err(reason, top_usage), json);
        }
    };

    let Some((sub, sub_rest)) = leftover.split_first() else {
        return report_error(usage_err("missing subcommand", top_usage), json);
    };

    match router(sub, sub_rest, json, location) {
        Ok(()) => 0,
        Err(err) => report_error(err, json),
    }
}

/// Every subcommand-level flag that consumes the NEXT token as its value.
/// [`extract_location_globals`] must know these so a global flag appearing
/// as a *value* — `--body "--json"`, `--author "--repo"` — is carried
/// through verbatim instead of being stolen mid-scan (Phase-2 review P3:
/// the old single-pass scan treated every `--json`/`--repo` token as
/// global regardless of position).
const VALUE_FLAGS: &[&str] = &[
    "--file",
    "--lines",
    "--side",
    "--body",
    "--review",
    "--author",
    "--source",
    "--range",
    "--commit",
    "--title",
    "--base",
    "--pr",
    "--verdict",
    "--status",
    "--timeout",
];

/// Pull `--repo <path>`, `--wsl <distro>:<posix-path>`, and `--json` out of
/// `args` wherever they appear (global flags are accepted anywhere after
/// the `review`/`comment` subcommand), returning the resolved location
/// override (if any), whether `--json` was requested, plus everything else
/// in original order for subcommand-specific parsing. Value-taking
/// subcommand flags ([`VALUE_FLAGS`]) have their value token copied through
/// uninterpreted, so it can never be mistaken for a global.
fn extract_location_globals(
    args: &[String],
) -> Result<(Option<RepoLocation>, bool, Vec<String>), String> {
    let mut location: Option<RepoLocation> = None;
    let mut json = false;
    let mut leftover = Vec::with_capacity(args.len());

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--repo" => {
                let value = iter.next().ok_or("--repo requires a path")?;
                if location.is_some() {
                    return Err("only one of --repo/--wsl may be given".to_string());
                }
                location = Some(RepoLocation::from_path_arg(value).map_err(|e| format!("{e:#}"))?);
            }
            "--wsl" => {
                let value = iter.next().ok_or("--wsl requires <distro>:<posix-path>")?;
                if location.is_some() {
                    return Err("only one of --repo/--wsl may be given".to_string());
                }
                location = Some(RepoLocation::from_wsl_arg(value).map_err(|e| format!("{e:#}"))?);
            }
            flag if VALUE_FLAGS.contains(&flag) => {
                leftover.push(arg.clone());
                // A missing value is the subcommand parser's error to
                // report, with its own usage text — pass through as-is.
                if let Some(value) = iter.next() {
                    leftover.push(value.clone());
                }
            }
            _ => leftover.push(arg.clone()),
        }
    }
    Ok((location, json, leftover))
}

fn resolve_repo(location: Option<RepoLocation>) -> Result<GitRepo, CliError> {
    let location = location.unwrap_or_else(|| {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        // `dv.exe <cmd>` invoked FROM a WSL shell (interop) inherits a
        // `\\wsl.localhost\<distro>\...` cwd. Treating that as a Local
        // path would resolve the repo through WINDOWS git over 9P —
        // "dubious ownership" failures and a violation of the no-9P rule —
        // so parse it into `RepoLocation::Wsl` exactly like an explicit
        // `--repo \\wsl.localhost\...` argument (docs/backlog.md interop
        // item). Any other cwd (including a parse error on a weird UNC)
        // stays Local, preserving the old behavior.
        RepoLocation::from_path_arg(&cwd.to_string_lossy()).unwrap_or(RepoLocation::Local(cwd))
    });
    GitRepo::open(location).map_err(op_err)
}

// ---------------------------------------------------------------------
// review
// ---------------------------------------------------------------------

const REVIEW_USAGE: &str = "\
usage: dv review <list|show|create|delete|submit|wait> [options]

  list                        all reviews in the repo
  show <id>                   one review, including its comments
  create [--source working|staged] [--range a..b|a...b] [--commit <rev>]
                               (default source: working)
  delete <id>
  submit [<id>] --pr <number> [--verdict comment|approve|request-changes]
               [--body <text>] [--include-resolved]
               submit a draft review to GitHub via `gh` (docs/phase-3-github.md)
  wait [<id>] [--timeout <seconds>]
               block until review activity changes, then report it
               (exit 0 = changed, 3 = timeout)

global options (may appear anywhere after `review`):
  --repo <path>                local path or \\\\wsl.localhost\\<distro>\\<path> (default: .)
  --wsl <distro>:<posix-path>
  --json                       machine-readable output on stdout";

fn review_router(
    sub: &str,
    args: &[String],
    json: bool,
    location: Option<RepoLocation>,
) -> Result<(), CliError> {
    match sub {
        "list" => {
            if !args.is_empty() {
                return Err(usage_err("review list takes no arguments", REVIEW_USAGE));
            }
            let repo = resolve_repo(location)?;
            let store = ReviewStore::open(repo.location().clone());
            let reviews = store.list().map_err(op_err)?;
            print_review_list(&reviews, json);
            Ok(())
        }
        "show" => {
            let id = parse_single_id(args, "review show")
                .map_err(|reason| usage_err(reason, REVIEW_USAGE))?;
            let repo = resolve_repo(location)?;
            let store = ReviewStore::open(repo.location().clone());
            let review = store
                .load(&id)
                .map_err(op_err)?
                .ok_or_else(|| CliError::Op(format!("no review with id {id:?}")))?;
            print_review(&review, json);
            Ok(())
        }
        "create" => {
            let source =
                parse_review_create(args).map_err(|reason| usage_err(reason, REVIEW_USAGE))?;
            let repo = resolve_repo(location)?;
            // Resolve `a...b` to a concrete two-dot range anchored at the
            // actual merge base, exactly as the GUI does — otherwise
            // old-side comment anchors would hash blobs at the base *tip*,
            // which is not what the diff's old side shows once base has
            // advanced past the fork point.
            let source = match source {
                DiffSource::Range {
                    base,
                    head,
                    merge_base: true,
                } => {
                    let merged = repo.merge_base(&base, &head).map_err(op_err)?;
                    DiffSource::Range {
                        base: merged,
                        head,
                        merge_base: false,
                    }
                }
                other => other,
            };
            // Validate revs NOW (Phase-2 review P3): a typo'd `--range` or
            // `--commit` used to persist fine and only blow up when the
            // review was later opened. The merge-base arm above validates
            // implicitly (`merge_base` errors on unknown revs); symbolic
            // names are kept as typed — this checks existence, it doesn't
            // pin.
            match &source {
                DiffSource::Range { base, head, .. } => {
                    repo.resolve(base).map_err(op_err)?;
                    repo.resolve(head).map_err(op_err)?;
                }
                DiffSource::Commit(sha) => {
                    repo.resolve(sha).map_err(op_err)?;
                }
                DiffSource::WorkingTree | DiffSource::Staged => {}
            }
            let store = ReviewStore::open(repo.location().clone());
            let review = store.create(source).map_err(op_err)?;
            print_review(&review, json);
            Ok(())
        }
        "delete" => {
            let id = parse_single_id(args, "review delete")
                .map_err(|reason| usage_err(reason, REVIEW_USAGE))?;
            let repo = resolve_repo(location)?;
            let store = ReviewStore::open(repo.location().clone());
            store.delete(&id).map_err(op_err)?;
            print_review_delete(&id, json);
            Ok(())
        }
        "submit" => pr_cmd::cmd_review_submit(args, json, location),
        "wait" => wait::cmd_review_wait(args, json, location),
        other => Err(usage_err(
            format!("unknown review subcommand: {other}"),
            REVIEW_USAGE,
        )),
    }
}

fn parse_single_id(args: &[String], what: &str) -> Result<String, String> {
    match args {
        [id] if !id.starts_with("--") => Ok(id.clone()),
        [] => Err(format!("{what} requires <id>")),
        [extra, ..] => Err(format!("{what}: unexpected argument {extra:?}")),
    }
}

/// `--source working|staged`, `--range a..b|a...b`, `--commit <rev>` are
/// mutually exclusive; the default (none given) is `DiffSource::WorkingTree`.
fn parse_review_create(args: &[String]) -> Result<DiffSource, String> {
    let mut source: Option<DiffSource> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--source" => {
                let value = iter.next().ok_or("--source requires working|staged")?;
                let candidate = match value.as_str() {
                    "working" => DiffSource::WorkingTree,
                    "staged" => DiffSource::Staged,
                    other => return Err(format!("--source must be working|staged, got {other}")),
                };
                set_source_once(&mut source, candidate)?;
            }
            "--range" => {
                let value = iter
                    .next()
                    .ok_or("--range requires <a>..<b> or <a>...<b>")?;
                let candidate = dv_core::parse_range(value)?;
                set_source_once(&mut source, candidate)?;
            }
            "--commit" => {
                let value = iter.next().ok_or("--commit requires a revision")?;
                set_source_once(&mut source, DiffSource::Commit(value.clone()))?;
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok(source.unwrap_or(DiffSource::WorkingTree))
}

/// Errors if `source` is already set — `--source`/`--range`/`--commit` are
/// mutually exclusive ways to pick `review create`'s [`DiffSource`].
fn set_source_once(source: &mut Option<DiffSource>, value: DiffSource) -> Result<(), String> {
    if source.is_some() {
        return Err("only one of --source/--range/--commit may be given".to_string());
    }
    *source = Some(value);
    Ok(())
}

// ---------------------------------------------------------------------
// comment
// ---------------------------------------------------------------------

const COMMENT_USAGE: &str = "\
usage: dv comment <add|reply|resolve|unresolve|list> [options]

  add        --file <path> --lines N|N:M [--side old|new] --body <text>
             [--review <id>] [--author <name>]
  reply      <comment-id> --body <text> [--author <name>] [--review <id>]
  resolve    <comment-id> [--review <id>]
  unresolve  <comment-id> [--review <id>]
  list       [--file <path>] [--status open|resolved] [--review <id>]

With no --review, add/reply/resolve/unresolve/list search across all
reviews for the comment id (add targets the most recent draft, creating one
if none exists); list with no --review lists across all reviews.

global options (may appear anywhere after `comment`):
  --repo <path>                local path or \\\\wsl.localhost\\<distro>\\<path> (default: .)
  --wsl <distro>:<posix-path>
  --json                       machine-readable output on stdout";

fn comment_router(
    sub: &str,
    args: &[String],
    json: bool,
    location: Option<RepoLocation>,
) -> Result<(), CliError> {
    match sub {
        "add" => cmd_comment_add(args, json, location),
        "reply" => cmd_comment_reply(args, json, location),
        "resolve" => cmd_comment_status(args, json, location, CommentStatus::Resolved),
        "unresolve" => cmd_comment_status(args, json, location, CommentStatus::Open),
        "list" => cmd_comment_list(args, json, location),
        other => Err(usage_err(
            format!("unknown comment subcommand: {other}"),
            COMMENT_USAGE,
        )),
    }
}

struct CommentAddArgs {
    file: String,
    start: u32,
    end: u32,
    side: Side,
    body: String,
    review: Option<String>,
    /// `None` when `--author` wasn't given — resolved via
    /// `crate::author::resolve_author` once a [`GitRepo`] is available,
    /// rather than defaulting to a placeholder at parse time.
    author: Option<String>,
}

fn parse_comment_add(args: &[String]) -> Result<CommentAddArgs, String> {
    let mut file: Option<String> = None;
    let mut lines: Option<String> = None;
    let mut side = Side::New;
    let mut body: Option<String> = None;
    let mut review: Option<String> = None;
    let mut author: Option<String> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--file" => file = Some(iter.next().ok_or("--file requires a path")?.clone()),
            "--lines" => lines = Some(iter.next().ok_or("--lines requires N or N:M")?.clone()),
            "--side" => side = parse_side(iter.next().ok_or("--side requires old|new")?)?,
            "--body" => body = Some(iter.next().ok_or("--body requires text")?.clone()),
            "--review" => review = Some(iter.next().ok_or("--review requires an id")?.clone()),
            "--author" => author = Some(iter.next().ok_or("--author requires a name")?.clone()),
            other => return Err(format!("unknown flag: {other}")),
        }
    }

    // Comments anchor by repo-root-relative FORWARD-slashed paths (the
    // `ChangedFile` convention) — normalize a Windows-style `--file` so
    // `src\util\date.ts` matches instead of silently anchoring nowhere.
    let file = file.ok_or("--file is required")?.replace('\\', "/");
    let lines = lines.ok_or("--lines is required")?;
    let body = body.ok_or("--body is required")?;
    let (start, end) = parse_lines(&lines)?;

    Ok(CommentAddArgs {
        file,
        start,
        end,
        side,
        body,
        review,
        author,
    })
}

fn cmd_comment_add(
    args: &[String],
    json: bool,
    location: Option<RepoLocation>,
) -> Result<(), CliError> {
    let parsed = parse_comment_add(args).map_err(|reason| usage_err(reason, COMMENT_USAGE))?;

    let repo = resolve_repo(location)?;
    let author = parsed
        .author
        .unwrap_or_else(|| crate::author::resolve_author(&repo));
    let store = ReviewStore::open(repo.location().clone());

    // Two short, separately-locked spans (docs/backlog.md
    // durable-concurrency item) rather than one long one: `find-or-create`
    // is pure store I/O (locked — closes the "two racing invocations both
    // create a draft" TOCTOU), but `blob_sha` below can shell out to `git`,
    // and holding the lock across a subprocess spawn would turn every
    // OTHER writer's bounded acquire wait into a lottery against however
    // long `git` takes on this machine right now. Splitting means the lock
    // is only ever held for actual JSON load/save work — milliseconds, as
    // the whole timeout budget assumes — and the final span fresh-loads
    // again anyway, the same "never trust an unlocked-window-old copy"
    // convention every other mutator here already follows.
    let (target, created) =
        store.with_lock(|| target_review_for_add(&store, parsed.review.as_deref()))?;

    let anchor = dv_core::anchor_spec(&target.source, parsed.side, &parsed.file);
    // GUI parity (Phase-2 review P3): a blob_sha FAILURE — e.g. a Commit
    // source whose rev has no parent, so the old side's `<sha>^` isn't a
    // rev at all — degrades to an unverifiable anchor (warned, sha None)
    // instead of refusing the comment; the GUI records exactly this.
    let blob_sha = match repo.blob_sha(&anchor) {
        Ok(sha) => sha,
        Err(err) => {
            eprintln!(
                "warning: could not verify anchor ({err:#}); recording an unverifiable anchor"
            );
            None
        }
    };
    let unverifiable = blob_sha.is_none();

    let (review, comment) = store.with_lock(|| -> Result<_, CliError> {
        let mut review = store
            .load(&target.id)
            .map_err(op_err)?
            .ok_or_else(|| CliError::Op(format!("review {} disappeared mid-add", target.id)))?;
        let comment = review
            .add_comment(
                parsed.file,
                parsed.side,
                parsed.start,
                parsed.end,
                blob_sha,
                parsed.body,
                author,
            )
            .map_err(op_err)?
            .clone();
        store.save(&review).map_err(op_err)?;
        Ok((review, comment))
    })?;

    print_comment_add(&review.id, &comment, created, unverifiable, json);
    Ok(())
}

/// Resolve which review `comment add` should target: `--review <id>` if
/// given (must already exist), else the most recent draft, else a freshly
/// auto-created one (`review_created: true`). [`ReviewStore::list`] already
/// sorts newest-`created_ms`-first, so the first `Draft` found is the most
/// recent one.
fn target_review_for_add(
    store: &ReviewStore,
    review_id: Option<&str>,
) -> Result<(Review, bool), CliError> {
    if let Some(id) = review_id {
        let review = store
            .load(id)
            .map_err(op_err)?
            .ok_or_else(|| CliError::Op(format!("no review with id {id:?}")))?;
        // Same guard the GUI grew in Phase 6: appending to a SUBMITTED
        // review strands the comment (it will never reach GitHub — submit
        // already happened — and the GUI shows the review read-only).
        if matches!(review.state, ReviewState::Submitted { .. }) {
            return Err(CliError::Op(format!(
                "review {id} is already submitted; a comment added to it would never \
                 reach GitHub. Create a draft (`dv review create`) or omit --review \
                 to target the newest draft."
            )));
        }
        return Ok((review, false));
    }

    let reviews = store.list().map_err(op_err)?;
    if let Some(draft) = reviews
        .into_iter()
        .find(|r| matches!(r.state, ReviewState::Draft))
    {
        return Ok((draft, false));
    }

    let created = store.create(DiffSource::WorkingTree).map_err(op_err)?;
    Ok((created, true))
}

fn cmd_comment_reply(
    args: &[String],
    json: bool,
    location: Option<RepoLocation>,
) -> Result<(), CliError> {
    let (comment_id, body, author, review_id) =
        parse_comment_reply(args).map_err(|reason| usage_err(reason, COMMENT_USAGE))?;

    let repo = resolve_repo(location)?;
    let author = author.unwrap_or_else(|| crate::author::resolve_author(&repo));
    let store = ReviewStore::open(repo.location().clone());

    // Lock the whole find-review → mutate → save span (docs/backlog.md
    // durable-concurrency item) — see `cmd_comment_add`'s comment.
    let review = store.with_lock(|| -> Result<_, CliError> {
        let mut review = find_review_for_comment(&store, review_id.as_deref(), &comment_id)?;
        review.reply(&comment_id, body, author).map_err(op_err)?;
        store.save(&review).map_err(op_err)?;
        Ok(review)
    })?;

    // "reply returns the whole updated comment" (docs/phase-2-review-layer.md
    // § Agent CLI) — the reply we just appended is on it, not a top-level
    // field of its own.
    let comment = review
        .comments
        .iter()
        .find(|c| c.id == comment_id)
        .expect("just replied to it")
        .clone();
    print_comment_reply(&review.id, &comment, json);
    Ok(())
}

/// `(comment_id, body, author, review_id)` — `author` is `None` when
/// `--author` wasn't given, resolved by the caller via
/// `crate::author::resolve_author` once a [`GitRepo`] is available.
fn parse_comment_reply(
    args: &[String],
) -> Result<(String, String, Option<String>, Option<String>), String> {
    let (id, rest) = args
        .split_first()
        .ok_or("comment reply requires <comment-id>")?;
    if id.starts_with("--") {
        return Err(format!("expected <comment-id>, got flag {id}"));
    }

    let mut body: Option<String> = None;
    let mut author: Option<String> = None;
    let mut review: Option<String> = None;

    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--body" => body = Some(iter.next().ok_or("--body requires text")?.clone()),
            "--author" => author = Some(iter.next().ok_or("--author requires a name")?.clone()),
            "--review" => review = Some(iter.next().ok_or("--review requires an id")?.clone()),
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    let body = body.ok_or("--body is required")?;
    Ok((id.clone(), body, author, review))
}

fn cmd_comment_status(
    args: &[String],
    json: bool,
    location: Option<RepoLocation>,
    status: CommentStatus,
) -> Result<(), CliError> {
    let (comment_id, review_id) =
        parse_comment_status_args(args).map_err(|reason| usage_err(reason, COMMENT_USAGE))?;

    let repo = resolve_repo(location)?;
    let store = ReviewStore::open(repo.location().clone());

    // Lock the whole find-review → mutate → save span (docs/backlog.md
    // durable-concurrency item) — see `cmd_comment_add`'s comment.
    let review_id_out = store.with_lock(|| -> Result<_, CliError> {
        let mut review = find_review_for_comment(&store, review_id.as_deref(), &comment_id)?;
        review.set_status(&comment_id, status).map_err(op_err)?;
        store.save(&review).map_err(op_err)?;
        Ok(review.id)
    })?;

    print_comment_status(&review_id_out, &comment_id, status, json);
    Ok(())
}

fn parse_comment_status_args(args: &[String]) -> Result<(String, Option<String>), String> {
    let (id, rest) = args.split_first().ok_or("requires <comment-id>")?;
    if id.starts_with("--") {
        return Err(format!("expected <comment-id>, got flag {id}"));
    }

    let mut review: Option<String> = None;
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--review" => review = Some(iter.next().ok_or("--review requires an id")?.clone()),
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok((id.clone(), review))
}

/// Find the review containing `comment_id`. With `review_id`, only that
/// review is consulted (and must exist); otherwise every review is searched
/// — reply/resolve/unresolve/list never auto-create
/// (docs/phase-2-review-layer.md § Agent CLI semantics).
fn find_review_for_comment(
    store: &ReviewStore,
    review_id: Option<&str>,
    comment_id: &str,
) -> Result<Review, CliError> {
    if let Some(id) = review_id {
        let review = store
            .load(id)
            .map_err(op_err)?
            .ok_or_else(|| CliError::Op(format!("no review with id {id:?}")))?;
        if !review.comments.iter().any(|c| c.id == comment_id) {
            return Err(CliError::Op(format!(
                "no comment with id {comment_id:?} in review {id:?}"
            )));
        }
        return Ok(review);
    }

    let reviews = store.list().map_err(op_err)?;
    reviews
        .into_iter()
        .find(|r| r.comments.iter().any(|c| c.id == comment_id))
        .ok_or_else(|| CliError::Op(format!("no comment with id {comment_id:?} in any review")))
}

fn cmd_comment_list(
    args: &[String],
    json: bool,
    location: Option<RepoLocation>,
) -> Result<(), CliError> {
    let (file, status, review_id) =
        parse_comment_list(args).map_err(|reason| usage_err(reason, COMMENT_USAGE))?;

    let repo = resolve_repo(location)?;
    let store = ReviewStore::open(repo.location().clone());

    let reviews = if let Some(id) = &review_id {
        let review = store
            .load(id)
            .map_err(op_err)?
            .ok_or_else(|| CliError::Op(format!("no review with id {id:?}")))?;
        vec![review]
    } else {
        store.list().map_err(op_err)?
    };

    let mut items: Vec<(String, Comment)> = Vec::new();
    for review in &reviews {
        for comment in &review.comments {
            if let Some(file) = &file
                && &comment.path != file
            {
                continue;
            }
            if let Some(status) = status
                && comment.status != status
            {
                continue;
            }
            items.push((review.id.clone(), comment.clone()));
        }
    }
    items.sort_by_key(|(_, c)| c.created_ms);

    print_comment_list(&items, json);
    Ok(())
}

/// `(file filter, status filter, review-id filter)`.
type CommentListFilters = (Option<String>, Option<CommentStatus>, Option<String>);

fn parse_comment_list(args: &[String]) -> Result<CommentListFilters, String> {
    let mut file: Option<String> = None;
    let mut status: Option<CommentStatus> = None;
    let mut review: Option<String> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--file" => file = Some(iter.next().ok_or("--file requires a path")?.clone()),
            "--status" => {
                let value = iter.next().ok_or("--status requires open|resolved")?;
                status = Some(match value.as_str() {
                    "open" => CommentStatus::Open,
                    "resolved" => CommentStatus::Resolved,
                    other => return Err(format!("--status must be open|resolved, got {other}")),
                });
            }
            "--review" => review = Some(iter.next().ok_or("--review requires an id")?.clone()),
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    Ok((file, status, review))
}

fn parse_side(value: &str) -> Result<Side, String> {
    match value {
        "old" => Ok(Side::Old),
        "new" => Ok(Side::New),
        other => Err(format!("--side must be old|new, got {other}")),
    }
}

/// `10` → `(10, 10)`; `10:20` → `(10, 20)`. Rejects `start < 1` and
/// `end < start`.
fn parse_lines(raw: &str) -> Result<(u32, u32), String> {
    let (start_str, end_str) = raw.split_once(':').unwrap_or((raw, raw));
    let bad = || format!("invalid --lines value: {raw:?} (expected N or N:M)");
    let start: u32 = start_str.parse().map_err(|_| bad())?;
    let end: u32 = end_str.parse().map_err(|_| bad())?;
    if start < 1 {
        return Err(format!("--lines start must be >= 1, got {start}"));
    }
    if end < start {
        return Err(format!("--lines end ({end}) must be >= start ({start})"));
    }
    Ok((start, end))
}

// ---------------------------------------------------------------------
// output
// ---------------------------------------------------------------------

fn print_json(value: &Value) {
    println!("{value}");
}

fn side_word(side: Side) -> &'static str {
    match side {
        Side::Old => "old",
        Side::New => "new",
    }
}

fn status_word(status: CommentStatus) -> &'static str {
    match status {
        CommentStatus::Open => "open",
        CommentStatus::Resolved => "resolved",
    }
}

fn format_state(state: &ReviewState) -> String {
    match state {
        ReviewState::Draft => "draft".to_string(),
        ReviewState::Submitted { verdict, .. } => format!(
            "submitted:{}",
            match verdict {
                Verdict::Comment => "comment",
                Verdict::Approve => "approve",
                Verdict::RequestChanges => "request_changes",
            }
        ),
    }
}

fn format_source(source: &DiffSource) -> String {
    match source {
        DiffSource::WorkingTree => "working-tree".to_string(),
        DiffSource::Staged => "staged".to_string(),
        DiffSource::Range {
            base,
            head,
            merge_base,
        } => format!("{base}{}{head}", if *merge_base { "..." } else { ".." }),
        DiffSource::Commit(sha) => format!("commit {sha}"),
    }
}

fn line_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn print_review_list(reviews: &[Review], json: bool) {
    if json {
        print_json(&json!({ "reviews": reviews }));
        return;
    }
    if reviews.is_empty() {
        println!("no reviews");
        return;
    }
    println!(
        "{:<24} {:<16} {:<24} {:>4}/{:<5} CREATED",
        "ID", "STATE", "SOURCE", "OPEN", "TOTAL"
    );
    for review in reviews {
        let open = review
            .comments
            .iter()
            .filter(|c| c.status == CommentStatus::Open)
            .count();
        println!(
            "{:<24} {:<16} {:<24} {:>4}/{:<5} {}",
            review.id,
            format_state(&review.state),
            format_source(&review.source),
            open,
            review.comments.len(),
            format_ms(review.created_ms),
        );
    }
}

fn print_review(review: &Review, json: bool) {
    if json {
        print_json(&json!({ "review": review }));
        return;
    }
    println!("review {}", review.id);
    println!("  state:    {}", format_state(&review.state));
    println!("  source:   {}", format_source(&review.source));
    println!("  created:  {}", format_ms(review.created_ms));
    println!("  updated:  {}", format_ms(review.updated_ms));
    println!("  comments: {}", review.comments.len());
    for comment in &review.comments {
        print_comment_summary_line(comment);
    }
}

fn print_comment_summary_line(comment: &Comment) {
    println!(
        "    [{}] {} {}:{} ({}) by {} — {} repl{}",
        status_word(comment.status),
        comment.id,
        comment.path,
        line_range(comment.start_line, comment.end_line),
        side_word(comment.side),
        comment.author,
        comment.replies.len(),
        if comment.replies.len() == 1 {
            "y"
        } else {
            "ies"
        },
    );
}

fn print_review_delete(id: &str, json: bool) {
    if json {
        print_json(&json!({ "review_id": id, "deleted": true }));
        return;
    }
    println!("deleted review {id}");
}

fn print_comment_add(
    review_id: &str,
    comment: &Comment,
    created: bool,
    unverifiable: bool,
    json: bool,
) {
    if json {
        print_json(&json!({
            "review_id": review_id,
            "comment": comment,
            "review_created": created,
        }));
        return;
    }
    if created {
        println!("note: no draft review found — created new draft {review_id}");
    }
    if unverifiable {
        eprintln!(
            "warning: path not found on {} side — anchor will be unverifiable",
            side_word(comment.side)
        );
    }
    println!(
        "added comment {} to review {} ({}:{})",
        comment.id,
        review_id,
        comment.path,
        line_range(comment.start_line, comment.end_line),
    );
}

fn print_comment_reply(review_id: &str, comment: &Comment, json: bool) {
    if json {
        print_json(&json!({
            "review_id": review_id,
            "comment": comment,
            "review_created": false,
        }));
        return;
    }
    println!("replied to comment {} in review {}", comment.id, review_id);
}

fn print_comment_status(review_id: &str, comment_id: &str, status: CommentStatus, json: bool) {
    let status_str = status_word(status);
    if json {
        print_json(&json!({
            "review_id": review_id,
            "comment_id": comment_id,
            "status": status_str,
        }));
        return;
    }
    println!("comment {comment_id} in review {review_id} is now {status_str}");
}

fn print_comment_list(items: &[(String, Comment)], json: bool) {
    if json {
        let comments: Vec<Value> = items
            .iter()
            .map(|(review_id, comment)| json!({ "review_id": review_id, "comment": comment }))
            .collect();
        print_json(&json!({ "comments": comments }));
        return;
    }
    if items.is_empty() {
        println!("no comments");
        return;
    }
    for (review_id, comment) in items {
        println!(
            "[{}] {} {}:{} ({}) review={} by {}: {}",
            status_word(comment.status),
            comment.id,
            comment.path,
            line_range(comment.start_line, comment.end_line),
            side_word(comment.side),
            review_id,
            comment.author,
            first_line(&comment.body),
        );
    }
}

/// A one-line preview of a (possibly multi-line markdown) comment body for
/// table-style human output.
fn first_line(body: &str) -> &str {
    body.lines().next().unwrap_or("")
}

/// Format Unix epoch milliseconds as `YYYY-MM-DD HH:MM:SS UTC`, purely for
/// cosmetic human-mode output — not worth a chrono/time dependency for
/// this, so this is Howard Hinnant's `civil_from_days` algorithm
/// (http://howardhinnant.github.io/date_algorithms.html) translated
/// directly to Rust.
fn format_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (h, m, s) = (
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
    );

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_headless / pr_first_positional: the shared GUI-vs-CLI router
    // (moved here from crates/app/src/main.rs when the Windows console
    // launcher made this the predicate's single home) -------------------

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn is_headless_recognizes_cli_subcommands() {
        for first in ["review", "comment", "skill", "--version", "-V"] {
            assert!(is_headless(&argv(&[first])), "{first} should be headless");
        }
        for sub in ["list", "view", "create", "fetch"] {
            assert!(is_headless(&argv(&["pr", sub])));
        }
        // `dv pr` alone stays headless so `run` prints its own usage error.
        assert!(is_headless(&argv(&["pr"])));
        assert!(is_headless(&argv(&["pr", "--json"])));
    }

    #[test]
    fn is_headless_routes_gui_shapes_away() {
        for shape in [
            vec!["D:/some/repo"],
            vec!["pr", "123"],
            vec!["pr", "https://github.com/o/r/pull/9"],
            vec!["pr", "--repo", "D:/x", "123"],
            vec!["--staged"],
            vec!["--help"],
            vec!["-h"],
        ] {
            assert!(!is_headless(&argv(&shape)), "{shape:?} should be GUI");
        }
        assert!(!is_headless(&argv(&[])), "bare launch is a GUI launch");
    }

    #[test]
    fn pr_first_positional_skips_flags_and_finds_subcommand() {
        assert_eq!(pr_first_positional(&argv(&["list"])), Some("list"));
        assert_eq!(
            pr_first_positional(&argv(&["--repo", "D:/x", "list"])),
            Some("list")
        );
        assert_eq!(
            pr_first_positional(&argv(&["--json", "--wsl", "Ubuntu:/x", "fetch", "5"])),
            Some("fetch")
        );
        assert_eq!(pr_first_positional(&argv(&["123"])), Some("123"));
        assert_eq!(pr_first_positional(&argv(&[])), None);
        assert_eq!(pr_first_positional(&argv(&["--json"])), None);
    }

    // --- parse_lines ---------------------------------------------------

    #[test]
    fn parse_lines_single() {
        assert_eq!(parse_lines("10"), Ok((10, 10)));
    }

    #[test]
    fn parse_lines_range() {
        assert_eq!(parse_lines("10:20"), Ok((10, 20)));
    }

    #[test]
    fn parse_lines_rejects_reversed_range() {
        assert!(parse_lines("20:10").is_err());
    }

    #[test]
    fn parse_lines_rejects_zero() {
        assert!(parse_lines("0").is_err());
    }

    #[test]
    fn parse_lines_rejects_garbage() {
        assert!(parse_lines("abc").is_err());
        assert!(parse_lines("10:abc").is_err());
    }

    // --- arg parsing ----------------------------------------------------

    #[test]
    fn parse_comment_add_minimal() {
        let args: Vec<String> = ["--file", "a.rs", "--lines", "10:12", "--body", "why?"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parsed = parse_comment_add(&args).expect("should parse");
        assert_eq!(parsed.file, "a.rs");
        assert_eq!((parsed.start, parsed.end), (10, 12));
        assert_eq!(parsed.side, Side::New); // default
        assert_eq!(parsed.body, "why?");
        // `--author` unset resolves later, via `crate::author::resolve_author`
        // (needs a `GitRepo`), not at parse time.
        assert!(parsed.author.is_none());
        assert!(parsed.review.is_none());
    }

    #[test]
    fn parse_comment_add_all_flags() {
        let args: Vec<String> = [
            "--file", "a.rs", "--lines", "5", "--side", "old", "--body", "hi", "--review",
            "r-1-abcd", "--author", "kyle",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let parsed = parse_comment_add(&args).expect("should parse");
        assert_eq!((parsed.start, parsed.end), (5, 5));
        assert_eq!(parsed.side, Side::Old);
        assert_eq!(parsed.review.as_deref(), Some("r-1-abcd"));
        assert_eq!(parsed.author.as_deref(), Some("kyle"));
    }

    #[test]
    fn parse_comment_add_missing_body_is_usage_error() {
        let args: Vec<String> = ["--file", "a.rs", "--lines", "10"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_comment_add(&args).is_err());
    }

    #[test]
    fn parse_comment_add_bad_lines_is_usage_error() {
        let args: Vec<String> = ["--file", "a.rs", "--lines", "20:10", "--body", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_comment_add(&args).is_err());
    }

    #[test]
    fn parse_review_create_defaults_to_working_tree() {
        let args: Vec<String> = Vec::new();
        assert_eq!(parse_review_create(&args), Ok(DiffSource::WorkingTree));
    }

    #[test]
    fn parse_review_create_range() {
        let args: Vec<String> = ["--range", "main..feature"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_review_create(&args),
            Ok(DiffSource::Range {
                base: "main".to_string(),
                head: "feature".to_string(),
                merge_base: false,
            })
        );
    }

    #[test]
    fn parse_review_create_conflicting_flags_is_error() {
        let args: Vec<String> = ["--staged", "--commit", "abc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // "--staged" isn't even a recognized flag (it's --source staged) so
        // this should already fail as "unknown flag" before it gets to the
        // conflict check — sanity that unknown flags are usage errors too.
        assert!(parse_review_create(&args).is_err());

        let args: Vec<String> = ["--source", "staged", "--commit", "abc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_review_create(&args).is_err());
    }

    #[test]
    fn parse_single_id_requires_exactly_one() {
        assert!(parse_single_id(&[], "review show").is_err());
        assert_eq!(
            parse_single_id(&["r-1".to_string()], "review show"),
            Ok("r-1".to_string())
        );
        assert!(parse_single_id(&["r-1".to_string(), "r-2".to_string()], "review show").is_err());
    }

    #[test]
    fn extract_location_globals_repo_and_json() {
        let args: Vec<String> = ["--json", "--repo", "D:/some/repo", "list"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (location, json, rest) = extract_location_globals(&args).expect("should parse");
        assert_eq!(
            location,
            Some(RepoLocation::Local(PathBuf::from("D:/some/repo")))
        );
        assert!(json);
        assert_eq!(rest, vec!["list".to_string()]);
    }

    #[test]
    fn extract_location_globals_never_steals_a_value_that_looks_global() {
        // `--body "--json"` / `--author "--repo"`: the value tokens must
        // ride through verbatim, NOT toggle json mode or eat the next arg.
        let args: Vec<String> = [
            "add", "--body", "--json", "--author", "--repo", "--file", "a.rs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (location, json, rest) = extract_location_globals(&args).expect("should parse");
        assert_eq!(location, None);
        assert!(!json, "a --json sitting in a value slot is not the global");
        assert_eq!(rest, args);
    }

    #[test]
    fn extract_location_globals_still_finds_globals_after_value_flags() {
        let args: Vec<String> = ["add", "--body", "hello", "--json", "--repo", "D:/r"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (location, json, rest) = extract_location_globals(&args).expect("should parse");
        assert_eq!(location, Some(RepoLocation::Local(PathBuf::from("D:/r"))));
        assert!(json);
        assert_eq!(
            rest,
            vec!["add".to_string(), "--body".to_string(), "hello".to_string()]
        );
    }

    #[test]
    fn extract_location_globals_rejects_both_repo_and_wsl() {
        let args: Vec<String> = ["--repo", "a", "--wsl", "Ubuntu:/b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(extract_location_globals(&args).is_err());
    }

    #[test]
    fn parse_comment_list_flags() {
        let args: Vec<String> = ["--file", "a.rs", "--status", "open"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (file, status, review) = parse_comment_list(&args).expect("should parse");
        assert_eq!(file.as_deref(), Some("a.rs"));
        assert_eq!(status, Some(CommentStatus::Open));
        assert!(review.is_none());
    }

    #[test]
    fn parse_comment_list_bad_status_is_error() {
        let args: Vec<String> = ["--status", "nope"].iter().map(|s| s.to_string()).collect();
        assert!(parse_comment_list(&args).is_err());
    }

    #[test]
    fn parse_comment_reply_requires_comment_id_and_body() {
        assert!(parse_comment_reply(&[]).is_err());
        let args: Vec<String> = vec!["c-1-abcd".to_string()];
        assert!(parse_comment_reply(&args).is_err(), "missing --body");

        let args: Vec<String> = ["c-1-abcd", "--body", "hi"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (id, body, author, review) = parse_comment_reply(&args).expect("should parse");
        assert_eq!(id, "c-1-abcd");
        assert_eq!(body, "hi");
        assert!(author.is_none());
        assert!(review.is_none());
    }

    #[test]
    fn format_ms_epoch_and_known_date() {
        assert_eq!(format_ms(0), "1970-01-01 00:00:00 UTC");
        // 2021-01-01T00:00:00Z
        assert_eq!(format_ms(1_609_459_200_000), "2021-01-01 00:00:00 UTC");
    }
}
