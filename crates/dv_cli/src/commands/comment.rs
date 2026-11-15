//! `dv comment list` and `dv comment show` (docs/PLAN.md §10.1, §8): threads with where their
//! code stands in the review's current diff, so an agent acts on `anchor.current`, never on a
//! stale line number. The text prints each thread as a block with the diff around its code now
//! (`format::thread_block`, PLAN §10.3); `--oneline` lists one line per thread.

use std::collections::HashSet;

use dv_core::diff::FileEntry;
use dv_core::github::{GithubThread, Mirror};
use dv_core::review::{Anchor, Thread, ThreadState};

use super::format;
use crate::args::{CommentListArgs, GlobalArgs, ThreadStatusArg};
use crate::context::Context;
use crate::dto::{CommentListDoc, CommentShowDoc, ThreadDto};
use crate::error::{CliError, Exit};
use crate::output;
use crate::placement;
use crate::repo::Repo;
use crate::select::{self, Shown};

/// `dv comment list [--status …] [--file <p>] [--oneline]`: the review's live threads in the
/// order they were opened; `--status` keeps those in any of the states named (`outdated`: whose
/// code can't be found now), `--file` those on that file (its current path, or the one the
/// thread was written on). The text is the review's line, then each thread's block, or with
/// `--oneline` each thread's header and its comment's first line.
///
/// A PR review lists GitHub's threads on its pull request after its own (PLAN §9.10), as the
/// stored mirror has them, in GitHub's order: `open` keeps those GitHub doesn't call
/// resolved, `resolved` those it does, `outdated` those without a place in the diff, and
/// `addressed` and `annotation` none of them. A pull request without a mirror to read here
/// has none to list, which a note on stderr says, with why.
///
/// Only the threads the mirror and the review's file list don't rule out are placed
/// ([`may_match`]): `--file` and a `--status` that GitHub's own flag answers read no file
/// for the others, and list what the whole listing shows.
pub(crate) fn list(
    global: &GlobalArgs,
    args: &CommentListArgs,
    ctx: &mut Context,
) -> Result<Exit, CliError> {
    let repo = Repo::open(global, ctx)?;
    let review = select::review(&repo, global, ctx, None)?;
    let file = args
        .file
        .as_deref()
        .map(|file| repo.relative_path(file, &review, ctx));
    let threads: Vec<&Thread> = review.live_threads().collect();
    let mirror = crate::pr::mirror(&repo, &review);
    let states: HashSet<ThreadStatusArg> = args.status.iter().copied().collect();
    let placement::LocatedThreads {
        own: located,
        github,
    } = match &mirror {
        Some(mirror) => placement::locate_with_github(
            &repo,
            &review,
            &threads,
            mirror,
            |thread, entries| may_match(mirror, thread, entries, &states, file.as_deref()),
            ctx,
        )?,
        None => placement::LocatedThreads {
            own: placement::locate(&repo, &review, &threads, ctx)?,
            github: Vec::new(),
        },
    };
    if review.is_pr_review() && mirror.is_none() {
        output::warning(
            ctx,
            &format!(
                "GitHub's threads on the pull request aren't listed: {}",
                crate::pr::why_no_mirror(&repo, &review)
            ),
        )?;
    }
    // A thread's state as `--status` names it: the review's own, or GitHub's word.
    let in_state = |state: ThreadState, outdated: bool| {
        states.is_empty()
            || states.iter().any(|wanted| match wanted {
                ThreadStatusArg::Open => state == ThreadState::Open,
                ThreadStatusArg::Addressed => state == ThreadState::Addressed,
                ThreadStatusArg::Resolved => state == ThreadState::Resolved,
                ThreadStatusArg::Annotation => state == ThreadState::Annotation,
                ThreadStatusArg::Outdated => outdated,
            })
    };
    // A thread is on `--file` by its file in the current diff (a rename's new path), where its
    // code is now, or any path it was written or recorded on.
    let on_file = |recorded: &[&Anchor], dto: &ThreadDto| {
        file.as_deref().is_none_or(|file| {
            dto.path == file
                || dto
                    .anchor
                    .current
                    .as_ref()
                    .is_some_and(|current| current.path == file)
                || recorded
                    .iter()
                    .any(|anchor| anchor.path == file || anchor.old_path.as_deref() == Some(file))
        })
    };
    let own = threads
        .iter()
        .zip(&located)
        .map(|(thread, located)| (thread, ThreadDto::new(&review, thread, located)))
        .filter(|(thread, dto)| {
            in_state(thread.state, dto.outdated)
                && on_file(&[&thread.anchor, thread.first_anchor()], dto)
        })
        .map(|(_, dto)| dto);
    let host = mirror
        .as_ref()
        .map_or("", |mirror| mirror.key.host.as_str());
    let githubs = mirror
        .iter()
        .flat_map(|mirror| {
            github
                .iter()
                .map(|(index, anchored, located)| (&mirror.threads[*index], anchored, located))
        })
        .map(|(thread, anchored, located)| {
            (
                thread,
                anchored,
                ThreadDto::github(&review, host, thread, anchored, located),
            )
        })
        .filter(|(thread, anchored, dto)| {
            let state = if thread.resolved {
                ThreadState::Resolved
            } else {
                ThreadState::Open
            };
            in_state(state, dto.outdated) && on_file(&[&anchored.anchor], dto)
        })
        .map(|(_, _, dto)| dto);
    let dtos: Vec<ThreadDto> = own.chain(githubs).collect();
    let doc = CommentListDoc {
        review_id: review.id.to_string(),
        threads: dtos,
    };
    if global.json {
        output::json(ctx, &doc)?;
        return Ok(Exit::Ok);
    }
    if doc.threads.is_empty() {
        let filtered = !states.is_empty() || file.is_some();
        let none = if filtered {
            "no matching threads"
        } else {
            "no threads"
        };
        output::text(ctx, &[none.to_string()])?;
        return Ok(Exit::Ok);
    }
    let lines: Vec<String> = if args.oneline {
        doc.threads
            .iter()
            .map(|thread| {
                format!(
                    "{}  {}",
                    format::thread_line(thread),
                    format::first_line(&thread.body)
                )
            })
            .collect()
    } else {
        let mut lines = vec![format::review_line(
            &repo.review_dto(&review, &repo.labels()),
        )];
        for thread in &doc.threads {
            lines.push(String::new());
            lines.extend(format::thread_block(thread));
        }
        lines
    };
    output::text(ctx, &lines)?;
    Ok(Exit::Ok)
}

/// Whether `thread`, one of `mirror`'s, can be among what `--status` (`states`) and `--file`
/// keep, as far as the mirror and the review's own file list (`entries`) tell: its state by
/// GitHub's own flag (`outdated` needs its place, so it rules nothing out); its file by the
/// names it may show under. Those are the path GitHub has it on and the names GitHub's file
/// list says that file is renamed from or to, and the review's own entry at `file` when
/// that entry's old or new path is the thread's path or its file's earlier one: the review
/// may call a rename what GitHub lists as a file removed and a file added. What passes is
/// placed, and filtered again by where it stands, so the filter changes what a listing
/// costs and never what it lists.
fn may_match(
    mirror: &Mirror,
    thread: &GithubThread,
    entries: &[FileEntry],
    states: &HashSet<ThreadStatusArg>,
    file: Option<&str>,
) -> bool {
    let in_state = states.is_empty()
        || states.iter().any(|wanted| match wanted {
            ThreadStatusArg::Open => !thread.resolved,
            ThreadStatusArg::Resolved => thread.resolved,
            ThreadStatusArg::Outdated => true,
            ThreadStatusArg::Addressed | ThreadStatusArg::Annotation => false,
        });
    let on_file = file.is_none_or(|file| {
        // The name GitHub's list says the thread's file is renamed from.
        let earlier = mirror
            .files
            .iter()
            .filter(|listed| listed.path == thread.path)
            .find_map(|listed| listed.previous_path.as_deref());
        let named = |path: &str| path == thread.path || Some(path) == earlier;
        named(file)
            || mirror.files.iter().any(|listed| {
                listed.path == file && listed.previous_path.as_deref() == Some(&thread.path)
            })
            || entries.iter().any(|entry| {
                let old = entry.old_path.as_deref();
                (entry.path == file || old == Some(file))
                    && (named(&entry.path) || old.is_some_and(named))
            })
    });
    in_state && on_file
}

/// `dv comment show <id>`: one thread, found in whichever review holds it (PLAN §10.2); for
/// `gh:<node id>`, one of GitHub's threads on a PR review's pull request (PLAN §9.10).
pub(crate) fn show(global: &GlobalArgs, id: &str, ctx: &mut Context) -> Result<Exit, CliError> {
    let repo = Repo::open(global, ctx)?;
    let (review, thread) = match select::shown(&repo, global, ctx, id)? {
        Shown::Local(review, id) => {
            let thread = select::live_thread(&review, &id)?;
            let located = placement::locate(&repo, &review, &[thread], ctx)?;
            let dto = ThreadDto::new(&review, thread, &located[0]);
            (review, dto)
        }
        Shown::Github(review, mirror, index) => {
            let thread = &mirror.threads[index];
            let (anchored, located) =
                placement::locate_github(&repo, &review, &mirror, thread, ctx)?;
            let dto = ThreadDto::github(&review, &mirror.key.host, thread, &anchored, &located);
            (review, dto)
        }
    };
    let doc = CommentShowDoc { thread };
    if global.json {
        output::json(ctx, &doc)?;
    } else {
        let mut lines = vec![
            format::review_line(&repo.review_dto(&review, &repo.labels())),
            String::new(),
        ];
        lines.extend(format::thread_block(&doc.thread));
        output::text(ctx, &lines)?;
    }
    Ok(Exit::Ok)
}
