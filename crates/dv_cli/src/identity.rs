//! Who a command acts as (docs/PLAN.md §2.5, §10): an agent named by `--author <name>`, else
//! by `$DV_AUTHOR`, else `claude-code` when `CLAUDECODE` is set (Claude Code exports it to the
//! commands it runs); otherwise the reviewer, by the repository's `user.name`, as the GUI
//! writes (`dv_core::review::reviewer_identity`). What each may do is `review::ops`'s rule.
//!
//! An agent also says which model it runs on, in the model's own words: `--model <name>`, else
//! `$DV_MODEL`. It is what the app shows for the agent, never who the agent is. `--model`
//! without an agent is a usage error; `$DV_MODEL`, like every variable here, is only read
//! when the command acts as an agent.

use dv_core::backend::Backend;
use dv_core::review::{Author, reviewer_identity};

use crate::args::GlobalArgs;
use crate::context::Context;
use crate::error::CliError;

/// The agent name Claude Code's commands act as by default.
pub const CLAUDE_CODE: &str = "claude-code";

/// The agent the flags and the environment name, if any: `--author`, `$DV_AUTHOR`, then
/// `CLAUDECODE`, on the model `--model` or `$DV_MODEL` names. An empty `--author` or `--model`
/// is a usage error, as `--model` without an agent is; an empty `$DV_AUTHOR` or `$DV_MODEL`
/// counts as unset.
pub(crate) fn agent(global: &GlobalArgs, ctx: &Context) -> Result<Option<Author>, CliError> {
    let agent = if let Some(name) = &global.author {
        Some(Author::agent(one_line(name, "--author", "an agent name")?))
    } else if let Some(name) = ctx.env.non_empty("DV_AUTHOR") {
        Some(Author::agent(one_line(
            &name,
            "$DV_AUTHOR",
            "an agent name",
        )?))
    } else if ctx.env.non_empty("CLAUDECODE").is_some() {
        Some(Author::agent(CLAUDE_CODE))
    } else {
        None
    };
    let model = match &global.model {
        Some(model) => Some(one_line(model, "--model", "a model name")?),
        None => ctx
            .env
            .non_empty("DV_MODEL")
            .map(|model| one_line(&model, "$DV_MODEL", "a model name"))
            .transpose()?,
    };
    match (agent, model) {
        (Some(agent), Some(model)) => Ok(Some(agent.with_model(model))),
        (None, Some(_)) if global.model.is_some() => Err(CliError::usage(
            "--model names the model an agent runs on: pass --author <agent name> with it",
        )),
        (agent, _) => Ok(agent),
    }
}

/// Who the command acts as: [`agent`], else the reviewer (one `git config` call).
pub(crate) fn author(
    global: &GlobalArgs,
    ctx: &Context,
    backend: &dyn Backend,
) -> Result<Author, CliError> {
    Ok(match agent(global, ctx)? {
        Some(agent) => agent,
        None => reviewer_identity(backend),
    })
}

/// The agent a command that only agents run acts as (`command`: `dv review done`, `dv comment
/// annotate`); without an agent identity it is a usage error (exit 2) that says how to give
/// one: "`command` is an agent's `role`: pass --author …; `otherwise`", what the reviewer runs
/// instead.
pub(crate) fn require_agent(
    global: &GlobalArgs,
    ctx: &Context,
    command: &str,
    role: &str,
    otherwise: &str,
) -> Result<Author, CliError> {
    agent(global, ctx)?.ok_or_else(|| {
        CliError::usage(format!(
            "`{command}` is an agent's {role}: pass --author <agent name> (claude-code, codex, \
             …); {otherwise}"
        ))
    })
}

/// `text` trimmed, when it is one line that says something; else a usage error naming where
/// it came from (`source`) and what it should be (`what`: "an agent name").
fn one_line(text: &str, source: &str, what: &str) -> Result<String, CliError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(CliError::usage(format!("{source} needs {what}")));
    }
    if text.chars().any(char::is_control) {
        return Err(CliError::usage(format!(
            "{source}: {what} is one line of text"
        )));
    }
    Ok(text.to_string())
}
