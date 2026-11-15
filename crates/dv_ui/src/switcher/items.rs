//! What the switcher lists, as plain data built from the reviews service's rows ([`build`]):
//! a header per repository, then the rows that can be picked.
//!
//! A repository that opens offers its targets — uncommitted changes, staged changes and, when a
//! branch other than the default one is checked out, that branch against the default — each
//! beside its open review when it has one (picking it resumes that review, PLAN §2.4), then its
//! other open reviews, and under a "Pull requests" label its PR reviews, drafts and
//! submitted ones, each by its pull request's number and title (PLAN §9.9). A repository that can't be opened lists none and says why; one whose
//! distro isn't running offers one row, **Start Ubuntu**, and one whose connection failed one
//! row, **Retry**, each of which connects and then lists the repository as usual (PLAN §5.2);
//! while it does, the row stays, inert ([`Item::is_starting`]), so the selection stays on it
//! and picking it again does nothing. One whose connection is reconnecting says so and keeps
//! the rows it had. "Open folder…" closes the list, whatever the filter.
//!
//! The filter keeps the rows whose repository name, path and label hold every word typed,
//! ignoring case; a repository whose header matches but offers nothing still shows its header.

use dv_core::review::{ReviewId, ReviewStatus, ReviewSummary, displayed_round};
use dv_core::{Location, Target};

use crate::components::status_label;
use crate::services::reviews::{OpenRequest, RepoRow, ReviewRow, WorkingSummary};
use dv_core::label::target_label;

/// The targets every repository offers (the source picker's menu too).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Working,
    Staged,
    Branch,
}

impl TargetKind {
    /// `"working"`, `"staged"` or `"branch"`: row keys and automation state.
    pub fn key(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Staged => "staged",
            Self::Branch => "branch",
        }
    }
}

/// An open review as a row shows it: its status, round and open threads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewBadge {
    pub id: ReviewId,
    pub status: ReviewStatus,
    pub round: u32,
    pub open_threads: u32,
}

impl ReviewBadge {
    pub(super) fn of(summary: &ReviewSummary) -> Self {
        Self {
            id: summary.id.clone(),
            status: summary.status,
            round: summary.current_round,
            open_threads: summary.threads.open,
        }
    }
}

/// One of a repository's targets, as the switcher and the source picker offer it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetOffer {
    pub kind: TargetKind,
    pub target: Target,
    /// "Uncommitted changes", "Staged changes", "Branch feat vs main".
    pub label: String,
    /// Its open review in this worktree, if any.
    pub review: Option<ReviewBadge>,
}

/// The targets `row`'s repository offers: none when it can't be opened or read
/// ([`RepoRow::is_readable`]), since a review there could only fail, or write a store into a
/// folder that is no longer a repository.
pub fn target_offers(row: &RepoRow) -> Vec<TargetOffer> {
    if !row.is_readable() {
        return Vec::new();
    }
    let mut offers = vec![
        (
            TargetKind::Working,
            Target::Working,
            "Uncommitted changes".to_string(),
        ),
        (
            TargetKind::Staged,
            Target::Staged,
            "Staged changes".to_string(),
        ),
    ];
    if let Some(target) = row.labels.branch_target() {
        let label = format!("Branch {}", target_label(&target, &row.labels));
        offers.push((TargetKind::Branch, target, label));
    }
    offers
        .into_iter()
        .map(|(kind, target, label)| TargetOffer {
            review: row.open_review_of(&target).map(ReviewBadge::of),
            kind,
            target,
            label,
        })
        .collect()
}

/// What a pickable row does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// Review one of the repository's targets.
    Target(TargetOffer),
    /// Resume an open review of another target.
    Review { label: String, badge: ReviewBadge },
    /// Open a PR review, a draft or a submitted one (PLAN §9.9): `label` is `#482 Title`
    /// (`#482` before the pull request was read here), as drawn whatever the title holds;
    /// its badge's round is the pull request's (`dv_core::review::displayed_round`).
    Pr { label: String, badge: ReviewBadge },
    /// Start the WSL distro the repository is in, which isn't running; `starting` while that
    /// start runs.
    Start { label: String, starting: bool },
    /// Connect again to the repository, whose connection failed; `starting` while that runs.
    Retry { starting: bool },
    /// The folder picker, to add a repository.
    OpenFolder,
    /// "Compare branches…": the branch picker over the repository's branches ([`super::branches`]).
    CompareBranches,
    /// The branch picker's first step: a branch, compared with `base` unless another is picked.
    Branch {
        name: String,
        /// The base it is offered against: its stack base, else the default branch; `None` with
        /// neither.
        base: Option<String>,
        /// The open review of the branch against `base`.
        review: Option<ReviewBadge>,
    },
    /// The branch picker's second step: a base to compare the picked branch with.
    Base {
        offer: TargetOffer,
        /// Why it is offered first: "stack base" or "default branch".
        note: Option<&'static str>,
    },
}

/// A pickable row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// A stable name for the row: `<repo>/working`, `<repo>/staged`, `<repo>/branch`,
    /// `<repo>/review/<id>`, `<repo>/pr/<id>`, `<repo>/start`, `<repo>/retry`, `<repo>/compare` or
    /// `open_folder`, where `<repo>` is the repository's name (its full path when two known
    /// repositories share a name); in the branch picker `<repo>/pick/<branch>` and
    /// `<repo>/base/<branch>`.
    pub key: String,
    /// Its repository, an index into [`Listing::repos`].
    pub repo: Option<usize>,
    pub kind: ItemKind,
}

impl Item {
    /// What the row reads.
    pub fn label(&self) -> &str {
        match &self.kind {
            ItemKind::Target(offer) => &offer.label,
            ItemKind::Review { label, .. }
            | ItemKind::Pr { label, .. }
            | ItemKind::Start { label, .. } => label,
            ItemKind::Retry { .. } => "Retry",
            ItemKind::OpenFolder => "Open folder…",
            ItemKind::CompareBranches => "Compare branches…",
            ItemKind::Branch { name, .. } => name,
            ItemKind::Base { offer, .. } => &offer.label,
        }
    }

    /// What a branch picker's row reads after its label, dimmed: "vs feat-a", "stack base".
    pub fn detail(&self) -> Option<String> {
        match &self.kind {
            ItemKind::Branch { base, .. } => base.as_ref().map(|base| format!("vs {base}")),
            ItemKind::Base { note, .. } => note.map(str::to_string),
            _ => None,
        }
    }

    /// A **Start** or **Retry** row whose start runs: drawn inert, and picking it does nothing
    /// (module docs).
    pub fn is_starting(&self) -> bool {
        matches!(
            self.kind,
            ItemKind::Start { starting: true, .. } | ItemKind::Retry { starting: true }
        )
    }

    /// The review the row stands for or resumes: an open one, or a PR review that was
    /// submitted.
    pub fn review(&self) -> Option<&ReviewBadge> {
        match &self.kind {
            ItemKind::Target(offer) => offer.review.as_ref(),
            ItemKind::Review { badge, .. } | ItemKind::Pr { badge, .. } => Some(badge),
            ItemKind::Branch { review, .. } => review.as_ref(),
            ItemKind::Base { offer, .. } => offer.review.as_ref(),
            ItemKind::Start { .. }
            | ItemKind::Retry { .. }
            | ItemKind::OpenFolder
            | ItemKind::CompareBranches => None,
        }
    }

    /// [`Item::review`] while it is open: what **Close review** closes.
    pub fn open_review(&self) -> Option<&ReviewBadge> {
        self.review().filter(|badge| badge.status.is_open())
    }

    /// `"target"`, `"review"`, `"pr"`, `"start"`, `"retry"`, `"open_folder"`, `"compare"`, `"branch"`
    /// or `"base"`, for automation state.
    pub fn kind_key(&self) -> &'static str {
        match self.kind {
            ItemKind::Target(_) => "target",
            ItemKind::Review { .. } => "review",
            ItemKind::Pr { .. } => "pr",
            ItemKind::Start { .. } => "start",
            ItemKind::Retry { .. } => "retry",
            ItemKind::OpenFolder => "open_folder",
            ItemKind::CompareBranches => "compare",
            ItemKind::Branch { .. } => "branch",
            ItemKind::Base { .. } => "base",
        }
    }
}

/// The second line of a repository's header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoSummary {
    /// Nothing computed yet.
    Loading,
    /// It can't be opened; the few words why.
    Unreachable(String),
    /// Its WSL distro isn't running: the distro, which the row's **Start** starts.
    Stopped(String),
    /// Its distro is being started, or its connection retried: the line ("Starting Ubuntu…",
    /// "connecting…").
    Starting(String),
    /// Part of the row couldn't be read.
    Error(String),
    Clean,
    Changes(WorkingSummary),
}

/// A repository's header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoHeader {
    pub location: Location,
    /// The name rows are keyed by (see [`Item::key`]).
    pub key: String,
    pub name: String,
    /// Its full location, `RepoRow::path`.
    pub path: String,
    /// The path the header shows beside its location chip: a WSL repository's path inside
    /// its distro (the chip names the distro), else the full path.
    pub shown_path: String,
    /// The distro of a WSL repository, for its location chip.
    pub distro: Option<String>,
    pub summary: RepoSummary,
}

/// A line of the list, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Repo(usize),
    Item(usize),
    /// A label over the rows that follow it, inside a repository's.
    Section(&'static str),
}

/// What the switcher shows for a filter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub repos: Vec<RepoHeader>,
    /// The pickable rows, in order, "Open folder…" last.
    pub items: Vec<Item>,
    /// The scrolling list's lines (headers and rows, "Open folder…" left out: it sits in the
    /// footer).
    pub entries: Vec<Entry>,
}

impl Listing {
    /// Where `key`'s row is.
    pub fn position(&self, key: &str) -> Option<usize> {
        self.items.iter().position(|item| item.key == key)
    }

    /// The line `item` is on.
    pub fn entry_of(&self, item: usize) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| *entry == Entry::Item(item))
    }

    /// What picking `item` opens, with its repository; `None` for "Open folder…" and the rows
    /// that lead to another step of the branch picker.
    pub fn request(&self, item: usize) -> Option<(Location, OpenRequest)> {
        let item = self.items.get(item)?;
        let location = self.repos.get(item.repo?)?.location.clone();
        let request = match &item.kind {
            ItemKind::Target(offer) => OpenRequest::Target(offer.target.clone()),
            ItemKind::Review { badge, .. } | ItemKind::Pr { badge, .. } => {
                OpenRequest::Review(badge.id.clone())
            }
            ItemKind::Base { offer, .. } => OpenRequest::Target(offer.target.clone()),
            ItemKind::Start { .. }
            | ItemKind::Retry { .. }
            | ItemKind::OpenFolder
            | ItemKind::CompareBranches
            | ItemKind::Branch { .. } => {
                return None;
            }
        };
        Some((location, request))
    }

    /// The repository of `item`; `None` for "Open folder…".
    pub fn location_of(&self, item: usize) -> Option<&Location> {
        let item = self.items.get(item)?;
        Some(&self.repos.get(item.repo?)?.location)
    }

    /// The first row of the repository listed after `location`'s, else of the one before it,
    /// else "Open folder…": where the selection goes when `location` is removed.
    pub fn row_after_removing(&self, location: &Location) -> Option<&str> {
        let repo = self
            .repos
            .iter()
            .position(|repo| &repo.location == location)?;
        let first_of = |repo: usize| self.items.iter().find(|item| item.repo == Some(repo));
        first_of(repo + 1)
            .or_else(|| repo.checked_sub(1).and_then(first_of))
            .or_else(|| self.items.last())
            .map(|item| item.key.as_str())
    }

    /// The repository picking `item` connects to, when it is a **Start** or **Retry** row
    /// (whose start doesn't run already).
    pub fn start(&self, item: usize) -> Option<Location> {
        let item = self.items.get(item)?;
        match item.kind {
            ItemKind::Start {
                starting: false, ..
            }
            | ItemKind::Retry { starting: false } => {
                Some(self.repos.get(item.repo?)?.location.clone())
            }
            _ => None,
        }
    }

    /// The first row of the repository at `location`.
    pub fn first_of(&self, location: &Location) -> Option<usize> {
        let repo = self
            .repos
            .iter()
            .position(|repo| &repo.location == location)?;
        self.items.iter().position(|item| item.repo == Some(repo))
    }

    /// The first row of the repository at `location` that can be picked: not a start that
    /// runs.
    pub fn first_ready_of(&self, location: &Location) -> Option<usize> {
        let first = self.first_of(location)?;
        let repo = self.items.get(first)?.repo;
        self.items
            .iter()
            .enumerate()
            .skip(first)
            .take_while(|(_, item)| item.repo == repo)
            .find(|(_, item)| !item.is_starting())
            .map(|(ix, _)| ix)
    }

    /// The row that stands for the open review `id`.
    pub fn review_row(&self, id: &ReviewId) -> Option<usize> {
        self.items
            .iter()
            .position(|item| item.review().is_some_and(|badge| &badge.id == id))
    }
}

/// The label over a repository's PR reviews.
pub const PULL_REQUESTS: &str = "Pull requests";

/// A pull request as a row and the title bar name it (PLAN §9.9): `#482 Title`, or `#482`
/// before it was read here. The title is its author's text, so it is drawn as one line of
/// bounded length whatever it holds (`crate::thread::drawn`).
pub fn pr_label(number: u64, title: Option<&str>) -> String {
    match title.map(str::trim).filter(|title| !title.is_empty()) {
        Some(title) => format!("#{number} {}", crate::thread::drawn(title)),
        None => format!("#{number}"),
    }
}

/// The `key` of every row of `rows`: their names, or their paths where names repeat.
pub(crate) fn repo_keys(rows: &[&RepoRow]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let shared = rows.iter().filter(|other| other.name == row.name).count() > 1;
            if shared {
                row.path.clone()
            } else {
                row.name.clone()
            }
        })
        .collect()
}

fn summary(row: &RepoRow) -> RepoSummary {
    if row.starting {
        return RepoSummary::Starting(match (&row.stopped, &row.location) {
            (Some(distro), _) => format!("Starting {distro}…"),
            (None, Location::Wsl { distro, .. }) => format!("{distro} · connecting…"),
            (None, Location::Local(_)) => "connecting…".to_string(),
        });
    }
    if let Some(distro) = &row.stopped {
        return RepoSummary::Stopped(distro.clone());
    }
    if row.reconnecting {
        return RepoSummary::Unreachable("reconnecting…".to_string());
    }
    if let Some(reason) = &row.unreachable {
        // A repository reached without a connection (no Retry to offer) has none to be down:
        // what kept it from opening (its folder gone, no longer a repository) is a read error,
        // in `red` (DESIGN §4.1), and the header shows its path already.
        if !row.retry {
            let reason = reason
                .strip_suffix(&format!(": {}", row.path))
                .unwrap_or(reason);
            return RepoSummary::Error(reason.to_string());
        }
        return RepoSummary::Unreachable(reason.clone());
    }
    match (row.working, &row.error) {
        (Some(working), _) if working.files == 0 => RepoSummary::Clean,
        (Some(working), _) => RepoSummary::Changes(working),
        (None, Some(error)) => RepoSummary::Error(error.message.clone()),
        (None, None) => RepoSummary::Loading,
    }
}

/// Whether every word of `query` is in `haystack` (already lowercase).
pub(super) fn matches(haystack: &str, words: &[String]) -> bool {
    words.iter().all(|word| haystack.contains(word.as_str()))
}

/// The header of `row`'s repository, keyed `key`.
pub(super) fn header(row: &RepoRow, key: String) -> RepoHeader {
    let (distro, shown_path) = match &row.location {
        Location::Wsl { distro, path } => (Some(distro.clone()), path.clone()),
        Location::Local(_) => (None, row.path.clone()),
    };
    RepoHeader {
        location: row.location.clone(),
        key,
        name: row.name.clone(),
        path: row.path.clone(),
        shown_path,
        distro,
        summary: summary(row),
    }
}

/// The listing for `rows` (most recently used first) under `query`.
pub fn build(rows: &[&RepoRow], query: &str) -> Listing {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let keys = repo_keys(rows);
    let mut listing = Listing::default();
    for (row, key) in rows.iter().zip(keys) {
        let repo_text = format!("{} {}", row.name, row.path).to_lowercase();
        let mut items: Vec<Item> = target_offers(row)
            .into_iter()
            .map(|offer| Item {
                key: format!("{key}/{}", offer.kind.key()),
                repo: None,
                kind: ItemKind::Target(offer),
            })
            .collect();
        // A stopped distro's repository offers to start it, and one whose connection failed
        // to retry it, and nothing else (PLAN §5.2); the row stays while the start runs.
        if let Some(distro) = &row.stopped {
            items.push(Item {
                key: format!("{key}/start"),
                repo: None,
                kind: ItemKind::Start {
                    label: format!("Start {distro}"),
                    starting: row.starting,
                },
            });
        } else if row.retry {
            items.push(Item {
                key: format!("{key}/retry"),
                repo: None,
                kind: ItemKind::Retry {
                    starting: row.starting,
                },
            });
        }
        let offered: Vec<ReviewId> = items
            .iter()
            .filter_map(|item| item.review().map(|badge| badge.id.clone()))
            .collect();
        // A repository that can't be read offers no rows at all (DESIGN §4.1).
        let reviews = if row.is_readable() {
            row.reviews.as_slice()
        } else {
            &[]
        };
        for review in reviews {
            let ReviewRow::Ok { summary, label, pr } = review else {
                continue;
            };
            // A pull request's reviews have their own section, below.
            if pr.is_some() || !summary.status.is_open() || offered.contains(&summary.id) {
                continue;
            }
            items.push(Item {
                key: format!("{key}/review/{}", summary.id),
                repo: None,
                kind: ItemKind::Review {
                    label: label.clone(),
                    badge: ReviewBadge::of(summary),
                },
            });
        }
        // The branch picker, for a repository with a branch to compare (DESIGN §4.1).
        if row.is_readable() && super::branches::has_branches(row) {
            items.push(Item {
                key: format!("{key}/compare"),
                repo: None,
                kind: ItemKind::CompareBranches,
            });
        }
        // Its PR reviews (PLAN §9.9), newest first as the store lists them: the drafts and
        // the submitted ones; a closed one is listed nowhere.
        for review in reviews {
            let ReviewRow::Ok {
                summary,
                pr: Some(pr),
                ..
            } = review
            else {
                continue;
            };
            if !matches!(
                summary.status,
                ReviewStatus::Draft | ReviewStatus::Submitted
            ) {
                continue;
            }
            items.push(Item {
                key: format!("{key}/pr/{}", summary.id),
                repo: None,
                kind: ItemKind::Pr {
                    label: pr_label(pr.number, pr.title.as_deref()),
                    badge: ReviewBadge {
                        round: displayed_round(summary.current_round, Some(pr.round)),
                        ..ReviewBadge::of(summary)
                    },
                },
            });
        }
        items.retain(|item| {
            let status = item
                .review()
                .map(|badge| status_label(badge.status))
                .unwrap_or_default();
            let text = format!("{repo_text} {} {status}", item.label()).to_lowercase();
            matches(&text, &words)
        });
        if items.is_empty() && !matches(&repo_text, &words) {
            continue;
        }
        let repo = listing.repos.len();
        listing.repos.push(header(row, key));
        listing.entries.push(Entry::Repo(repo));
        let mut in_prs = false;
        for mut item in items {
            item.repo = Some(repo);
            // The label over a repository's first PR review.
            let pr = matches!(item.kind, ItemKind::Pr { .. });
            if pr && !in_prs {
                listing.entries.push(Entry::Section(PULL_REQUESTS));
            }
            in_prs = pr;
            listing.entries.push(Entry::Item(listing.items.len()));
            listing.items.push(item);
        }
    }
    listing.items.push(Item {
        key: "open_folder".to_string(),
        repo: None,
        kind: ItemKind::OpenFolder,
    });
    listing
}
