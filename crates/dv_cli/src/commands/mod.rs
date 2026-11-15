//! Each command's work (docs/PLAN.md §10.1): [`run`] dispatches.
//!
//! - Reading: [`status`], [`review`] (`list`, `show`) and [`comment`] (`list`, `show`).
//! - Writing: [`review_write`] (`new`, `done`, `submit`, `close`, `delete`), [`pr`] (`new
//!   --pr`, `refresh`: a GitHub pull request's review, PLAN §9.10) and
//!   [`comment_write`] (`add`, `annotate`, `reply`, `address`, `resolve`, `reopen`, `edit`,
//!   `delete`).
//! - Waiting for the reviewer: [`wait`].
//! - Setup (PLAN §12): the checks every environment lists, [`doctor`]; their fixes, [`setup`]
//!   (`agents`, `path`, `all`); installing dv in a WSL distro, [`setup_wsl`].
//! - Serving the app's remote backend over stdio: [`backend`].
//!
//! Commands of later versions are recognised and refused with what to do instead ([`later`]).
//! On Windows, a command for a repository inside a WSL distro runs there instead
//! (`crate::reexec`, PLAN §10.4).

mod backend;
mod comment;
pub(crate) mod comment_write;
pub(crate) mod doctor;
mod format;
mod later;
mod pr;
mod review;
pub(crate) mod review_write;
pub(crate) mod setup;
mod setup_wsl;
mod status;
pub(crate) mod wait;

use dv_core::review::ThreadState;

use crate::args::{Command, CommentCommand, GlobalArgs, ReviewCommand, SetupCommand};
use crate::context::Context;
use crate::error::{CliError, Exit};
use crate::reexec;

/// Runs `command`, printing its output; an error is returned for the caller to report. A
/// command for a WSL repository runs inside its distro when this is Windows (PLAN §10.4).
pub(crate) fn run(
    global: &GlobalArgs,
    command: Command,
    ctx: &mut Context,
) -> Result<Exit, CliError> {
    if let Some((distro, path)) = reexec::wsl_repository(global, &command, ctx)? {
        return reexec::run(&distro, &path, ctx);
    }
    match command {
        Command::Status => status::status(global, ctx),
        Command::Review(command) => match command {
            ReviewCommand::List(args) => review::list(global, &args, ctx),
            ReviewCommand::Show(args) => review::show(global, args.id.as_deref(), ctx),
            ReviewCommand::New(args) => match &args.pr {
                Some(reference) => pr::new(global, reference, ctx),
                None => review_write::new(global, &args, ctx),
            },
            ReviewCommand::Refresh(args) => pr::refresh(global, args.id.as_deref(), ctx),
            ReviewCommand::Wait(args) => wait::wait(global, &args, ctx),
            ReviewCommand::Done(args) => review_write::done(global, &args, ctx),
            ReviewCommand::Submit(args) => review_write::submit(global, &args, ctx),
            ReviewCommand::Close(args) => review_write::close(global, &args.id, ctx),
            ReviewCommand::Delete(args) => review_write::delete(global, &args.id, ctx),
        },
        Command::Comment(command) => match command {
            CommentCommand::List(args) => comment::list(global, &args, ctx),
            CommentCommand::Show(args) => comment::show(global, &args.id, ctx),
            CommentCommand::Add(args) => comment_write::add(global, &args, ctx),
            CommentCommand::Annotate(args) => comment_write::annotate(global, &args, ctx),
            CommentCommand::Reply(args) => comment_write::reply(global, &args, ctx),
            CommentCommand::Address(args) => comment_write::set_state(
                global,
                &args.thread.id,
                ThreadState::Addressed,
                Some(&args.note),
                ctx,
            ),
            CommentCommand::Resolve(args) => {
                comment_write::set_state(global, &args.id, ThreadState::Resolved, None, ctx)
            }
            CommentCommand::Reopen(args) => {
                comment_write::set_state(global, &args.id, ThreadState::Open, None, ctx)
            }
            CommentCommand::Edit(args) => comment_write::edit(global, &args, ctx),
            CommentCommand::Delete(args) => comment_write::delete(global, &args.id, ctx),
        },
        Command::Setup(args) => {
            let yes = args.yes.yes;
            match &args.what {
                Some(SetupCommand::Agents(agents)) => setup::agents(global, agents, yes, ctx),
                Some(SetupCommand::Path(path)) => setup::path(global, path, yes, ctx),
                Some(SetupCommand::Wsl(wsl)) => setup_wsl::wsl(global, wsl, yes, ctx),
                Some(SetupCommand::All(all)) => setup::all(global, yes || all.yes, ctx),
                None => setup::all(global, yes, ctx),
            }
        }
        Command::Doctor => doctor::doctor(global, ctx),
        Command::Backend(args) => backend::backend(&args, ctx),
    }
}
