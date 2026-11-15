//! Printing (docs/PLAN.md §10.3): with `--json` one JSON document on stdout ([`json`]), else
//! short stable lines without colour ([`text`]), what would break or redraw a line spelled
//! out (`escape`), as it is on stderr, where a message that holds a line break goes on
//! indented ([`message`]); errors always on stderr, and with `--json` also
//! as `{"schema": 1, "error": {…}}` on stdout ([`error`]), clap's own parse errors included
//! ([`parse_error`]).

use std::ffi::OsString;
use std::io::Write as _;

use serde::Serialize;

use crate::context::Context;
use crate::dto::{Document, ErrorDoc, ErrorDto};
use crate::error::{CliError, Exit, code};
use crate::escape;

/// Prints `body` as one compact JSON document, `"schema": 1` first, and a newline.
pub(crate) fn json<T: Serialize>(ctx: &mut Context, body: &T) -> Result<(), CliError> {
    let text = serde_json::to_string(&Document::new(body)).map_err(|error| {
        CliError::new(
            Exit::Failure,
            code::INTERNAL,
            format!("encoding JSON: {error}"),
        )
    })?;
    write_out(ctx, &format!("{text}\n"))
}

/// Prints `lines` on stdout, each ending in a newline and each one line whatever is in it
/// (`escape::line`): a path, a name or a comment dv didn't write can hold a line break or a
/// terminal escape, and may not start a line of its own or redraw the terminal (PLAN §10.3).
pub(crate) fn text(ctx: &mut Context, lines: &[String]) -> Result<(), CliError> {
    let mut out = String::new();
    for line in lines {
        out.push_str(&escape::line(line));
        out.push('\n');
    }
    write_out(ctx, &out)
}

fn write_out(ctx: &mut Context, text: &str) -> Result<(), CliError> {
    ctx.stdout
        .write_all(text.as_bytes())
        .and_then(|()| ctx.stdout.flush())
        .map_err(|error| {
            CliError::new(
                Exit::Failure,
                code::IO,
                format!("writing the output: {error}"),
            )
        })
}

/// `text` as one argument of a command dv suggests to paste on Windows (`dv.exe --wsl …`, `dv
/// open …`), where the user's shell is PowerShell or cmd: as it is when it holds only letters,
/// digits and the characters paths usually have; else in double quotes, which keep spaces and
/// the like together in both; but text with `$`, a backtick or a double quote, which PowerShell
/// expands or ends inside double quotes, in PowerShell's literal single quotes (a `'` in it
/// doubled), there being no quoting both shells read alike for those.
pub fn windows_arg(text: &str) -> String {
    let plain = !text.is_empty()
        && text.chars().all(|c| {
            c.is_alphanumeric() || matches!(c, '\\' | '/' | '.' | '_' | '-' | ':' | '~' | '+')
        });
    if plain {
        text.to_string()
    } else if text.contains(['$', '`', '"']) {
        format!("'{}'", text.replace('\'', "''"))
    } else {
        format!("\"{text}\"")
    }
}

/// Prints `dv: warning: <text>` on stderr: something a command that succeeded left as it was.
/// Best effort, like [`error`].
pub(crate) fn warning(ctx: &mut Context, text: &str) -> Result<(), CliError> {
    notice(ctx, &format!("warning: {text}"));
    Ok(())
}

/// Prints `dv: <text>` on stderr: what a command does besides its work, which a person waits
/// on (installing dv in a distro). Best effort, like [`error`].
pub(crate) fn notice(ctx: &mut Context, text: &str) {
    let mut text = message(text).join("\n");
    text.push('\n');
    let _ = ctx.stderr.write_all(text.as_bytes());
    let _ = ctx.stderr.flush();
}

/// `text` as stderr prints a message (PLAN §10.3): `dv: ` and its first line, then each
/// line after a line break indented two spaces, as an error's detail is, so nothing a
/// message holds (a path, a name) starts a line that reads as dv's own; each line escaped
/// (`escape::line`).
fn message(text: &str) -> Vec<String> {
    let mut lines = escape::breaks(text).map(escape::line);
    let first = lines.next().unwrap_or_default();
    std::iter::once(format!("dv: {first}"))
        .chain(lines.map(|line| format!("  {line}")))
        .collect()
}

/// Reports `error`: `dv: <message>` on stderr ([`message`]), then its candidates (`id  label
/// status  round n  worktree`, PLAN §10.2: the worktree tells apart two worktrees' reviews
/// whose labels read the same) and its detail, each line indented and escaped; with `json`
/// also the error document on stdout. Returns its exit code. Printing is best effort: a
/// closed stream can't report anything anyway.
pub fn error(ctx: &mut Context, json: bool, error: &CliError) -> Exit {
    let mut lines = message(&error.message);
    if let Some(candidates) = &error.candidates {
        lines.extend(candidates.iter().map(|candidate| {
            let line = format!(
                "  {}  {}  {}  round {}  {}",
                candidate.id,
                candidate.label,
                candidate.status,
                candidate.round,
                candidate.worktree
            );
            escape::line(&line).into_owned()
        }));
    }
    if let Some(detail) = &error.detail {
        lines.extend(escape::breaks(detail).map(|line| format!("  {}", escape::line(line))));
    }
    let mut text = lines.join("\n");
    text.push('\n');
    let _ = ctx.stderr.write_all(text.as_bytes());
    let _ = ctx.stderr.flush();
    if json {
        let doc = ErrorDoc {
            error: ErrorDto {
                code: error.code,
                message: error.message.clone(),
                detail: error.detail.clone(),
                candidates: error.candidates.clone(),
            },
        };
        let _ = self::json(ctx, &doc);
    }
    error.exit
}

/// Reports a command line clap couldn't parse, given the arguments it was parsing (the program
/// name first). `--help` and `--version` print on stdout and exit 0; a real error prints on
/// stderr and exits 2, and when `--json` is anywhere on the line it is also the JSON error
/// document (`usage`) on stdout, so an agent that asked for JSON always gets JSON. clap
/// quotes what was typed in its errors: each of their lines is escaped as dv's own are, and
/// when an argument itself holds a line break, every line after the first is indented, so
/// what follows the break in clap's quote of it starts no line.
pub fn parse_error(error: &clap::Error, args: &[OsString], ctx: &mut Context) -> Exit {
    let rendered = error.render().to_string();
    if error.exit_code() == 0 {
        let _ = ctx.stdout.write_all(rendered.as_bytes());
        let _ = ctx.stdout.flush();
        return Exit::Ok;
    }
    let broken = args
        .iter()
        .any(|arg| arg.to_string_lossy().contains(['\n', '\r']));
    let lines: Vec<String> = rendered
        .split('\n')
        .enumerate()
        .map(|(index, line)| {
            let line = escape::line(line);
            if broken && index > 0 && !line.is_empty() && !line.starts_with(' ') {
                format!("  {line}")
            } else {
                line.into_owned()
            }
        })
        .collect();
    let _ = ctx.stderr.write_all(lines.join("\n").as_bytes());
    let _ = ctx.stderr.flush();
    if asks_for_json(args) {
        let message = match error.kind() {
            // A command group named alone (`dv --json review`): clap shows the group's help,
            // whose first line is its description, not what is wrong.
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                missing_subcommand(&rendered)
            }
            _ => headline(&rendered),
        };
        let doc = ErrorDoc {
            error: ErrorDto {
                code: code::USAGE,
                message,
                detail: None,
                candidates: None,
            },
        };
        let _ = json(ctx, &doc);
    }
    Exit::Usage
}

/// Whether a line clap couldn't parse asked for JSON: `--json` (or a `--json=…` spelling clap
/// refuses for a flag) anywhere before a bare `--`, after which every argument is a value. A
/// `--` right after `--body` or `--note` is that option's value (they take text that starts
/// with '-', and refuse a lone `--`), not the end of the options.
fn asks_for_json(args: &[OsString]) -> bool {
    let mut previous: Option<&str> = None;
    for arg in args.iter().skip(1) {
        let text = arg.to_str();
        if text == Some("--") && !matches!(previous, Some("--body" | "--note")) {
            return false;
        }
        if text.is_some_and(|text| text == "--json" || text.starts_with("--json=")) {
            return true;
        }
        previous = text;
    }
    false
}

/// What a command group named without one of its commands lacks, in clap's own words for
/// the same mistake made otherwise (`dv review --json`): "'dv review' requires a subcommand but
/// one was not provided". The group is read from the help's usage line (`Usage: dv review
/// [OPTIONS] <COMMAND>`).
fn missing_subcommand(rendered: &str) -> String {
    let usage = rendered
        .lines()
        .find_map(|line| line.trim().strip_prefix("Usage: "))
        .unwrap_or("dv");
    let command: Vec<&str> = usage
        .split_whitespace()
        .take_while(|word| !word.starts_with(['[', '<']))
        .collect();
    format!(
        "'{}' requires a subcommand but one was not provided",
        command.join(" ")
    )
}

/// The first line of clap's message, without its `error: ` prefix. A line that ends in `:`
/// introduces the indented lines under it (the required arguments that are missing), which
/// follow it, so the message alone says what to fix.
fn headline(rendered: &str) -> String {
    let mut lines = rendered.lines().skip_while(|line| line.trim().is_empty());
    let first = lines
        .next()
        .map(str::trim)
        .unwrap_or("invalid command line");
    let first = first.strip_prefix("error: ").unwrap_or(first);
    if !first.ends_with(':') {
        return first.to_string();
    }
    let listed: Vec<&str> = lines
        .take_while(|line| line.starts_with([' ', '\t']) && !line.trim().is_empty())
        .map(str::trim)
        .collect();
    if listed.is_empty() {
        first.to_string()
    } else {
        format!("{first} {}", listed.join(", "))
    }
}
