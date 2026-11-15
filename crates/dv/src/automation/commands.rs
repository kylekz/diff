//! The commands that open repositories and reviews, the one that plays the agent, the one that
//! writes into the run's scratch home, and the one that makes an update of dv ready:
//!
//! - `open {path}`: as `dv open <path>` starts the app (`Shell::open_repo`): the repository
//!   becomes a known one (its worktree root; a WSL path as given), its switcher row is
//!   computed, and the switcher opens with its first row selected; one that doesn't open is an
//!   error and the main area's banner. Replies `{path, location}`.
//! - `open_review {path, target?}`: opens or resumes the review of `target` (default
//!   `"working"`; any target in its stored shape, e.g. `"staged"` or
//!   `{"branch": {"branch": "feat", "base": null}}`) and shows it, through the shell as the
//!   switcher would. Replies `{review_id, created}`; `settle` then waits for its file list.
//! - `open_pr {path, pr}`: opens the pull request `pr` names (`7`, `#7`, `owner/repo#7` or its
//!   URL) in the repository at `path` as a review, through the shell as `dv open --pr` does
//!   (PLAN §9.3: its open draft, its review submitted at the head, or a new draft). Replies
//!   `{review_id, created}`; an open that fails is an error and the main area's banner.
//! - `pr_refresh {}`: starts a refresh of the open PR review (PLAN §9.4), as the reviewer's
//!   Refresh does; `settle` then waits for it, and `state.session.pr` says how it went.
//! - `review_op {op, thread, body?, note?, author?}`: an agent's change to the open review
//!   straight through `dv_core::review::ops`, without the CLI's own rules (scenarios that play
//!   the agent through the real CLI use `cli`, `agent_cli`): `reply` (with `body`), `address`
//!   (with an optional `note`) or `done` (the review's status to `ready`, as `dv review done`;
//!   no `thread`). `author` is the agent's name (`agent` by default). The open review sees the
//!   change through its watch, as it would an agent's, so scenarios `wait_for` it.
//! - `home_file {path, contents}`: a file in the run's scratch home ([`home_file`]).
//! - `update_ready {version, others?, fail?}`: an update of dv ready to restart into, through
//!   the seam, with a stand-in for the install ([`update_ready`]).

use anyhow::{Context as _, Result, anyhow, bail};
use dv_core::github::PrRef;
use dv_core::review::{Author, ReviewStatus, ThreadId, ThreadState, ops};
use dv_core::{Location, Target};
use dv_ui::Shell;
use dv_ui::services::reviews::OpenRequest;
use dv_ui::shell::IfNotOpened;
use gpui::{AppContext as _, Context, Entity, Task, Window};
use serde_json::{Map, Value, json};

use super::DvAutomation;

/// `args` as an object whose keys are all in `known`.
pub(super) fn object(cmd: &str, args: Value, known: &[&str]) -> Result<Map<String, Value>> {
    let object = match args {
        Value::Object(object) => object,
        Value::Null => Map::new(),
        other => bail!("{cmd} takes an object, not {other}"),
    };
    if let Some(unknown) = object.keys().find(|key| !known.contains(&key.as_str())) {
        bail!("{cmd} takes {}, not {unknown:?}", known.join(", "));
    }
    Ok(object)
}

pub(super) fn string<'a>(cmd: &str, args: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("{cmd} needs a string {key:?}"))
}

/// `open {path}`: [`Shell::open_repo`], as `dv open <path>` starts the app.
pub fn open(
    shell: Entity<Shell>,
    args: Value,
    window: &mut Window,
    cx: &mut Context<DvAutomation>,
) -> Result<Task<Result<Value>>> {
    let args = object("open", args, &["path"])?;
    let location = Location::parse(string("open", &args, "path")?)?;
    let opening = shell.update(cx, |shell, cx| {
        shell.open_repo(location, IfNotOpened::Banner, window, cx)
    });
    Ok(cx.background_spawn(async move {
        let known = opening.await.map_err(|failure| anyhow!("{failure}"))?;
        Ok(json!({"path": known.display_name(), "location": known}))
    }))
}

/// The `target` of `open_review`: `working` when absent.
fn target(args: &Map<String, Value>) -> Result<Target> {
    match args.get("target") {
        None | Some(Value::Null) => Ok(Target::Working),
        Some(value) => serde_json::from_value(value.clone()).with_context(|| {
            format!(
                "{value} isn't a target (\"working\", \"staged\", \
                 {{\"branch\": {{\"branch\": \"feat\", \"base\": null}}}}, …)"
            )
        }),
    }
}

/// `open_review {path, target?}`.
pub fn open_review(
    shell: Entity<Shell>,
    args: Value,
    window: &mut Window,
    cx: &mut Context<DvAutomation>,
) -> Result<Task<Result<Value>>> {
    let args = object("open_review", args, &["path", "target"])?;
    let location = Location::parse(string("open_review", &args, "path")?)?;
    let target = target(&args)?;
    let opening = shell.update(cx, |shell, cx| {
        shell.open(location, OpenRequest::Target(target), window, cx)
    });
    Ok(cx.background_spawn(async move {
        let opened = opening.await.map_err(|failure| anyhow!("{failure}"))?;
        Ok(json!({"review_id": opened.review, "created": opened.created}))
    }))
}

/// `open_pr {path, pr}`.
pub fn open_pr(
    shell: Entity<Shell>,
    args: Value,
    window: &mut Window,
    cx: &mut Context<DvAutomation>,
) -> Result<Task<Result<Value>>> {
    let args = object("open_pr", args, &["path", "pr"])?;
    let location = Location::parse(string("open_pr", &args, "path")?)?;
    let reference: PrRef = string("open_pr", &args, "pr")?.parse()?;
    let opening = shell.update(cx, |shell, cx| {
        shell.open(location, OpenRequest::Pr(reference), window, cx)
    });
    Ok(cx.background_spawn(async move {
        let opened = opening.await.map_err(|failure| anyhow!("{failure}"))?;
        Ok(json!({"review_id": opened.review, "created": opened.created}))
    }))
}

/// `pr_refresh {}`: `ReviewSession::refresh_pr` on the open PR review.
pub fn pr_refresh(
    shell: &Entity<Shell>,
    args: &Value,
    cx: &mut Context<DvAutomation>,
) -> Result<Value> {
    object("pr_refresh", args.clone(), &[])?;
    let session = shell
        .read(cx)
        .session()
        .cloned()
        .filter(|session| session.read(cx).review().is_pr_review())
        .context("pr_refresh needs an open PR review (open_pr first)")?;
    session.update(cx, |session, cx| session.refresh_pr(cx));
    Ok(json!({"refreshing": session.read(cx).is_refreshing_pr()}))
}

/// `review_op {op, thread, body?, note?, author?}`.
pub fn review_op(
    shell: Entity<Shell>,
    args: Value,
    cx: &mut Context<DvAutomation>,
) -> Result<Task<Result<Value>>> {
    let args = object(
        "review_op",
        args,
        &["op", "thread", "body", "note", "author"],
    )?;
    let session = shell
        .read(cx)
        .session()
        .cloned()
        .context("review_op needs an open review (open_review first)")?;
    let (store, review) = {
        let session = session.read(cx);
        (session.store().clone(), session.review_id().clone())
    };
    let author = Author::agent(
        args.get("author")
            .and_then(Value::as_str)
            .unwrap_or("agent"),
    );
    let thread = || -> Result<ThreadId> {
        let id = string("review_op", &args, "thread")?;
        ThreadId::parse(id).map_err(|error| anyhow!("{error}"))
    };
    let op = string("review_op", &args, "op")?;
    let job: Box<dyn FnOnce() -> Result<Value> + Send> = match op {
        "reply" => {
            let (thread, body) = (thread()?, string("review_op", &args, "body")?.to_string());
            Box::new(move || {
                let id = ops::reply(&store, &review, &thread, &body, author, None)?;
                Ok(json!({"comment": id}))
            })
        }
        "address" => {
            let thread = thread()?;
            let note = args.get("note").and_then(Value::as_str).map(str::to_string);
            Box::new(move || {
                let changed = ops::set_thread_state(
                    &store,
                    &review,
                    &thread,
                    ThreadState::Addressed,
                    note.as_deref(),
                    author,
                    None,
                )?;
                Ok(json!({"changed": changed}))
            })
        }
        "done" => {
            let note = args.get("note").and_then(Value::as_str).map(str::to_string);
            Box::new(move || {
                let changed = ops::set_status(
                    &store,
                    &review,
                    ReviewStatus::Ready,
                    &author,
                    note.as_deref(),
                )?;
                Ok(json!({"changed": changed}))
            })
        }
        other => bail!("review_op's op is \"reply\", \"address\" or \"done\", not {other:?}"),
    };
    Ok(cx.background_spawn(async move { job() }))
}

/// `home_file {path, contents}`: writes `contents` at `path` (relative, inside it) in the run's
/// scratch home, `<data_dir>/home` (`crate::app::AUTOMATION_HOME`), its folders made: a fake
/// agent's folder (`.claude/settings.json`), a skill an older dv installed. Replies `{path}`,
/// the file's absolute path. The setup checks see it once they run again (`setup/check_again`).
pub fn home_file(args: &Value, cx: &mut Context<DvAutomation>) -> Result<Value> {
    let args = object("home_file", args.clone(), &["path", "contents"])?;
    let path = string("home_file", &args, "path")?;
    let contents = string("home_file", &args, "contents")?;
    let relative = std::path::Path::new(path);
    let inside = !path.is_empty()
        && relative.is_relative()
        && relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)));
    if !inside {
        bail!("home_file's \"path\" is a relative path inside the home, not {path:?}");
    }
    use dv_ui::ActiveServices as _;
    let file = cx
        .services()
        .data_dir()
        .join(crate::app::AUTOMATION_HOME)
        .join(relative);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&file, contents).with_context(|| format!("writing {}", file.display()))?;
    Ok(json!({"path": file.display().to_string()}))
}

/// `update_ready {version, others?, fail?}`: an update of dv downloaded and ready, through the
/// seam an installed copy's updates report to (`dv_ui::services::Updates`, PLAN D9), with a
/// stand-in for the install: `others` other dv processes running from it (default 0), which
/// Restart to update asks about first, and `fail` the error its restart reports (default none:
/// it is recorded, never done). The status bar shows Restart to update; clicking it
/// (`status/update/restart`) sets `state.update.restart_requested` or opens the confirmation.
/// Again with other arguments, the stand-in changes. Replies `state.update`.
pub fn update_ready(args: &Value, cx: &mut Context<DvAutomation>) -> Result<Value> {
    let args = object("update_ready", args.clone(), &["version", "others", "fail"])?;
    let version = string("update_ready", &args, "version")?.trim().to_string();
    if version.is_empty() {
        bail!("update_ready needs a non-empty \"version\"");
    }
    let others = match args.get("others") {
        None | Some(Value::Null) => 0,
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .with_context(|| format!("update_ready's \"others\" is a count, not {value}"))?,
    };
    let fail = match args.get("fail") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => bail!("update_ready's \"fail\" is a string, not {other}"),
    };
    use dv_ui::ActiveServices as _;
    let updates = cx.services().updates.clone();
    updates.update(cx, |updates, cx| {
        updates.set_updater(dv_ui::services::Updater::Scripted { others, fail });
        updates.set_ready(version, cx);
    });
    Ok(super::state::update(cx))
}
