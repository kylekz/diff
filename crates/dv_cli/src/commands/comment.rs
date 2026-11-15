//! `dv comment list` and `dv comment show` (docs/PLAN.md §10.1, §8): threads with where their
//! code stands in the review's current diff, so an agent acts on `anchor.current`, never on a
//! stale line number. The text prints each thread as a block with the diff around its code now
//! (`format::thread_block`, PLAN §10.3); `--oneline` lists one line per thread.

use std::collections::HashSet;

use dv_core::review::{Thread, ThreadState};

use super::format;
use crate::args::{CommentListArgs, GlobalArgs, ThreadStatusArg};
use crate::context::Context;
use crate::dto::{CommentListDoc, CommentShowDoc, ThreadDto};
use crate::error::{CliError, Exit};
use crate::output;
use crate::placement;
use crate::repo::Repo;
use crate::select;

/// `dv comment list [--status …] [--file <p>] [--oneline]`: the review's live threads in the
/// order they were opened; `--status` keeps those in any of the states named (`outdated`: whose
/// code can't be found now), `--file` those on that file (its current path, or the one the
/// thread was written on). The text is the review's line, then each thread's block, or with
/// `--oneline` each thread's header and its comment's first line.
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
    let located = placement::locate(&repo, &review, &threads, ctx)?;
    let states: HashSet<ThreadStatusArg> = args.status.iter().copied().collect();
    let in_state = |thread: &Thread, outdated: bool| {
        states.is_empty()
            || states.iter().any(|state| match state {
                ThreadStatusArg::Open => thread.state == ThreadState::Open,
                ThreadStatusArg::Addressed => thread.state == ThreadState::Addressed,
                ThreadStatusArg::Resolved => thread.state == ThreadState::Resolved,
                ThreadStatusArg::Annotation => thread.state == ThreadState::Annotation,
                ThreadStatusArg::Outdated => outdated,
            })
    };
    // A thread is on `--file` by its file in the current diff (a rename's new path), where its
    // code is now, or any path it was written or recorded on.
    let on_file = |thread: &Thread, dto: &ThreadDto| {
        file.as_deref().is_none_or(|file| {
            let recorded = [&thread.anchor, thread.first_anchor()];
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
    let dtos: Vec<ThreadDto> = threads
        .iter()
        .zip(&located)
        .map(|(thread, located)| (thread, ThreadDto::new(&review, thread, located)))
        .filter(|(thread, dto)| in_state(thread, dto.outdated) && on_file(thread, dto))
        .map(|(_, dto)| dto)
        .collect();
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

/// `dv comment show <id>`: one thread, found in whichever review holds it (PLAN §10.2).
pub(crate) fn show(global: &GlobalArgs, id: &str, ctx: &mut Context) -> Result<Exit, CliError> {
    let repo = Repo::open(global, ctx)?;
    let (review, id) = select::thread(&repo, global, id)?;
    let thread = select::live_thread(&review, &id)?;
    let located = placement::locate(&repo, &review, &[thread], ctx)?;
    let doc = CommentShowDoc {
        thread: ThreadDto::new(&review, thread, &located[0]),
    };
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
