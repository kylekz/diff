//! Review events and their log format (docs/PLAN.md D5, §7).
//!
//! A review is `<git-common-dir>/dv/reviews/<id>.jsonl`: one JSON event per line, appended in a
//! single write, never rewritten. Each line is an object with the format version `v`, a unique
//! `eid`, the time `at` (ms since the Unix epoch) and the event's `kind` with its fields:
//!
//! ```text
//! {"v":1,"eid":"e-01j…","at":1759560000000,"kind":"reply_added","thread":"t-01j…","id":"c-01j…","body":"Done.","author":{"kind":"agent","name":"claude-code"}}
//! ```
//!
//! Reading a log ([`parse_log`]) never fails as a whole: a line that isn't a valid event (a
//! line torn by a crash, a hand edit) becomes a warning and is skipped, and so is an event from a
//! newer dv — a higher `v`, or a `kind` this build doesn't know — which is counted instead, so
//! old builds keep reading logs that newer ones append to (PLAN §7).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::id::{CommentId, EventId, ItemId, ThreadId};
use super::model::{Anchor, Author, ReviewStatus, ThreadState, Verdict};
use crate::git::Oid;
use crate::target::Target;

/// The event format this build writes and understands; a line with a higher `v` comes from a
/// newer dv and is skipped.
pub const EVENT_VERSION: u32 = 1;

/// One line of a review log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// The format version, [`EVENT_VERSION`] for every event this build writes.
    pub v: u32,
    pub eid: EventId,
    /// When the event was written, in ms since the Unix epoch. Informational: the order of the
    /// lines in the file is the order events apply in, whatever the writers' clocks say.
    pub at: u64,
    #[serde(flatten)]
    pub body: EventBody,
}

impl Event {
    /// A new event with a fresh id, timestamped now.
    pub fn new(body: EventBody) -> Self {
        Self {
            v: EVENT_VERSION,
            eid: EventId::generate(),
            at: now_ms(),
            body,
        }
    }

    /// The event as one log line, without the trailing newline. JSON escapes control
    /// characters inside strings, so the line never contains a newline itself.
    pub fn to_line(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

/// What happened (PLAN §7). Serialised internally tagged: the variant is the line's `kind`, in
/// snake_case, next to its fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventBody {
    /// The first line of every log.
    Created {
        target: Target,
        author: Author,
        /// The worktree root the review belongs to (`RepoInfo::toplevel`): reviews of every
        /// worktree of a repo share one store, and a `working` target means this worktree's
        /// changes (PLAN §7).
        worktree: String,
    },
    /// Round `n` closed at these two sides (PLAN §2.3).
    RoundClosed {
        n: u32,
        new_oid: Oid,
        old_oid: Oid,
        /// `None` for a plain "Snapshot round".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verdict: Option<Verdict>,
        /// The reviewer's summary of the round, Markdown (the "Finish round" sheet's Summary,
        /// DESIGN §4.2).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
    },
    /// The reviewer marked `path` viewed (`on`) or not viewed in `round`.
    Viewed { round: u32, path: String, on: bool },
    /// A new thread. `annotation`: an agent's explanation of its own change rather than a
    /// comment (PLAN §19), which starts in [`ThreadState::Annotation`]; left out when false.
    ThreadAdded {
        id: ThreadId,
        anchor: Anchor,
        body: String,
        author: Author,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        annotation: bool,
    },
    /// A reply. `anchor`: the thread's anchor re-taken where relocation found its text in the
    /// round the reply was written in, when it had moved (PLAN §8); the thread is attached
    /// there from then on.
    ReplyAdded {
        thread: ThreadId,
        id: CommentId,
        body: String,
        author: Author,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<Anchor>,
    },
    /// A thread's state change, with an optional note and, like `reply_added`, the relocated
    /// anchor when the thread had moved (PLAN §8).
    ThreadState {
        thread: ThreadId,
        state: ThreadState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        author: Author,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<Anchor>,
    },
    /// A thread's opening comment or a reply got a new body.
    BodyEdited { target_id: ItemId, body: String },
    /// A thread or a reply was deleted (a tombstone, PLAN §7).
    Deleted { target_id: ItemId },
    PrLinked {
        owner: String,
        repo: String,
        number: u64,
    },
    /// The review was posted to GitHub as review `github_review_id` (PLAN §9.8).
    Submitted {
        github_review_id: u64,
        url: String,
        event: Verdict,
    },
    /// The review's status moved (PLAN §2.4). `note`: what the one who moved it says, Markdown
    /// (the agent's `dv review done --note`); `author`: who moved it. Both are left out by
    /// writers that don't know them. `if_status`: the move applies only to a review that has
    /// that status where the event lands in the log, and changes nothing otherwise (an agent's
    /// automatic `in_progress`, which a status written just before it must win over, PLAN
    /// §2.4). `if_round`: likewise, only to a review with exactly that many closed rounds
    /// where it lands (the agent's `dv review done`, which must not hand back a round the
    /// reviewer closed after the agent last looked).
    Status {
        status: ReviewStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        author: Option<Author>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_status: Option<ReviewStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_round: Option<u32>,
    },
}

impl EventBody {
    /// Every `kind` this build understands; a line of any other kind is from a newer dv.
    pub const KINDS: [&'static str; 11] = [
        "created",
        "round_closed",
        "viewed",
        "thread_added",
        "reply_added",
        "thread_state",
        "body_edited",
        "deleted",
        "pr_linked",
        "submitted",
        "status",
    ];

    /// This event's `kind` as it appears in the log.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Created { .. } => "created",
            Self::RoundClosed { .. } => "round_closed",
            Self::Viewed { .. } => "viewed",
            Self::ThreadAdded { .. } => "thread_added",
            Self::ReplyAdded { .. } => "reply_added",
            Self::ThreadState { .. } => "thread_state",
            Self::BodyEdited { .. } => "body_edited",
            Self::Deleted { .. } => "deleted",
            Self::PrLinked { .. } => "pr_linked",
            Self::Submitted { .. } => "submitted",
            Self::Status { .. } => "status",
        }
    }
}

/// Milliseconds since the Unix epoch, now; `0` if the clock is before 1970.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A problem found while reading or folding a log. Neither stops the fold: the line or event is
/// skipped and the rest applies (PLAN §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "problem", rename_all = "snake_case")]
pub enum LogWarning {
    /// A line that isn't a valid event: torn by a crash, hand-edited, written by a broken tool.
    #[error("line {line}: {error}")]
    Malformed {
        /// 1-based.
        line: usize,
        error: String,
    },
    /// A valid event that doesn't apply to the state it lands on ([`super::fold::Rejection`]).
    #[error("{kind} event {eid}: {reason}")]
    Rejected {
        eid: EventId,
        kind: String,
        reason: super::fold::Rejection,
    },
}

/// A log split into its events, in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedLog {
    pub events: Vec<Event>,
    /// Lines that aren't valid events ([`LogWarning::Malformed`]).
    pub warnings: Vec<LogWarning>,
    /// Events from a newer dv (a higher `v` or an unknown `kind`), skipped.
    pub newer_events: usize,
}

/// Splits a log into events (module docs). Blank lines are ignored; a line that doesn't parse
/// is a warning, including a final line without its newline (a write cut off by a crash).
pub fn parse_log(bytes: &[u8]) -> ParsedLog {
    let mut parsed = ParsedLog::default();
    let lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    let last = lines.len().saturating_sub(1);
    for (index, line) in lines.into_iter().enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match parse_line(line) {
            Ok(Line::Event(event)) => parsed.events.push(*event),
            Ok(Line::Newer) => parsed.newer_events += 1,
            Err(error) => {
                // `split` yields the text after the last newline last: a line without one.
                let error = if index == last {
                    format!("incomplete last line: {error}")
                } else {
                    error
                };
                parsed.warnings.push(LogWarning::Malformed {
                    line: index + 1,
                    error,
                });
            }
        }
    }
    parsed
}

enum Line {
    Event(Box<Event>),
    /// From a newer dv: skipped.
    Newer,
}

fn parse_line(line: &[u8]) -> Result<Line, String> {
    let value: serde_json::Value = serde_json::from_slice(line).map_err(|err| err.to_string())?;
    let object = value.as_object().ok_or("not a JSON object")?;
    // The version decides first: a newer format may change anything else about the line.
    let v = object
        .get("v")
        .ok_or("no \"v\"")?
        .as_u64()
        .ok_or("\"v\" is not a version number")?;
    if v == 0 {
        return Err("\"v\" is 0".to_string());
    }
    if v > u64::from(EVENT_VERSION) {
        return Ok(Line::Newer);
    }
    let kind = object
        .get("kind")
        .ok_or("no \"kind\"")?
        .as_str()
        .ok_or("\"kind\" is not a string")?;
    if !EventBody::KINDS.contains(&kind) {
        return Ok(Line::Newer);
    }
    let event: Event = serde_json::from_value(value).map_err(|err| err.to_string())?;
    Ok(Line::Event(Box::new(event)))
}
