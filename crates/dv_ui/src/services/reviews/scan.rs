//! A repository's row as a background scan reads it ([`scan`]): its reviews, with a PR
//! review's pull request as the store holds it, its uncommitted changes, its labels and its
//! branches.

use std::collections::HashMap;
use std::sync::Arc;

use dv_core::diff::file_list;
use dv_core::git::branches::Branch;
use dv_core::github::MirrorStore;
use dv_core::github::fetch::refs_prefix;
use dv_core::label::{LabelContext, Labels};
use dv_core::review::{ListEntry, PrLink, ReviewId, ReviewStore, ReviewSummary};
use dv_core::{Backend, GitRepo, Location, Target};

use super::{PrRow, ReviewRow, WorkingSummary};
use crate::failure::Failure;

/// What a background scan of a repository found.
pub(super) struct Scan {
    pub(super) working: Result<WorkingSummary, Failure>,
    pub(super) reviews: Result<Vec<ReviewRow>, Failure>,
    pub(super) labels: LabelContext,
    /// Empty when they couldn't be read: the picker then offers none, the row the rest.
    pub(super) branches: Vec<Branch>,
    pub(super) toplevel: String,
    /// The repository's location as its backend resolved it: its worktree root.
    pub(super) root: Location,
    /// The backend it was read through, for what is read after the row lands.
    pub(super) backend: Arc<dyn Backend>,
}

impl Scan {
    /// The connection died under the scan: what it read is the reconnect's to read again.
    pub(super) fn lost_connection(&self) -> bool {
        [self.working.as_ref().err(), self.reviews.as_ref().err()]
            .into_iter()
            .flatten()
            .any(|failure| failure.disconnected)
    }
}

/// Reads a repository's row (blocking): its reviews, its uncommitted changes and its labels.
pub(super) fn scan(backend: Arc<dyn Backend>) -> Scan {
    let toplevel = backend.repo().toplevel.clone();
    let root = backend.repo().location.clone();
    let scanned = backend.clone();
    let repo = GitRepo::new(backend.clone());
    // Another worktree's `working` or `staged` review is named by that worktree's HEAD.
    let all_labels = Labels::read(&repo);
    let labels = all_labels.here.clone();
    let store = ReviewStore::new(backend.clone());
    let reviews = store
        .list()
        .map(|entries| {
            entries
                .into_iter()
                .map(|entry| match entry {
                    ListEntry::Ok(summary) => ReviewRow::Ok {
                        label: all_labels.label(&summary.target, &summary.worktree),
                        pr: None,
                        summary,
                    },
                    ListEntry::Err { id, error } => ReviewRow::Err {
                        id,
                        error: Failure::from(&error),
                    },
                })
                .collect()
        })
        .map(|rows| with_pull_requests(rows, &store))
        .map_err(|error| Failure::from(&error));
    let working = Target::Working
        .resolve(&repo)
        .map_err(|error| Failure::from(&error))
        .and_then(|spec| file_list(&repo, &spec).map_err(|error| Failure::from(&error)))
        .map(|files| WorkingSummary {
            files: files.len(),
            additions: files.iter().map(|file| u64::from(file.additions)).sum(),
            deletions: files.iter().map(|file| u64::from(file.deletions)).sum(),
        });
    let branches = repo
        .branches(labels.default_base.as_deref())
        .unwrap_or_else(|error| {
            log::warn!("couldn't list the branches of {toplevel}: {error}");
            Vec::new()
        });
    Scan {
        working,
        reviews,
        labels,
        branches,
        toplevel,
        root,
        backend: scanned,
    }
}

/// The pull request `summary`'s review is of, when its target is one (`Target::Pr`): a
/// local review linked to a pull request isn't a PR review.
fn pr_target(summary: &ReviewSummary) -> Option<&PrLink> {
    summary
        .pr
        .as_ref()
        .filter(|link| link.is_target(&summary.target))
}

/// `rows` with each PR review's pull request (PLAN §9.9): its number, its title as the
/// most recently read mirror has it (from the mirror's summary, a few hundred bytes, once
/// per pull request: `MirrorStore::latest_title`, PLAN §9.4; whichever login read it and
/// under whichever host a remote names; no `gh`), and the review's PR round. A pull request
/// that was never read here has no title. A PR review is one whose target is the pull
/// request ([`pr_target`]), as everywhere a PR round is counted (`PrLink::round_of`).
fn with_pull_requests(mut rows: Vec<ReviewRow>, store: &ReviewStore) -> Vec<ReviewRow> {
    let targets: Vec<(ReviewId, Target)> = rows
        .iter()
        .filter_map(|row| match row {
            ReviewRow::Ok { summary, .. } if pr_target(summary).is_some() => {
                Some((summary.id.clone(), summary.target.clone()))
            }
            _ => None,
        })
        .collect();
    if targets.is_empty() {
        return rows;
    }
    let mirrors = MirrorStore::new(store.backend().clone());
    let hosts = mirrors.hosts().unwrap_or_default();
    let mut titles: HashMap<String, Option<String>> = HashMap::new();
    for row in &mut rows {
        let ReviewRow::Ok { summary, pr, .. } = row else {
            continue;
        };
        let Some(link) = pr_target(summary) else {
            continue;
        };
        let title = titles
            .entry(refs_prefix(&link.owner, &link.repo, link.number))
            .or_insert_with(|| mirrors.latest_title(&hosts, link).ok().flatten())
            .clone();
        *pr = Some(PrRow {
            number: link.number,
            title,
            round: link.round_of(&summary.id, targets.iter().map(|(id, target)| (id, target))),
        });
    }
    rows
}
