//! The review store over a [`Backend`] (docs/PLAN.md D5, §7): one append-only log per review
//! under `<git-common-dir>/dv/reviews/`, a snapshot cache of each fold next to it, and each
//! review's round snapshots under `refs/dv/reviews/<id>/`.
//!
//! The store itself doesn't judge events: [`ReviewStore::append`] writes what it is given and
//! the fold sorts it out (PLAN §7). The checked way to change a review is `review::ops`.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::event::{Event, EventBody, LogWarning, ParsedLog, parse_log};
use super::fold::{FoldError, fold};
use super::id::ReviewId;
use super::model::{Author, Review, ReviewSummary};
use crate::backend::{Backend, BackendError, GitOpts};
use crate::target::Target;

/// The store directory holding the logs, relative to the store root (`<git-common-dir>/dv/`).
const REVIEWS_DIR: &str = "reviews";

/// The snapshot cache's format; a snapshot of any other version is ignored and rebuilt. It
/// changes whenever the folded state does, so a cache folded by a build that ignored some
/// event field (a `status` note or condition, a relocated anchor), skipped an event this build
/// applies (a thread on a whole file, whose file-level anchor an older build refuses) or didn't
/// derive some state (the status a round closed from) or read a thread as something else (an
/// annotation, PLAN §19) is never taken for this build's fold.
const SNAPSHOT_VERSION: u32 = 7;

/// A review's log, relative to the store root.
fn log_path(id: &ReviewId) -> String {
    format!("{REVIEWS_DIR}/{id}.jsonl")
}

/// A review's snapshot cache, relative to the store root.
fn snapshot_path(id: &ReviewId) -> String {
    format!("{REVIEWS_DIR}/{id}.state.json")
}

/// The ref namespace of a review's round snapshots (PLAN §2.3).
fn refs_prefix(id: &ReviewId) -> String {
    format!("refs/dv/reviews/{id}")
}

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error("no review {id}")]
    UnknownReview { id: ReviewId },
    /// The log holds no review: its `created` line is missing or unreadable.
    #[error("review {id} is unreadable: {source}")]
    Corrupt {
        id: ReviewId,
        #[source]
        source: FoldError,
    },
    /// `created` is written only by [`ReviewStore::create`], as a log's first line.
    #[error("a created event can't be appended to an existing review")]
    CreatedEvent,
    #[error("couldn't encode the event: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("unexpected output from git {command}: {detail}")]
    UnexpectedOutput {
        command: &'static str,
        detail: String,
    },
}

/// A review read from its log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedReview {
    pub review: Review,
    /// Lines that didn't parse and events that didn't apply, each skipped (PLAN §7).
    pub warnings: Vec<LogWarning>,
    /// Events from a newer dv, skipped.
    pub newer_events: usize,
    /// Whether the state came from the snapshot cache rather than a fold of the log.
    pub from_cache: bool,
}

impl LoadedReview {
    /// The list row for this review, with its warning count.
    pub fn summary(&self) -> ReviewSummary {
        ReviewSummary {
            warnings: self.warnings.len(),
            ..self.review.summary()
        }
    }
}

/// One row of [`ReviewStore::list`]: a review, or the error that stopped it loading. One bad
/// log never hides the others (PLAN §7, §15).
#[derive(Debug)]
pub enum ListEntry {
    /// Boxed: a summary is several times the size of an error.
    Ok(Box<ReviewSummary>),
    Err {
        id: ReviewId,
        error: StoreError,
    },
}

impl ListEntry {
    pub fn id(&self) -> &ReviewId {
        match self {
            ListEntry::Ok(summary) => &summary.id,
            ListEntry::Err { id, .. } => id,
        }
    }
}

/// What a snapshot cache was folded from: the log's complete lines (its event count, PLAN §7),
/// its length and an FNV-1a hash of its bytes. A snapshot is used only when all three match the
/// log as it is now, which is far cheaper to check than parsing every line, and catches a log
/// that grew, was truncated or was edited in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LogStamp {
    lines: usize,
    len: u64,
    hash: String,
}

impl LogStamp {
    fn of(log: &[u8]) -> Self {
        Self {
            lines: log.iter().filter(|&&b| b == b'\n').count(),
            len: log.len() as u64,
            hash: format!("{:016x}", fnv1a64(log)),
        }
    }
}

/// 64-bit FNV-1a: a fast, stable fingerprint (not a security measure).
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes
        .iter()
        .fold(OFFSET, |hash, &b| (hash ^ u64::from(b)).wrapping_mul(PRIME))
}

/// The snapshot cache file, `<id>.state.json` (PLAN §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snapshot {
    v: u32,
    stamp: LogStamp,
    review: Review,
    warnings: Vec<LogWarning>,
    newer_events: usize,
}

/// Reviews stored in one repository (PLAN §7), shared by all its worktrees.
#[derive(Clone)]
pub struct ReviewStore {
    backend: Arc<dyn Backend>,
}

impl std::fmt::Debug for ReviewStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewStore")
            .field("common_dir", &self.backend.repo().common_dir)
            .finish()
    }
}

impl ReviewStore {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    /// The worktree this store's backend is bound to, which new reviews record.
    pub fn worktree(&self) -> &str {
        &self.backend.repo().toplevel
    }

    /// Creates a review of `target` in this worktree: a new log whose first line is the
    /// `created` event. Doesn't check for an open review of the same target; `ops::open_review`
    /// does (PLAN §2.4).
    pub fn create(&self, target: Target, author: Author) -> Result<Review, StoreError> {
        let id = ReviewId::generate();
        let event = Event::new(EventBody::Created {
            target,
            author,
            worktree: self.worktree().to_string(),
        });
        self.backend
            .store_append(&log_path(&id), &event.to_line()?)?;
        let folded = fold(&id, std::slice::from_ref(&event))
            .map_err(|source| StoreError::Corrupt { id, source })?;
        Ok(folded.review)
    }

    /// Appends a new event with `body` to an existing review and returns it as written.
    pub fn append(&self, id: &ReviewId, body: EventBody) -> Result<Event, StoreError> {
        let event = Event::new(body);
        self.append_event(id, &event)?;
        Ok(event)
    }

    /// Appends `event` to an existing review's log in one write (PLAN D5).
    ///
    /// A log whose last line has no newline ends in a write cut off by a crash; appending
    /// straight after it would glue the new event onto the torn line and lose both, so a lone
    /// newline is written first to end it. Two writers may both do that: the extra blank line
    /// is ignored by the reader.
    ///
    /// The log must exist. `store_append` creates a missing file, so an append racing a
    /// [`ReviewStore::delete`] can leave a log holding only the appended event; it lists as
    /// unreadable, and deleting the review again removes it.
    pub(crate) fn append_event(&self, id: &ReviewId, event: &Event) -> Result<(), StoreError> {
        if matches!(event.body, EventBody::Created { .. }) {
            return Err(StoreError::CreatedEvent);
        }
        let line = event.to_line()?;
        let rel = log_path(id);
        let log = self
            .backend
            .store_read(&rel)?
            .ok_or_else(|| StoreError::UnknownReview { id: id.clone() })?;
        if log.last().is_some_and(|&b| b != b'\n') {
            self.backend.store_append(&rel, b"")?;
        }
        self.backend.store_append(&rel, &line)?;
        Ok(())
    }

    /// The events of a review's log, in file order (PLAN §7).
    pub fn events(&self, id: &ReviewId) -> Result<ParsedLog, StoreError> {
        Ok(parse_log(&self.read_log(id)?))
    }

    /// Reads and folds a review. The snapshot cache answers when it was folded from exactly
    /// the log as it is now ([`LogStamp`]); otherwise the log is folded and the cache rewritten
    /// atomically. The cache is only ever an optimisation: a cache that can't be read or
    /// written is ignored.
    pub fn load(&self, id: &ReviewId) -> Result<LoadedReview, StoreError> {
        self.load_with(id, true)
    }

    /// [`ReviewStore::load`] for an operation about to append to the log (`review::ops`): a
    /// fresh cache still answers, but a fold doesn't rewrite it, because the operation's own
    /// append makes a snapshot of the state it loaded stale at once (PLAN §7). Writing one
    /// would cost every operation a durable file write that nothing reads.
    pub(crate) fn load_for_update(&self, id: &ReviewId) -> Result<LoadedReview, StoreError> {
        self.load_with(id, false)
    }

    fn load_with(&self, id: &ReviewId, write_cache: bool) -> Result<LoadedReview, StoreError> {
        let log = self.read_log(id)?;
        let stamp = LogStamp::of(&log);
        if let Some(snapshot) = self.read_snapshot(id)
            && snapshot.stamp == stamp
            && snapshot.review.id == *id
        {
            return Ok(LoadedReview {
                review: snapshot.review,
                warnings: snapshot.warnings,
                newer_events: snapshot.newer_events,
                from_cache: true,
            });
        }
        let parsed = parse_log(&log);
        let folded = fold(id, &parsed.events).map_err(|source| StoreError::Corrupt {
            id: id.clone(),
            source,
        })?;
        let mut warnings = parsed.warnings;
        warnings.extend(folded.warnings);
        let snapshot = Snapshot {
            v: SNAPSHOT_VERSION,
            stamp,
            review: folded.review,
            warnings,
            newer_events: parsed.newer_events,
        };
        if write_cache {
            self.write_snapshot(id, &snapshot);
        }
        Ok(LoadedReview {
            review: snapshot.review,
            warnings: snapshot.warnings,
            newer_events: snapshot.newer_events,
            from_cache: false,
        })
    }

    /// Every review in the store, newest first (ids start with their creation time). A log
    /// that fails to load is listed with its error, after the same ordering (PLAN §7, §15); a
    /// log deleted between the listing and its load is gone, not broken, and isn't listed.
    /// Files in `reviews/` that aren't named `<review id>.jsonl` are not reviews and are
    /// skipped.
    ///
    /// Also removes snapshot caches whose log is gone. A [`ReviewStore::load`] that read a log
    /// just before another process deleted the review writes its cache after the delete removed
    /// it; no later delete would find that file, since the review no longer lists. Writing it
    /// fires a store watch event, so the list that follows sweeps it. A cache is only an
    /// optimisation, so removing one in error costs a rebuild.
    pub fn list(&self) -> Result<Vec<ListEntry>, StoreError> {
        let names = self.backend.store_list(REVIEWS_DIR)?;
        let id_of = |name: &str, suffix: &str| ReviewId::parse(name.strip_suffix(suffix)?).ok();
        let logs: BTreeSet<ReviewId> = names
            .iter()
            .filter_map(|name| id_of(name, ".jsonl"))
            .collect();
        for id in names.iter().filter_map(|name| id_of(name, ".state.json")) {
            if !logs.contains(&id)
                && let Err(err) = self.backend.store_remove(&snapshot_path(&id))
            {
                log::debug!("review {id}: couldn't remove the orphaned snapshot cache: {err}");
            }
        }
        let mut entries: Vec<ListEntry> = logs
            .into_iter()
            .filter_map(|id| match self.load(&id) {
                Ok(loaded) => Some(ListEntry::Ok(Box::new(loaded.summary()))),
                Err(StoreError::UnknownReview { .. }) => None,
                Err(error) => Some(ListEntry::Err { id, error }),
            })
            .collect();
        entries.sort_by(|a, b| b.id().cmp(a.id()));
        Ok(entries)
    }

    /// Deletes a review: its log, its snapshot cache and its refs (`refs/dv/reviews/<id>/*`:
    /// its round snapshots, a PR review's pins, PLAN §7). `false` when there was no log. The
    /// log goes first, so the review disappears even if a later step fails, and the steps
    /// still run without a log, so deleting again finishes an interrupted delete. A cache
    /// written back by a load racing the delete is swept by [`ReviewStore::list`].
    ///
    /// What dv keeps for a pull request goes with its last review: the commits fetched under
    /// `refs/dv/pr/` and the mirror files (`crate::github::forget`), swept best effort once
    /// the review itself is gone; a sweep that fails is the next deletion's to finish.
    pub fn delete(&self, id: &ReviewId) -> Result<bool, StoreError> {
        let existed = self.backend.store_remove(&log_path(id))?;
        self.backend.store_remove(&snapshot_path(id))?;
        self.delete_refs(id)?;
        if let Err(error) = crate::github::forget::unreviewed(self) {
            log::warn!("review {id}: what dv keeps for its pull request wasn't removed: {error}");
        }
        Ok(existed)
    }

    /// Deletes every ref under `refs/dv/reviews/<id>/`, in one `git update-ref --stdin`
    /// transaction; returns how many there were.
    fn delete_refs(&self, id: &ReviewId) -> Result<usize, StoreError> {
        // `for-each-ref` matches a pattern literally up to a `/`, so `refs/dv/reviews/<id>`
        // lists exactly that review's refs (not another id that starts with the same text).
        let output = self.backend.git(
            &["for-each-ref", "--format=%(refname)", &refs_prefix(id)],
            &GitOpts::default(),
        )?;
        let text = output
            .stdout_utf8()
            .ok_or_else(|| StoreError::UnexpectedOutput {
                command: "for-each-ref",
                detail: "stdout is not UTF-8".to_string(),
            })?;
        let refs: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
        if refs.is_empty() {
            return Ok(0);
        }
        let commands: String = refs.iter().map(|name| format!("delete {name}\n")).collect();
        self.backend.git(
            &["update-ref", "--stdin"],
            &GitOpts::new().with_stdin(commands),
        )?;
        Ok(refs.len())
    }

    fn read_log(&self, id: &ReviewId) -> Result<Vec<u8>, StoreError> {
        self.backend
            .store_read(&log_path(id))?
            .ok_or_else(|| StoreError::UnknownReview { id: id.clone() })
    }

    /// The snapshot cache, when there is a readable one of this build's format.
    fn read_snapshot(&self, id: &ReviewId) -> Option<Snapshot> {
        let bytes = match self.backend.store_read(&snapshot_path(id)) {
            Ok(bytes) => bytes?,
            Err(err) => {
                log::debug!("review {id}: snapshot cache unreadable: {err}");
                return None;
            }
        };
        match serde_json::from_slice::<Snapshot>(&bytes) {
            Ok(snapshot) if snapshot.v == SNAPSHOT_VERSION => Some(snapshot),
            Ok(_) => None,
            Err(err) => {
                log::debug!("review {id}: snapshot cache invalid: {err}");
                None
            }
        }
    }

    fn write_snapshot(&self, id: &ReviewId, snapshot: &Snapshot) {
        let result = serde_json::to_vec(snapshot)
            .map_err(StoreError::from)
            .and_then(|bytes| {
                Ok(self
                    .backend
                    .store_write_atomic(&snapshot_path(id), &bytes)?)
            });
        // Only an optimisation: the next load folds the log again. Readers woken by the same
        // append (an agent's `dv review wait`, the app) all write the cache at once, and on
        // Windows the losers' replace fails while another holds the file, so a failure here is
        // routine and never reaches a command's stderr.
        if let Err(err) = result {
            log::debug!("review {id}: couldn't write the snapshot cache: {err}");
        }
    }
}
