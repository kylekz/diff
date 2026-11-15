//! The repository a command works on (docs/PLAN.md §10.1, §10.4): `--wsl <distro>:<path>`,
//! else `--repo <path>`, else the working directory's. On Windows a command for a WSL location
//! runs inside its distro before it gets here (`crate::reexec`). Inside that distro, a location
//! of the distro itself is its local path (a `--repo` or a `--file` spelled through
//! `\\wsl.localhost\<this distro>\…` too), a Windows drive's path given as `--repo` is that
//! drive as WSL mounts it (`D:\code` → `/mnt/d/code`), and any other WSL location is exit 4 at
//! once.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use dv_core::backend::{Backend, LocalBackend, LocalBackendOptions};
use dv_core::git::GitRepo;
use dv_core::github::Gh;
use dv_core::label::Labels;
use dv_core::location::{Location, mount_path};
use dv_core::review::{Home, HomeError, Review, ReviewStore, home, same_worktree as home_is};

use crate::args::GlobalArgs;
use crate::context::{Context, Env};
use crate::dto::ReviewDto;
use crate::error::{CliError, Exit, code};
use crate::output::windows_arg;
use crate::pr::StoredPrs;

/// An open repository: its backend, git queries over it, its review store, the `gh` the
/// command reaches GitHub with (`crate::pr::gh`; nothing runs until a command asks), and
/// what its PR reviews are printed from, read when the first one is.
#[derive(Clone)]
pub(crate) struct Repo {
    pub backend: Arc<dyn Backend>,
    pub git: GitRepo,
    pub store: ReviewStore,
    pub gh: Arc<Gh>,
    stored_prs: Arc<OnceLock<StoredPrs>>,
}

impl Repo {
    /// The repository the command means (module docs), opened with the context's git
    /// environment.
    pub fn open(global: &GlobalArgs, ctx: &Context) -> Result<Self, CliError> {
        let location = location(global, &ctx.cwd, this_distro(&ctx.env).as_deref())?;
        let path = match location {
            Location::Local(path) => path,
            Location::Wsl { distro, path } if in_distro(ctx, &distro) => path.into(),
            Location::Wsl { distro, path } => return Err(elsewhere(ctx, &distro, &path)),
        };
        Self::open_at(Path::new(&path), ctx)
    }

    /// The work tree at (or containing) `path`, with the context's git environment.
    fn open_at(path: &Path, ctx: &Context) -> Result<Self, CliError> {
        let backend: Arc<dyn Backend> = Arc::new(LocalBackend::open_with(
            path,
            LocalBackendOptions {
                env: ctx.git_env.clone(),
            },
        )?);
        Ok(Self {
            git: GitRepo::new(backend.clone()),
            store: ReviewStore::new(backend.clone()),
            backend,
            gh: Arc::new(crate::pr::gh(ctx)),
            stored_prs: Arc::default(),
        })
    }

    /// Where `review`'s target resolves: its home worktree (`dv_core::review::home`, PLAN §2.2,
    /// §2.4) — for a `working` or `staged` review the worktree it was made in, for a `branch`
    /// review the worktree that has the branch checked out — which may be another worktree of
    /// this repository, the same one the GUI resolves it in; any other target resolves the same
    /// from every worktree, so here. A home worktree that is gone (removed, moved, or deleted)
    /// is exit 1 (`target`): what the review is of can't be read anywhere else.
    pub fn for_review(&self, review: &Review, ctx: &Context) -> Result<Self, CliError> {
        let home = match home(&self.git, &review.target, &review.worktree) {
            Ok(home) => home,
            Err(HomeError::Git(error)) => return Err(error.into()),
            Err(gone @ HomeError::Gone { .. }) => {
                return Err(CliError::new(
                    Exit::Failure,
                    code::TARGET,
                    format!("review {} can't be read: {gone}", review.id),
                ));
            }
        };
        let root = match home {
            Home::Worktree(root) if !home_is(&self.backend.repo().location, &root, self.root()) => {
                root
            }
            Home::Worktree(_) | Home::Anywhere => return Ok(self.clone()),
        };
        let there = Self::open_at(Path::new(&root), ctx).map_err(|error| CliError {
            message: format!(
                "review {} is read in the worktree {root}, which doesn't open: {}",
                review.id, error.message
            ),
            ..error
        })?;
        if !same_path_text(
            &there.backend.repo().common_dir,
            &self.backend.repo().common_dir,
        ) {
            return Err(CliError::new(
                Exit::Failure,
                code::TARGET,
                format!(
                    "review {} is read in the worktree {root}, which is another repository's now",
                    review.id
                ),
            ));
        }
        Ok(there)
    }

    /// The worktree root.
    pub fn root(&self) -> &str {
        &self.backend.repo().toplevel
    }

    /// `path`, a file of `review`, as a path from a worktree root, `/`-separated, as reviews
    /// record paths: an absolute path inside the worktree the review was made in (where a
    /// `working` or `staged` review is read, [`Repo::for_review`]) or the one the command runs
    /// in loses that root, the longer one when both hold it, `\` becomes `/`, and a leading
    /// `./` goes; any other path is taken as already relative to the root. Inside a WSL
    /// distro, a path spelled through that distro's share (`\\wsl.localhost\Ubuntu\…`) is its
    /// path there first. An absolute path whose spelling starts with neither root is compared
    /// again as the file system resolves it ([`resolved`]): through a Windows 8.3 short name
    /// (`RUNNER~1`) or a link, neither of which git's resolved root has.
    pub fn relative_path(&self, path: &str, review: &Review, ctx: &Context) -> String {
        let path = match Location::parse(path.trim()) {
            Ok(Location::Wsl { distro, path }) if in_distro(ctx, &distro) => path,
            _ => path.trim().replace('\\', "/"),
        };
        let roots = [review.worktree.as_str(), self.root()]
            .map(|root| format!("{}/", root.replace('\\', "/").trim_end_matches('/')));
        let resolved_path;
        let mut inside = inside_root(&path, &roots);
        if inside.is_none()
            && Path::new(&path).is_absolute()
            && let Some(long) = resolved(&path)
        {
            let long_roots = roots.clone().map(|root| {
                resolved(&root)
                    .map(|root| format!("{}/", root.trim_end_matches('/')))
                    .unwrap_or(root)
            });
            resolved_path = long;
            inside = inside_root(&resolved_path, &long_roots);
        }
        let mut rel = inside.unwrap_or(&path);
        while let Some(rest) = rel.strip_prefix("./") {
            rel = rest;
        }
        rel.to_string()
    }

    /// The names targets are labelled with, every worktree's `HEAD` included (blocking: a
    /// few git calls).
    pub fn labels(&self) -> Labels {
        Labels::read(&self.git)
    }

    /// The store's PR reviews and where their mirrors are, read the first time a command
    /// prints a PR review and kept for the rest of it: a command prints its reviews after
    /// whatever it writes.
    pub fn stored_prs(&self) -> &StoredPrs {
        self.stored_prs.get_or_init(|| StoredPrs::read(self))
    }

    /// `review` as the commands print it, a PR review with its pull request
    /// (`crate::pr::dto`).
    pub fn review_dto(&self, review: &Review, labels: &Labels) -> ReviewDto {
        ReviewDto::new(review, labels, crate::pr::dto(self, review))
    }
}

/// Where the repository is, from the flags or the working directory `cwd`, for a dv that runs
/// inside the WSL distro `here` ([`this_distro`]) or none. A relative `--repo` resolves against
/// `cwd`, and the path that makes is read as a location again, so a relative path from a
/// working directory inside WSL (`\\wsl.localhost\…`, where PowerShell can stand) is a WSL
/// location too. Inside a distro, a Windows drive's path (`D:\code`, `d:/code`) is that drive
/// as WSL mounts it (`/mnt/d/code`, `dv_core::location::mount_path`) rather than a relative
/// path. Nothing is opened: a WSL location is returned as it is.
pub fn location(global: &GlobalArgs, cwd: &Path, here: Option<&str>) -> Result<Location, CliError> {
    if let Some(wsl) = &global.wsl {
        return Location::parse_wsl(wsl)
            .map_err(|error| CliError::usage(format!("--wsl: {error}")));
    }
    let (text, from_flag) = match &global.repo {
        Some(repo) => (repo.clone(), true),
        None if cwd.as_os_str().is_empty() => {
            return Err(CliError::new(
                Exit::Failure,
                code::IO,
                "the current directory can't be read; pass --repo <path>",
            ));
        }
        None => (cwd.to_string_lossy().into_owned(), false),
    };
    let location = Location::parse(&text).map_err(|error| {
        CliError::usage(if from_flag {
            format!("--repo: {error}")
        } else {
            error.to_string()
        })
    })?;
    // A drive's path is no WSL location, so it parsed as a local one.
    if let Some(mounted) = here.and_then(|_| mount_path(&text)) {
        return Ok(Location::Local(mounted.into()));
    }
    match location {
        Location::Local(path) if path.is_relative() => {
            let joined = cwd.join(&path);
            Ok(Location::parse(&joined.to_string_lossy()).unwrap_or(Location::Local(joined)))
        }
        other => Ok(other),
    }
}

/// What of `path` follows the longest of `roots` (each ending in `/`) that starts it.
fn inside_root<'a>(path: &'a str, roots: &[String]) -> Option<&'a str> {
    roots
        .iter()
        .filter(|root| {
            path.get(..root.len())
                .is_some_and(|head| same_path_text(head, root))
        })
        .max_by_key(|root| root.len())
        .map(|root| &path[root.len()..])
}

/// `path` as the file system resolves it, `/`-separated: its longest existing ancestor
/// canonicalized (long names, links followed, Windows' `\\?\` prefix taken off) with the rest
/// after it as written, since a file the review deleted needn't exist. `None` when no part of
/// it resolves.
fn resolved(path: &str) -> Option<String> {
    let mut rest = Vec::new();
    let mut ancestor = Path::new(path);
    let base = loop {
        if let Ok(base) = std::fs::canonicalize(ancestor) {
            break base;
        }
        rest.push(ancestor.file_name()?);
        ancestor = ancestor.parent()?;
    };
    let mut text = base.to_string_lossy().replace('\\', "/");
    if let Some(unc) = text.strip_prefix("//?/UNC/") {
        text = format!("//{unc}");
    } else if let Some(local) = text.strip_prefix("//?/") {
        text = local.to_string();
    }
    for part in rest.iter().rev() {
        text = format!("{}/{}", text.trim_end_matches('/'), part.to_string_lossy());
    }
    Some(text)
}

/// Whether two spellings of a path are the same: ignoring case on Windows and macOS, whose
/// file systems usually do.
fn same_path_text(a: &str, b: &str) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// The WSL distro this dv runs in: `$WSL_DISTRO_NAME` in `env`, on Linux, when it names one.
pub fn this_distro(env: &Env) -> Option<String> {
    env.var("WSL_DISTRO_NAME")
        .filter(|here| cfg!(target_os = "linux") && !here.trim().is_empty())
}

/// Whether this dv runs inside the WSL distro `distro` ([`this_distro`], naming it without
/// regard to case, as `wsl.exe` takes names), where its paths are local ones.
fn in_distro(ctx: &Context, distro: &str) -> bool {
    this_distro(&ctx.env).is_some_and(|here| here.eq_ignore_ascii_case(distro))
}

/// The exit-4 error for a repository in a WSL distro this dv can't reach: another distro than
/// the one it runs in, or any distro where there is no WSL (Linux, macOS). Only dv on Windows
/// reaches every distro, running the command inside it (PLAN §10.4), so that is what it says
/// to use, quoted to paste there ([`windows_arg`]); inside a distro `wsl.exe -d` would need a
/// dv installed in the other one.
fn elsewhere(ctx: &Context, distro: &str, path: &str) -> CliError {
    let location = format!("{distro}:{path}");
    let quoted = windows_arg(&location);
    let reach = match this_distro(&ctx.env) {
        Some(here) => format!("which dv in {here} can't reach"),
        None => "which only dv on Windows reaches".to_string(),
    };
    CliError::unavailable(format!(
        "{location} is in the WSL distro {distro}, {reach}: run the command from Windows \
         (`dv.exe --wsl {quoted} …`), which runs it inside {distro}"
    ))
}
