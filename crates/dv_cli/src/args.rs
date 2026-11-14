//! The headless half of dv's one command tree (docs/PLAN.md §10.1): the global flags and the
//! subcommands agents and scripts run. The `dv` binary composes [`Command`] into its own tree
//! beside the GUI's options and `dv open`, so `dv --help` lists every command; [`Cli`] is the
//! same tree without the GUI, which tests and [`crate::main_with`] parse.
//!
//! No argument here reads the environment through clap: `$DV_REVIEW`, `$DV_AUTHOR`,
//! `$DV_MODEL` and `CLAUDECODE` come from [`crate::Context`], so a command run in-process sees exactly the
//! environment its caller gives it.

use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

/// The flags every headless command takes, wherever they appear on the line (PLAN §10.1).
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobalArgs {
    /// The repository: a path anywhere inside its working tree (also
    /// \\wsl.localhost\<distro>\<path>) [default: the current directory's]
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "wsl")]
    pub repo: Option<String>,

    /// A repository inside a WSL distro, as <distro>:<absolute path> (on Windows the command
    /// then runs inside the distro)
    #[arg(long, global = true, value_name = "DISTRO:PATH")]
    pub wsl: Option<String>,

    /// The review to act on [default: $DV_REVIEW, else the one open review of the work checked
    /// out here]
    #[arg(long, global = true, value_name = "ID")]
    pub review: Option<String>,

    /// Print one JSON document ("schema": 1) on stdout instead of text
    #[arg(long, global = true)]
    pub json: bool,

    /// Act as the agent NAME (claude-code, codex, …) [default: $DV_AUTHOR, else claude-code
    /// under Claude Code, else the reviewer, by git's user.name]
    #[arg(long, global = true, value_name = "NAME")]
    pub author: Option<String>,

    /// The model the agent runs on, as it names itself ("Claude Fable 5.2"), shown beside
    /// what it writes [default: $DV_MODEL]
    #[arg(long, global = true, value_name = "MODEL")]
    pub model: Option<String>,
}

/// dv's headless commands on their own: the tree [`crate::main_with`] parses. The `dv` binary
/// parses the full tree (with the GUI's options and `dv open`) and calls [`crate::run`].
#[derive(Parser, Debug, Clone, PartialEq, Eq)]
#[command(
    name = "dv",
    bin_name = "dv",
    version = crate::version_text(),
    about = "Review diffs locally; agents answer through this CLI"
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// The headless commands (PLAN §10.1).
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// The repository, its open reviews and the current one with its round and thread counts
    Status,

    /// List, start, show and wait for reviews; hand a round back or finish it
    #[command(subcommand)]
    Review(ReviewCommand),

    /// List and read comment threads; add, answer and address them
    #[command(subcommand)]
    Comment(CommentCommand),

    /// Set up what `dv doctor` checks: dv on PATH, the agent skill, dv in WSL distros (`dv setup`
    /// alone is `dv setup all`)
    Setup(SetupArgs),

    /// Check dv's setup on this machine and in each WSL distro: dv on PATH, the agent skills,
    /// git's identity, gh (exit 1 when something needs fixing)
    Doctor,

    /// Serve repositories to the dv app over stdin and stdout (the app starts it)
    Backend(BackendArgs),
}

/// `dv review …`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ReviewCommand {
    /// The repository's open reviews, or every review with --all
    List(ReviewListArgs),

    /// Start a review (or resume the open one) of the working tree, the index, a branch, a
    /// commit or a range
    New(ReviewNewArgs),

    /// A review: its target, status, rounds and thread counts
    Show(ReviewIdArg),

    /// Wait until the reviewer finishes a round (exit 3 on --timeout)
    Wait(ReviewWaitArgs),

    /// Agent: the round is addressed; hand the review back to the reviewer
    Done(ReviewDoneArgs),

    /// Reviewer: finish the round with a verdict
    Submit(ReviewSubmitArgs),

    /// Reviewer: close a review without approving it
    Close(RequiredReviewId),

    /// Reviewer: delete a review, its log and its round snapshots
    Delete(RequiredReviewId),
}

/// `dv review list`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ReviewListArgs {
    /// Every review, finished ones included
    #[arg(long)]
    pub all: bool,

    /// Only reviews with this status (repeat it, or separate several with commas)
    #[arg(long, value_enum, value_delimiter = ',', value_name = "STATUS")]
    pub status: Vec<StatusArg>,
}

/// A review status as `--status` names it.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[value(rename_all = "snake_case")]
pub enum StatusArg {
    Open,
    #[value(alias = "changes-requested")]
    ChangesRequested,
    #[value(alias = "in-progress")]
    InProgress,
    Ready,
    Approved,
    Closed,
    Draft,
    Submitted,
}

/// `dv review new`: which target (PLAN §2.2); the working tree when none is named.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[command(group(ArgGroup::new("target").args(["staged", "branch", "commit", "range", "pr"])))]
pub struct ReviewNewArgs {
    /// The index (staged changes) against HEAD
    #[arg(long)]
    pub staged: bool,

    /// A branch's work since it left its base [default branch: the one checked out]
    #[arg(long, value_name = "BRANCH", num_args = 0..=1)]
    pub branch: Option<Option<String>>,

    /// The branch --branch compares with [default: origin/HEAD, else main, else master]
    #[arg(long, value_name = "BASE", requires = "branch")]
    pub base: Option<String>,

    /// One commit against its first parent
    #[arg(long, value_name = "REV")]
    pub commit: Option<String>,

    /// A range, A..B or A...B
    #[arg(long, value_name = "A..B", value_parser = parse_range)]
    pub range: Option<RangeArg>,

    /// A GitHub pull request (a later version)
    #[arg(long, value_name = "N")]
    pub pr: Option<String>,
}

/// A `--range` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeArg {
    pub from: String,
    pub to: String,
    pub three_dot: bool,
}

fn parse_range(text: &str) -> Result<RangeArg, String> {
    let (from, to, three_dot) = match text.split_once("...") {
        Some((from, to)) => (from, to, true),
        None => match text.split_once("..") {
            Some((from, to)) => (from, to, false),
            None => return Err(format!("expected A..B or A...B, got {text:?}")),
        },
    };
    if from.is_empty() || to.is_empty() {
        return Err(format!("both ends of the range are needed, got {text:?}"));
    }
    Ok(RangeArg {
        from: from.to_string(),
        to: to.to_string(),
        three_dot,
    })
}

/// A review id that may be left out (PLAN §10.2 then picks the review).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ReviewIdArg {
    /// The review [default: --review, $DV_REVIEW, else the one open review of the work checked
    /// out here]
    #[arg(value_name = "ID")]
    pub id: Option<String>,
}

/// A review id that must be given.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct RequiredReviewId {
    /// The review
    #[arg(value_name = "ID")]
    pub id: String,
}

/// `dv review wait`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ReviewWaitArgs {
    #[command(flatten)]
    pub review: ReviewIdArg,

    /// Give up after this many seconds (fractions allowed) and exit 3
    #[arg(
        long,
        value_name = "SECONDS",
        value_parser = parse_timeout,
        allow_negative_numbers = true
    )]
    pub timeout: Option<Duration>,
}

fn parse_timeout(text: &str) -> Result<Duration, String> {
    let seconds: f64 = text
        .trim()
        .parse()
        .map_err(|_| format!("expected a number of seconds, got {text:?}"))?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(format!(
            "the timeout must be a positive number of seconds, got {text:?}"
        ));
    }
    Duration::try_from_secs_f64(seconds).map_err(|_| format!("{text:?} seconds is too long"))
}

/// `dv review done`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ReviewDoneArgs {
    #[command(flatten)]
    pub review: ReviewIdArg,

    #[command(flatten)]
    pub note: Note,

    /// The round you worked on (`round.n` from `dv review wait`), which must be the last one
    /// the reviewer finished; refused with unseen_round when they finished another meanwhile
    #[arg(long, value_name = "N")]
    pub round: Option<u32>,
}

/// A note on what was done (`dv review done`, `dv comment address`): `--note <t>` or
/// `--note-file <f|->`, read as a body is.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
#[group(multiple = false)]
pub struct Note {
    /// What was done, for the reviewer (Markdown; text that starts with '-', a list, is fine)
    #[arg(
        long,
        value_name = "TEXT",
        allow_hyphen_values = true,
        value_parser = parse_text
    )]
    pub note: Option<String>,

    /// Read the note from a file, or from stdin with - (safe for any quoting)
    #[arg(long, value_name = "FILE|-")]
    pub note_file: Option<PathBuf>,
}

/// A `--body` or `--note` value: any text, one that starts with '-' included (a Markdown list,
/// "-1, I disagree": clap is told to take it as the value, `allow_hyphen_values`), except a
/// lone flag (`--json`, `-v`, `--author=x`: one word, a letter after the dashes), which means
/// the text was left out (`--note --json`) rather than written, and a lone `-` or `--`: stdin
/// meant through the inline option (`--note -`), or the end of the options (`--body --
/// --json`), never text anyone writes.
fn parse_text(text: &str) -> Result<String, String> {
    if text == "-" || text == "--" {
        return Err(format!(
            "{text:?} alone isn't text: to read it from stdin, use --body-file - (a note: \
             --note-file -)"
        ));
    }
    let name = text.strip_prefix("--").or_else(|| text.strip_prefix('-'));
    let flag = name.is_some_and(|name| {
        name.starts_with(|c: char| c.is_ascii_alphabetic()) && !name.contains(char::is_whitespace)
    });
    if flag {
        return Err(
            "that is a flag, not text: was the text left out? (text that is one word like a \
             flag goes through the option's -file form, from a file or stdin)"
                .to_string(),
        );
    }
    Ok(text.to_string())
}

/// `dv review submit`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ReviewSubmitArgs {
    #[command(flatten)]
    pub review: ReviewIdArg,

    /// The round's verdict
    #[arg(long, value_enum)]
    pub verdict: VerdictArg,

    #[command(flatten)]
    pub body: OptionalBody,
}

/// A verdict as `--verdict` names it.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictArg {
    Approve,
    #[value(alias = "request_changes")]
    RequestChanges,
    Comment,
}

/// A summary that may be left out: `--body <t>` or `--body-file <f|->`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[group(multiple = false)]
pub struct OptionalBody {
    /// The text (Markdown; text that starts with '-', a list, is fine)
    #[arg(
        long,
        value_name = "TEXT",
        allow_hyphen_values = true,
        value_parser = parse_text
    )]
    pub body: Option<String>,

    /// Read the text from a file, or from stdin with -
    #[arg(long, value_name = "FILE|-")]
    pub body_file: Option<PathBuf>,
}

/// A body that must be given: `--body <t>` or `--body-file <f|->`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[group(required = true, multiple = false)]
pub struct Body {
    /// The text (Markdown; text that starts with '-', a list, is fine)
    #[arg(
        long,
        value_name = "TEXT",
        allow_hyphen_values = true,
        value_parser = parse_text
    )]
    pub body: Option<String>,

    /// Read the text from a file, or from stdin with - (safe for any quoting)
    #[arg(long, value_name = "FILE|-")]
    pub body_file: Option<PathBuf>,
}

/// `dv comment …`.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum CommentCommand {
    /// The review's threads, each with the diff around its code now
    List(CommentListArgs),

    /// One thread, with the diff around its code now, the comment and the replies
    Show(ThreadIdArg),

    /// Comment on lines of a file in the review, or on the whole file
    Add(CommentAddArgs),

    /// Agent: explain your change on lines of a file, or on the whole file (an annotation)
    Annotate(CommentAddArgs),

    /// Reply to a thread
    Reply(CommentReplyArgs),

    /// Mark a thread addressed, with an optional note on what was done
    Address(CommentAddressArgs),

    /// Reviewer: resolve a thread
    Resolve(ThreadIdArg),

    /// Reviewer: reopen a thread
    Reopen(ThreadIdArg),

    /// Replace the text of a thread or reply you wrote
    Edit(CommentEditArgs),

    /// Delete a thread or reply you wrote
    Delete(ItemIdArg),
}

/// `dv comment edit`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct CommentEditArgs {
    #[command(flatten)]
    pub item: ItemIdArg,

    #[command(flatten)]
    pub body: Body,
}

/// A thread's or a reply's id.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ItemIdArg {
    /// The thread (t-…) or reply (c-…)
    #[arg(value_name = "ID")]
    pub id: String,
}

/// `dv comment list`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct CommentListArgs {
    /// Only threads in this state (repeat it, or separate several with commas)
    #[arg(long, value_enum, value_delimiter = ',', value_name = "STATE")]
    pub status: Vec<ThreadStatusArg>,

    /// Only threads on this file (its path from the repository root)
    #[arg(long, value_name = "PATH")]
    pub file: Option<String>,

    /// One line per thread: its id, state, place and the first line of its comment
    #[arg(long)]
    pub oneline: bool,
}

/// A thread state as `dv comment list --status` names it; `outdated` is where a thread stands
/// rather than its state (PLAN §8).
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThreadStatusArg {
    Open,
    Addressed,
    Resolved,
    /// An agent's annotation nobody has asked about (PLAN §19).
    Annotation,
    Outdated,
}

/// A thread id.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ThreadIdArg {
    /// The thread (t-…)
    #[arg(value_name = "ID")]
    pub id: String,
}

/// `dv comment add`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct CommentAddArgs {
    /// The file (its path from the repository root)
    #[arg(long, value_name = "PATH")]
    pub file: String,

    /// The line, or the first and last lines, on that side: N or N:M; without it, the comment
    /// is on the whole file
    #[arg(long, value_name = "N[:M]", value_parser = parse_lines)]
    pub lines: Option<LineRange>,

    /// Which side's lines (for a comment on the whole file, which side of it: `old` for a
    /// deleted file)
    #[arg(long, value_enum, default_value_t = SideArg::New)]
    pub side: SideArg,

    #[command(flatten)]
    pub body: Body,
}

/// A `--lines` argument: 1-based, inclusive, `start <= end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

fn parse_lines(text: &str) -> Result<LineRange, String> {
    let number = |part: &str| {
        part.trim()
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("expected N or N:M with lines from 1, got {text:?}"))
    };
    let (start, end) = match text.split_once([':', '-']) {
        Some((start, end)) => (number(start)?, number(end)?),
        None => {
            let line = number(text)?;
            (line, line)
        }
    };
    Ok(LineRange {
        start: start.min(end),
        end: start.max(end),
    })
}

/// A diff side as `--side` names it.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideArg {
    Old,
    New,
}

/// `dv comment reply`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct CommentReplyArgs {
    #[command(flatten)]
    pub thread: ThreadIdArg,

    #[command(flatten)]
    pub body: Body,
}

/// `dv comment address`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct CommentAddressArgs {
    #[command(flatten)]
    pub thread: ThreadIdArg,

    #[command(flatten)]
    pub note: Note,
}

/// `dv setup …`: alone, `dv setup all`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct SetupArgs {
    #[command(subcommand)]
    pub what: Option<SetupCommand>,

    #[command(flatten)]
    pub yes: YesArg,
}

/// What `dv setup` sets up (PLAN §12).
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum SetupCommand {
    /// Put dv on PATH: this dv's folder in your user Path on Windows; ~/.local/bin/dv on Linux
    /// and in WSL, with ~/.local/bin on your shell's PATH
    Path(SetupPathArgs),

    /// Install the dv-review skill for each agent found (Claude Code, Codex, Cursor)
    Agents(SetupAgentsArgs),

    /// Install this dv in a WSL distro and put it on the distro's PATH (Windows)
    Wsl(SetupWslArgs),

    /// Everything `dv doctor` lists that dv can fix, here and in each running WSL distro
    All(YesArg),
}

/// `dv setup path`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct SetupPathArgs {
    #[command(flatten)]
    pub yes: YesArg,

    /// Take dv off PATH again: exactly what `dv setup path` put there
    #[arg(long)]
    pub remove: bool,

    /// The folder to put on PATH on Windows [default: this dv's] (tests, the installer)
    #[arg(long, hide = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,
}

/// `--yes`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct YesArg {
    /// Go ahead without asking
    #[arg(long)]
    pub yes: bool,
}

/// `dv setup agents`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct SetupAgentsArgs {
    #[command(flatten)]
    pub yes: YesArg,

    /// The home directory the agents' folders are found in (tests)
    #[arg(long, hide = true, value_name = "DIR")]
    pub home: Option<PathBuf>,
}

/// `dv setup wsl`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct SetupWslArgs {
    /// The distro
    #[arg(value_name = "DISTRO")]
    pub distro: String,

    #[command(flatten)]
    pub yes: YesArg,
}

/// `dv backend --stdio` (PLAN §5.1).
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct BackendArgs {
    /// Speak the backend protocol on stdin and stdout (required)
    #[arg(long)]
    pub stdio: bool,
}

#[cfg(test)]
mod tests;
