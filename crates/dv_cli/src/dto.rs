//! The JSON agents parse (docs/PLAN.md §10.3): one document per command, `"schema": 1` first,
//! snake_case throughout. These are documented output shapes, never the internal event or
//! state structs, so the store can change without breaking an agent.
//!
//! Timestamps are milliseconds since the Unix epoch. Every key is always present (`null` when
//! it has no value), except an error's `detail` and `candidates`.
//!
//! | Command | Document |
//! |---|---|
//! | `status` | [`StatusDoc`]: `dv` (this build: `{version, channel}`, `channel` `release` for a packaged build, else `development`), `repo`, `author` (who this command acts as), `current` (the review the commands mean, or `null`), `reviews` (the open ones), `unreadable` |
//! | `review list` | [`ReviewListDoc`]: `reviews`, `unreadable` (logs that don't load: `{id, error}`) |
//! | `review show` | [`ReviewShowDoc`]: `review` |
//! | `review new` | [`ReviewNewDoc`]: `created` (`false`: the target's open review, resumed), `review` |
//! | `review wait` | [`WaitDoc`]: `outcome` (`changes_requested`, `approved`, `closed`), `review`, `round` (the last closed one, or `null`), `threads`; on `--timeout` (exit 3) [`WaitTimeoutDoc`]: `outcome: "timeout"` alone |
//! | `review done`, `review close` | [`ReviewChangeDoc`]: `changed` (`false`: nothing written), `review` |
//! | `review submit` | [`ReviewSubmitDoc`]: `review`, `round` (the round it closed) |
//! | `review delete` | [`ReviewDeleteDoc`]: `deleted` (the id) |
//! | `comment list` | [`CommentListDoc`]: `review_id`, `threads` |
//! | `comment show`, `comment add`, `comment annotate`, `comment edit` | [`CommentShowDoc`]: `thread` (an edited reply: [`ReplyDoc`]) |
//! | `comment reply` | [`ReplyDoc`]: `reply`, `thread` |
//! | `comment delete` | [`CommentDeleteDoc`]: `deleted` (the id), `thread` (`null` once the thread itself is deleted) |
//! | `comment address`, `resolve`, `reopen` | [`ThreadChangeDoc`]: `changed`, `thread` |
//! | `doctor` | [`DoctorDoc`]: `ok`, `environments: [{name, kind, running, note, checks: [{id, title, state, detail, fix}]}]` ([`Environment`], [`Check`]: `state` `ok`, `missing`, `out_of_date`, `failed` or `skipped`; `fix` `{action, command}` or `null`) |
//! | `setup agents` | [`SetupAgentsDoc`]: `agents: [{agent, path, action, reason}]` |
//! | `setup path` | [`SetupPathDoc`]: `dir`, `action`, `link`, `rc_file` |
//! | `setup all`, `setup` | [`SetupAllDoc`]: `steps: [{environment, check, outcome, detail, command}]` |
//! | `setup wsl` | [`SetupWslDoc`]: `distro`, `arch`, `build`, `installed`, `linked`, `path` |
//! | any, failing | [`ErrorDoc`]: `error: {code, message, detail?, candidates?}` |
//!
//! A review ([`ReviewDto`]): `id`, `target` (`kind` and the spec's fields, plus `label`, e.g.
//! "working tree vs main", a `working` or `staged` review named by the `HEAD` of the worktree
//! it was made in), `status`, `round` (the round in progress; a finished review's last),
//! `rounds` (the closed ones: `{n, verdict, body, closed_at}`), `threads` (`{open, addressed,
//! resolved, annotation}`, `annotation` the agent's annotations nobody asked about, PLAN §19), `worktree`, `created_at`, `updated_at`, `status_note` (the note the last status
//! change carried, e.g. `dv review done --note`, or `null`).
//!
//! A thread ([`ThreadDto`]): `id`, `review_id`, `state` (`open`, `addressed`, `resolved`, or
//! `annotation`: an agent's explanation of its change, waiting on no one, PLAN §19),
//! `annotation` (`true` for a thread an agent opened as an annotation, which it stays once the
//! reviewer's reply opened it),
//! `placement` (`unchanged`, `moved`, `edited`, `outdated` or `file`: where its code stands in
//! the review's current diff, PLAN §8; `moved` also when it stands where an earlier write
//! recorded it had moved to: `dv_core::review::shows_moved`; `edited` when it stands where it
//! was written but its text changed there, `dv_core::review::shows_edited`: read
//! `current_code`; `file` for a comment on the whole file whose file is in the diff),
//! `outdated`, `in_diff` (`false` for a thread whose file isn't among the review's changes: it
//! never changed, or stopped changing; it is placed against that file as the review's new side
//! holds it, `anchor.current` and `excerpt` from that text, or `outdated` once its code is gone
//! from it or the file is), `file_level` (`true` for a comment on the whole file, not on lines: its
//! `anchor.original` lines are 0, `anchor.current`, `code_context`, `current_code`, `excerpt`
//! and `trail` are `null` and `code` is empty; `path` names its file now, a rename followed,
//! and it is `outdated` once that file is gone or deleted), `path` (its file in the current
//! diff, else where it was
//! written), `side` (`old` or `new`), `anchor: {original: {round, path, side, start_line,
//! end_line}, current: {path, side, start_line, end_line} | null}`, `code` (the lines it was
//! written on, as they were), `code_context` (`{before, after}`: up to three lines that stood
//! around `code` then; only when `anchor.current` is `null`), `current_code` (the lines at
//! `anchor.current` now, or `null`), `excerpt` (the review's diff around `anchor.current` now,
//! the rows `dv comment show` prints, PLAN §10.3; `null` exactly when `anchor.current` is),
//! `trail` (for an `outdated` thread, where its code went since the round it was written in,
//! when that round's snapshot tells, PLAN §8: `{round, went, path, start_line, end_line,
//! before_line, code, excerpt}`, `went` `replaced` — `start_line..end_line` of `path` replaced
//! it, `code` those lines now and `excerpt` the diff around them, as `excerpt` is for a found
//! thread — or `removed` — nothing replaced it; it stood just before line `before_line` of
//! `path` now, one past the last line at its end, `code` and `excerpt` `null`; else `null`),
//! `body` (Markdown), `author: {kind, name, model}` (`model`: the one an agent said it runs
//! on, else `null`), `created_at`, `note` (the last state change's note),
//! `replies: [{id, author, body, created_at}]`. Act on `anchor.current`, never on the
//! original line numbers.
//!
//! Within a schema a document only grows: readers ignore keys they don't know; a key removed,
//! renamed or retyped, or a value whose meaning changes, is a new schema number (PLAN §10.3).

use dv_core::label::Labels;
use dv_core::review::{
    Anchor, AnchorSide, Author, Comment, Placement, Review, Round, Thread, Trail, Went,
    shows_edited, shows_moved,
};
use dv_core::target::Target;
use serde::Serialize;

pub use crate::checks::{Check, Environment, Fix, Kind, State};

/// The version of every document's shape; `"schema"` in each.
pub const SCHEMA: u32 = 1;

/// A document as printed: `"schema"` first, then the command's own keys.
#[derive(Serialize)]
pub struct Document<'a, T: Serialize> {
    pub schema: u32,
    #[serde(flatten)]
    pub body: &'a T,
}

impl<'a, T: Serialize> Document<'a, T> {
    pub fn new(body: &'a T) -> Self {
        Self {
            schema: SCHEMA,
            body,
        }
    }
}

/// Who wrote something, or who a command acts as.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct AuthorDto {
    /// `reviewer`, `agent` or `github`.
    pub kind: &'static str,
    pub name: String,
    /// The model an agent said it runs on ("Claude Fable 5.2"), or `null`.
    pub model: Option<String>,
}

impl From<&Author> for AuthorDto {
    fn from(author: &Author) -> Self {
        Self {
            kind: author.kind.as_str(),
            name: author.name.clone(),
            model: author.model.clone(),
        }
    }
}

/// A target's spec, tagged by `kind`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetSpecDto {
    Working,
    Staged,
    Branch {
        branch: String,
        base: Option<String>,
    },
    Commit {
        rev: String,
    },
    Range {
        from: String,
        to: String,
        three_dot: bool,
    },
    Pr {
        owner: String,
        repo: String,
        number: u64,
        base_oid: String,
        head_oid: String,
    },
}

impl From<&Target> for TargetSpecDto {
    fn from(target: &Target) -> Self {
        match target {
            Target::Working => Self::Working,
            Target::Staged => Self::Staged,
            Target::Branch { branch, base } => Self::Branch {
                branch: branch.clone(),
                base: base.clone(),
            },
            Target::Commit { rev } => Self::Commit { rev: rev.clone() },
            Target::Range {
                from,
                to,
                three_dot,
            } => Self::Range {
                from: from.clone(),
                to: to.clone(),
                three_dot: *three_dot,
            },
            Target::Pr {
                owner,
                repo,
                number,
                base_oid,
                head_oid,
            } => Self::Pr {
                owner: owner.clone(),
                repo: repo.clone(),
                number: *number,
                base_oid: base_oid.to_string(),
                head_oid: head_oid.to_string(),
            },
        }
    }
}

/// A target: its spec and how dv names it.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct TargetDto {
    #[serde(flatten)]
    pub spec: TargetSpecDto,
    /// "working tree vs main", as the GUI's title bar says (DESIGN §4.1).
    pub label: String,
}

impl TargetDto {
    /// `target` of a review made in `worktree` ([`Labels::label`]).
    pub fn new(target: &Target, worktree: &str, labels: &Labels) -> Self {
        Self {
            spec: target.into(),
            label: labels.label(target, worktree),
        }
    }
}

/// A closed round.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct RoundDto {
    pub n: u32,
    /// `approve`, `request_changes`, `comment`, or `null`.
    pub verdict: Option<&'static str>,
    /// The reviewer's summary, Markdown.
    pub body: Option<String>,
    pub closed_at: u64,
}

impl From<&Round> for RoundDto {
    fn from(round: &Round) -> Self {
        Self {
            n: round.n,
            verdict: round.verdict.map(|verdict| verdict.as_str()),
            body: round.body.clone(),
            closed_at: round.closed_at,
        }
    }
}

/// Live threads by state.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThreadCountsDto {
    pub open: u32,
    pub addressed: u32,
    pub resolved: u32,
    /// Annotations nobody has asked about (PLAN §19).
    pub annotation: u32,
}

impl ThreadCountsDto {
    /// `review`'s live threads by state.
    pub fn of(review: &Review) -> Self {
        let counts = review.summary().threads;
        Self {
            open: counts.open,
            addressed: counts.addressed,
            resolved: counts.resolved,
            annotation: counts.annotation,
        }
    }
}

/// The note the last status change carried.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct StatusNoteDto {
    /// The status it set.
    pub status: &'static str,
    /// Markdown.
    pub note: String,
    pub author: Option<AuthorDto>,
    pub at: u64,
}

/// A review (module docs).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ReviewDto {
    pub id: String,
    pub target: TargetDto,
    pub status: &'static str,
    /// The round in progress; a finished review's last.
    pub round: u32,
    pub rounds: Vec<RoundDto>,
    pub threads: ThreadCountsDto,
    /// The worktree root the review was made in.
    pub worktree: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub status_note: Option<StatusNoteDto>,
}

impl ReviewDto {
    pub fn new(review: &Review, labels: &Labels) -> Self {
        Self {
            id: review.id.to_string(),
            target: TargetDto::new(&review.target, &review.worktree, labels),
            status: review.status.as_str(),
            round: review.shown_round(),
            rounds: review.rounds.iter().map(RoundDto::from).collect(),
            threads: ThreadCountsDto::of(review),
            worktree: review.worktree.clone(),
            created_at: review.created_at,
            updated_at: review.updated_at,
            status_note: review.status_change.as_ref().and_then(|change| {
                Some(StatusNoteDto {
                    status: change.status.as_str(),
                    note: change.note.clone()?,
                    author: change.author.as_ref().map(AuthorDto::from),
                    at: change.at,
                })
            }),
        }
    }
}

/// A review a command could have meant (PLAN §10.2): its id, its target's label (a
/// `working` or `staged` review's named by its own worktree's `HEAD`), status, round and the
/// worktree it was made in, which tells two worktrees' reviews apart.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct CandidateDto {
    pub id: String,
    pub label: String,
    pub status: &'static str,
    pub round: u32,
    pub worktree: String,
}

/// A review whose log doesn't load.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct UnreadableDto {
    pub id: String,
    pub error: String,
}

/// Where a thread was written.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct OriginalAnchorDto {
    pub round: u32,
    pub path: String,
    pub side: &'static str,
    pub start_line: u32,
    pub end_line: u32,
}

impl From<&Anchor> for OriginalAnchorDto {
    fn from(anchor: &Anchor) -> Self {
        Self {
            round: anchor.round,
            path: anchor.path.clone(),
            side: side_str(anchor.side),
            start_line: anchor.start_line,
            end_line: anchor.end_line,
        }
    }
}

/// Where a thread's code stands in the review's current diff.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct CurrentAnchorDto {
    pub path: String,
    pub side: &'static str,
    pub start_line: u32,
    pub end_line: u32,
}

/// A thread's anchors.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct AnchorDto {
    pub original: OriginalAnchorDto,
    /// `null` when the code can't be found any more (outdated).
    pub current: Option<CurrentAnchorDto>,
}

/// A reply.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ReplyDto {
    pub id: String,
    pub author: AuthorDto,
    pub body: String,
    pub created_at: u64,
}

impl From<&Comment> for ReplyDto {
    fn from(reply: &Comment) -> Self {
        Self {
            id: reply.id.to_string(),
            author: (&reply.author).into(),
            body: reply.body.clone(),
            created_at: reply.created_at,
        }
    }
}

/// Where a thread shows now, as [`crate::placement`] found it.
#[derive(Debug, Clone, PartialEq)]
pub struct Located {
    pub placement: Placement,
    /// The listed file's path, when the thread's file is in the current diff.
    pub file: Option<String>,
    /// The path the listed file has on the thread's side (`AnchorPlace::for_entry`: a renamed
    /// file's old one on the old side), as the GUI names a comment on the whole file.
    pub side_path: Option<String>,
    /// Its code now, when it was found.
    pub current: Option<CurrentCode>,
    /// Where its code went since its round, when it is outdated and its round's snapshot
    /// tells.
    pub trail: Option<TrailCode>,
}

/// Where an outdated thread's code went ([`Trail`]), in the listed file's new side now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrailCode {
    pub trail: Trail,
    /// The file's path on the new side.
    pub path: String,
    /// The lines that replaced it, with the excerpt around them; `None` when it was removed.
    pub current: Option<CurrentCode>,
    /// It was removed at the file's end.
    pub at_end: bool,
}

/// Where an outdated thread's code went (module docs: a thread's `trail`).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct TrailDto {
    /// The round whose snapshot its code was found in: it changed since.
    pub round: u32,
    /// `replaced` or `removed`.
    pub went: &'static str,
    pub path: String,
    /// The lines that replaced it, when it was replaced.
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    /// The line it stood just before now, when it was removed.
    pub before_line: Option<u32>,
    /// The lines that replaced it, as they are now.
    pub code: Option<String>,
    /// The review's diff around the lines that replaced it, as `excerpt` is for a found thread.
    pub excerpt: Option<String>,
    /// It was removed at the file's end (`before_line` is past its last line now): the text
    /// output says so rather than name a line that isn't there.
    #[serde(skip)]
    pub at_end: bool,
}

impl From<&TrailCode> for TrailDto {
    fn from(code: &TrailCode) -> Self {
        let (start_line, end_line, before_line) = match code.trail.went {
            Went::Replaced {
                start_line,
                end_line,
            } => (Some(start_line), Some(end_line), None),
            Went::Removed { before_line } => (None, None, Some(before_line)),
        };
        Self {
            round: code.trail.round,
            went: code.trail.went.as_str(),
            path: code.path.clone(),
            start_line,
            end_line,
            before_line,
            code: code.current.as_ref().map(|current| current.code.clone()),
            excerpt: code.current.as_ref().map(|current| current.excerpt.clone()),
            at_end: code.at_end,
        }
    }
}

/// A thread's code as it stands now: its lines, their text and the excerpt of the diff around
/// them, taken together from one diff so the three never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentCode {
    pub anchor: CurrentAnchorDto,
    pub code: String,
    /// The excerpt's rows as printed, one per line (`dv_core::review::excerpt_lines`).
    pub excerpt: String,
}

/// The lines that stood around a thread's code when it was written: its first anchor's
/// context, up to `dv_core::review::ANCHOR_CONTEXT` lines either side.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct CodeContextDto {
    pub before: Vec<String>,
    pub after: Vec<String>,
}

/// A thread (module docs).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ThreadDto {
    pub id: String,
    pub review_id: String,
    pub state: &'static str,
    /// An agent opened it as an annotation (PLAN §19), asked about since or not.
    pub annotation: bool,
    pub placement: &'static str,
    pub outdated: bool,
    /// Its file is among the review's changes. `false` for a thread on a file the review
    /// doesn't change (it never changed, or stopped changing): placed in that file's text at
    /// the review's new side, or outdated when its code is gone from it (module docs).
    pub in_diff: bool,
    /// A comment on the whole file, not on lines (module docs).
    pub file_level: bool,
    pub path: String,
    pub side: &'static str,
    pub anchor: AnchorDto,
    /// The lines the thread was written on, as they were then.
    pub code: String,
    /// The lines around `code` when it was written; only when `anchor.current` is `null`, since
    /// `excerpt` shows what surrounds the code otherwise.
    pub code_context: Option<CodeContextDto>,
    /// The lines at `anchor.current` now.
    pub current_code: Option<String>,
    /// The review's diff around `anchor.current` now, as the text output prints it (PLAN
    /// §10.3); `null` exactly when `anchor.current` is.
    pub excerpt: Option<String>,
    /// Where an outdated thread's code went since its round (module docs).
    pub trail: Option<TrailDto>,
    pub body: String,
    pub author: AuthorDto,
    pub created_at: u64,
    /// The last state change's note (an agent's "what I did").
    pub note: Option<String>,
    pub replies: Vec<ReplyDto>,
    /// Its file is among the review's changes now (`path` is that file): the text output
    /// tells a comment on a whole file that left them from one whose file lacks its side.
    #[serde(skip)]
    pub listed: bool,
    /// The path its file has on its side now, else where it was written: the text names a
    /// comment on the whole file by it, as the GUI does (`format::location`).
    #[serde(skip)]
    pub side_path: String,
}

impl ThreadDto {
    pub fn new(review: &Review, thread: &Thread, located: &Located) -> Self {
        let original = thread.first_anchor();
        let current = located.current.as_ref();
        let file_level = thread.anchor.is_file_level();
        let code_context = (current.is_none() && !file_level).then(|| CodeContextDto {
            before: original.context_before.clone(),
            after: original.context_after.clone(),
        });
        let outdated = located.placement.is_outdated();
        Self {
            id: thread.id.to_string(),
            review_id: review.id.to_string(),
            state: thread.state.as_str(),
            annotation: thread.annotation,
            placement: match located.placement {
                _ if shows_moved(thread, &located.placement) => "moved",
                _ if shows_edited(thread, &located.placement) => "edited",
                Placement::Unchanged { .. }
                | Placement::Moved { .. }
                | Placement::Unlisted { .. } => "unchanged",
                Placement::Pending { .. } | Placement::Outdated { .. } => "outdated",
                Placement::File { .. } | Placement::UnlistedFile { .. } => "file",
            },
            outdated,
            in_diff: located.placement.in_diff(),
            file_level,
            path: located
                .file
                .clone()
                .unwrap_or_else(|| thread.anchor.path.clone()),
            side: side_str(thread.anchor.side),
            anchor: AnchorDto {
                original: original.into(),
                current: current.map(|current| current.anchor.clone()),
            },
            code: original.text.join("\n"),
            code_context,
            current_code: current.map(|current| current.code.clone()),
            excerpt: current.map(|current| current.excerpt.clone()),
            trail: located.trail.as_ref().map(TrailDto::from),
            body: thread.body.clone(),
            author: (&thread.author).into(),
            created_at: thread.created_at,
            note: thread
                .last_change
                .as_ref()
                .and_then(|change| change.note.clone()),
            replies: thread
                .replies
                .iter()
                .filter(|reply| !reply.is_deleted())
                .map(ReplyDto::from)
                .collect(),
            listed: located.file.is_some(),
            side_path: located
                .side_path
                .clone()
                .unwrap_or_else(|| thread.anchor.path.clone()),
        }
    }
}

/// `old` or `new`.
pub fn side_str(side: AnchorSide) -> &'static str {
    match side {
        AnchorSide::Old => "old",
        AnchorSide::New => "new",
    }
}

/// The repository a command ran in.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct RepoDto {
    /// The worktree root.
    pub root: String,
    /// The branch checked out, `null` when `HEAD` is detached.
    pub branch: Option<String>,
    /// The branch checked out, else `HEAD`'s short commit id; `null` before the first commit
    /// of a detached `HEAD`.
    pub head: Option<String>,
}

/// This build of dv, as `dv status` names it (`dv_backend::build`).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct BuildDto {
    /// `dv --version`'s, without the development note.
    pub version: String,
    /// `release` for a build `script/package` made, else `development`.
    pub channel: String,
}

impl BuildDto {
    /// This executable's.
    pub fn this() -> Self {
        Self {
            version: dv_backend::build::VERSION.to_string(),
            channel: dv_backend::build::channel_name().to_string(),
        }
    }
}

/// `dv status`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct StatusDoc {
    /// This build of dv.
    pub dv: BuildDto,
    pub repo: RepoDto,
    /// Who this command's writes would be by: an agent (`--author`, `$DV_AUTHOR`, Claude
    /// Code) or the reviewer.
    pub author: AuthorDto,
    /// The review commands mean when they name none (PLAN §10.2), or `null`.
    pub current: Option<ReviewDto>,
    /// The repository's open reviews, newest first.
    pub reviews: Vec<ReviewDto>,
    pub unreadable: Vec<UnreadableDto>,
}

/// `dv review list`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ReviewListDoc {
    /// Newest first.
    pub reviews: Vec<ReviewDto>,
    pub unreadable: Vec<UnreadableDto>,
}

/// `dv review show`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ReviewShowDoc {
    pub review: ReviewDto,
}

/// `dv comment list`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct CommentListDoc {
    pub review_id: String,
    /// In the order they were opened.
    pub threads: Vec<ThreadDto>,
}

/// `dv comment show`, and `dv comment add` (the new thread).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct CommentShowDoc {
    pub thread: ThreadDto,
}

/// `dv review new`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ReviewNewDoc {
    /// `false` when the target's open review already existed and was resumed (PLAN §2.4).
    pub created: bool,
    pub review: ReviewDto,
}

/// `dv review done` and `dv review close`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ReviewChangeDoc {
    /// `false` when the review already had that status (and note): nothing was written.
    pub changed: bool,
    pub review: ReviewDto,
}

/// `dv review submit`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ReviewSubmitDoc {
    pub review: ReviewDto,
    /// The round that closed.
    pub round: RoundDto,
}

/// `dv review delete`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ReviewDeleteDoc {
    /// The id of the review deleted.
    pub deleted: String,
}

/// `dv review wait`, woken: why it returned and where the review stands.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct WaitDoc {
    /// The review's status: `changes_requested`, `approved` or `closed` (`submitted` for a PR
    /// review).
    pub outcome: &'static str,
    pub review: ReviewDto,
    /// The last closed round (its verdict and the reviewer's summary), `null` before the first.
    pub round: Option<RoundDto>,
    pub threads: ThreadCountsDto,
}

/// `dv review wait` that ran out of time (exit 3).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct WaitTimeoutDoc {
    /// `timeout`.
    pub outcome: &'static str,
}

/// `dv comment reply`, and `dv comment edit` of a reply.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ReplyDoc {
    pub reply: ReplyDto,
    /// The thread with the reply.
    pub thread: ThreadDto,
}

/// `dv comment delete`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct CommentDeleteDoc {
    /// The id of the thread or reply deleted.
    pub deleted: String,
    /// The thread a deleted reply was in, as it is now; `null` once the thread itself is
    /// deleted.
    pub thread: Option<ThreadDto>,
}

/// `dv comment address`, `resolve` and `reopen`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ThreadChangeDoc {
    /// `false` when the thread was already resolved or open and nothing was written
    /// (addressing again always records the new note, PLAN §2.5).
    pub changed: bool,
    pub thread: ThreadDto,
}

/// `dv setup agents`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SetupAgentsDoc {
    pub agents: Vec<AgentSetupDto>,
}

/// One agent's skill install.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentSetupDto {
    /// `claude-code`, `codex` or `cursor`.
    pub agent: &'static str,
    /// Where the skill's `SKILL.md` is (or would be).
    pub path: String,
    /// `installed`, `updated` (it differed: repaired), `unchanged` or `skipped` (the agent
    /// isn't set up here).
    pub action: &'static str,
    /// Why it was skipped, else `null`.
    pub reason: Option<String>,
}

/// `dv setup wsl`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SetupWslDoc {
    /// The distro, as `wsl.exe --list` names it.
    pub distro: String,
    /// Its architecture: `x86_64` or `aarch64`.
    pub arch: &'static str,
    /// The build installed: the sha256 of this app's Linux build for `arch`.
    pub build: String,
    /// Whether this run installed it (`false`: it was installed and intact already).
    pub installed: bool,
    /// Whether `~/.local/bin/dv` runs it now (`false`: a link the install couldn't make, which
    /// a warning says how to fix; the app and the CLI run the build itself either way).
    pub linked: bool,
    /// Where it is inside the distro: `~/.local/share/dv/bin/dv-<build>`, which
    /// `~/.local/bin/dv` links to.
    pub path: String,
}

/// `dv doctor` (PLAN §12): every environment's checks (`crate::checks`).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct DoctorDoc {
    /// No check needs fixing (the exit code is 0).
    pub ok: bool,
    /// This machine's first, then (on Windows) each WSL distro's: `{name, kind, running, note,
    /// checks: [{id, title, state, detail, fix: {action, command} | null}]}`.
    pub environments: Vec<Environment>,
}

/// `dv setup path`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SetupPathDoc {
    /// The folder on `PATH`: this dv's (Windows), `~/.local/bin` (Linux, WSL).
    pub dir: String,
    /// `added`, `unchanged`, `removed` (`--remove`) or `absent` (`--remove`, nothing to take
    /// back).
    pub action: &'static str,
    /// Linux and WSL: `~/.local/bin/dv`, the link to this dv; `null` on Windows.
    pub link: Option<String>,
    /// Linux and WSL: the shell startup file a line was added to or taken out of, else `null`.
    pub rc_file: Option<String>,
}

/// `dv setup all` (and `dv setup` alone).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SetupAllDoc {
    /// One per check that needed fixing, in the order they ran.
    pub steps: Vec<SetupStepDto>,
}

/// One fix `dv setup all` made, or left to the user.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SetupStepDto {
    /// The environment's name: `Windows`, `Ubuntu`.
    pub environment: String,
    /// The check's id: `path`, `skill-claude-code`, `dv`, `gh`.
    pub check: String,
    /// `applied`, `failed`, or `manual` (only its `command` fixes it: `gh auth login`).
    pub outcome: &'static str,
    /// What was done, or why it failed; empty for `manual`.
    pub detail: String,
    /// The command that fixes it by hand.
    pub command: Option<String>,
}

/// A failure.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ErrorDto {
    /// snake_case: `usage`, `ambiguous_review`, `no_review`, `not_found`, … (`crate::error::code`).
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<CandidateDto>>,
}

/// Any command, failing.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ErrorDoc {
    pub error: ErrorDto,
}
