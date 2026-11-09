//! git's identity (docs/PLAN.md §12.2 item 5): `user.name` and `user.email` as git resolves
//! them outside a repository (global configuration with its includes, then the system's unless
//! `GIT_CONFIG_NOSYSTEM` is set for the checks, as a test's and an automation run's is), which
//! dv's comments are signed with. dv never changes git's configuration: the fix is the command to run.

use std::time::Duration;

use toolbox_process::Command;

use super::process::{self, apply_env};
use super::{Check, Fix, Seams, State, id};

const TITLE: &str = "git identity";

/// How long a `git config` may take.
const TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn check(seams: &Seams) -> Check {
    let mut name = None;
    let mut email = None;
    let no_system = seams
        .child_env
        .iter()
        .any(|(name, value)| name == "GIT_CONFIG_NOSYSTEM" && value.is_some());
    let scopes: &[&str] = if no_system {
        &["--global"]
    } else {
        &["--global", "--system"]
    };
    for scope in scopes {
        match read(seams, scope) {
            Ok(found) => {
                name = name.or(found.0);
                email = email.or(found.1);
            }
            Err(reason) => return Check::new(id::GIT, TITLE, State::Failed, reason),
        }
    }
    match (name, email) {
        (Some(name), Some(email)) => {
            Check::new(id::GIT, TITLE, State::Ok, format!("{name} <{email}>"))
        }
        (name, email) => {
            let mut unset = Vec::new();
            let mut commands = Vec::new();
            if name.is_none() {
                unset.push("user.name");
                commands.push("git config --global user.name \"Your Name\"");
            }
            if email.is_none() {
                unset.push("user.email");
                commands.push("git config --global user.email \"you@example.com\"");
            }
            Check::new(
                id::GIT,
                TITLE,
                State::Missing,
                format!(
                    "{} {} set, so git can't say who wrote a commit or a comment",
                    unset.join(" and "),
                    if unset.len() == 1 { "isn't" } else { "aren't" }
                ),
            )
            .with_fix(Fix::command(commands.join("; ")))
        }
    }
}

/// `user.name` and `user.email` in one configuration scope.
fn read(seams: &Seams, scope: &str) -> Result<(Option<String>, Option<String>), String> {
    let command = Command::new("git").args(["config", scope]);
    let command = if scope == "--global" {
        // `[include]`d files hold an identity as often as the file itself.
        command.arg("--includes")
    } else {
        command
    };
    let command = apply_env(
        command.args(["--get-regexp", r"^user\.(name|email)$"]),
        &seams.child_env,
    )
    .env("LC_ALL", "C");
    let ran = process::run(command, TIMEOUT).map_err(|error| {
        if process::not_found(&error) {
            "git isn't installed, or isn't on PATH".to_string()
        } else {
            format!("git config failed: {error}")
        }
    })?;
    match ran.code {
        // 1: none set.
        Some(0) | Some(1) => {}
        _ => {
            return Err(format!(
                "git config {scope} failed: {}",
                ran.stderr.trim().lines().next().unwrap_or("")
            ));
        }
    }
    let mut found = (None, None);
    for line in ran.stdout.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key {
            "user.name" => found.0 = Some(value.to_string()),
            "user.email" => found.1 = Some(value.to_string()),
            _ => {}
        }
    }
    Ok(found)
}
