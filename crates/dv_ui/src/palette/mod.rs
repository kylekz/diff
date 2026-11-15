//! The palette (`Ctrl K`, or a click on the title bar's command field; PLAN §11 `palette`,
//! DESIGN §4.1 "Palette"): one filter field over commands, reviews and files, in an overlay hung
//! under the command field over the scrim, in the file finder's look.
//!
//! **Rows**, in sections and in this order ([`build`]):
//! - **Pull request**, when what is typed is a pull request's URL or `owner/repo#n`, or `#n`
//!   for the repository in view (a bare number stays a search; PLAN §9.9, DESIGN §4.5): a
//!   row per known clone of its repository, the most recently used first, or, when dv knows
//!   none, one row that asks for its folder. Decided from what dv holds
//!   (`crate::pr_picker::reference`): no `gh` runs for what is typed.
//! - **Commands** ([`commands::COMMANDS`]): the shell's and the stream's actions by name, with
//!   their keycaps; the stream's only while a review is open, a pull request's only while a
//!   PR review is ([`commands::Scope`]).
//! - **Reviews**: every known repository's targets and open reviews as the switcher lists them
//!   (`crate::switcher::items::build`, the switcher's own filter: repository, label, status).
//! - **Files**: with a review open and something typed, the file finder's first
//!   [`FILE_ROWS`] matches (`crate::file_finder::filter`); the rest are `Ctrl P`'s.
//!
//! A command passes when every word typed is in its title or its other words, ignoring case.
//! An empty filter lists the commands and the reviews. The first row is selected; `↑`/`↓` move
//! the selection (wrapping) across the sections, `Enter` picks, `Esc` closes. The palette only
//! reports what was picked ([`PaletteEvent`]); the shell runs it. Rows refresh as the reviews
//! service recomputes them and as the review's side paths land, keeping the selected row.
//!
//! Automation targets: `palette`, `palette/input`, `palette/empty`, `palette/row/<key>` for
//! each row ([`Row::key`]).

pub mod commands;
mod render;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use dv_core::Location;
use dv_core::diff::FileStatus;
use gpui::{
    AppContext as _, Context, Entity, EventEmitter, Focusable as _, ScrollHandle, Subscription,
    Window,
};
use gpui_component::input::{InputEvent, InputState};

use crate::actions::palette::{Confirm, Dismiss, SelectNext, SelectPrevious};
use dv_core::github::PrRef;

use crate::file_finder::Candidate;
use crate::pr_picker::reference::{PrOffer, offer};
use crate::services::github::GitHub;
use crate::services::reviews::{OpenRequest, Reviews};
use crate::session::ReviewSession;
use crate::switcher::items::{self, ItemKind, ReviewBadge, TargetKind};
use commands::{COMMANDS, Command, Scope};

/// The panel's width (the switcher's and the file finder's).
pub const PALETTE_WIDTH: gpui::Pixels = gpui::px(560.);
/// The filter field's placeholder: the command field's words.
pub const PLACEHOLDER: &str = "Go to file, review or command";
/// How many files the palette lists at most.
pub const FILE_ROWS: usize = 5;
/// How many rows `state.palette` reports.
pub const STATE_ROWS: usize = 40;

/// The palette's sections, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    PullRequest,
    Commands,
    Reviews,
    Files,
}

impl Section {
    /// Its label over its rows.
    pub fn title(self) -> &'static str {
        match self {
            Self::PullRequest => "Pull request",
            Self::Commands => "Commands",
            Self::Reviews => "Reviews",
            Self::Files => "Files",
        }
    }

    /// `"pull_request"`, `"commands"`, `"reviews"` or `"files"`, for keys and automation
    /// state.
    pub fn key(self) -> &'static str {
        match self {
            Self::PullRequest => "pull_request",
            Self::Commands => "commands",
            Self::Reviews => "reviews",
            Self::Files => "files",
        }
    }
}

/// What a row does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// Open the pull request the query names in a known clone of its repository.
    Pr {
        /// `acme/frontend#482`, or `#482`.
        name: String,
        /// Its title, when a list read this session holds it.
        title: Option<String>,
        /// The clone's name, shown when the pull request's repository has several.
        clone: Option<String>,
        location: Location,
        reference: PrRef,
    },
    /// The pull request the query names is in a repository dv knows no clone of: ask for
    /// its folder.
    ChooseFolder {
        /// `acme/frontend`.
        repository: String,
        reference: PrRef,
    },
    Command(Command),
    /// Open one of a repository's targets, or resume an open review.
    Review {
        /// The repository's name.
        repo: String,
        /// "Uncommitted changes", "Branch feat vs main", a review's label.
        label: String,
        /// The target's kind; `None` for an open review of another target.
        target: Option<TargetKind>,
        /// Its open review, if any.
        badge: Option<ReviewBadge>,
        location: Location,
        request: OpenRequest,
    },
    /// Open this file of the review in the file view.
    File {
        path: String,
        status: Option<FileStatus>,
    },
}

/// One pickable row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// A stable name: `pr/<the clone's key>` (the switcher's repository key) or
    /// `pr/choose_folder`, `command/<action>` (`command/diff_pane::NextFile`), `review/<the
    /// switcher's key>` (`review/api-server/working`) or `file/<path>`.
    pub key: String,
    pub section: Section,
    pub kind: RowKind,
}

impl Row {
    /// What the row reads.
    pub fn label(&self) -> String {
        match &self.kind {
            RowKind::Pr { name, .. } => format!("Open pull request {name}"),
            RowKind::ChooseFolder { repository, .. } => {
                format!("{repository} isn't open in dv yet")
            }
            RowKind::Command(command) => command.title.to_string(),
            RowKind::Review { repo, label, .. } => format!("{repo} · {label}"),
            RowKind::File { path, .. } => path.clone(),
        }
    }
}

/// The pull request `query` names, when it is a reference the palette takes for one (module
/// docs): a bare number is a search.
pub fn pr_reference(query: &str) -> Option<PrRef> {
    PrRef::parse(query).filter(|reference| !matches!(reference, PrRef::Number(_)))
}

/// The rows for `query` (module docs, "Rows"): the pull request it names as `offered`
/// resolved it, `commands` filtered by it (those needing a review, or a PR review, only when
/// what is `open` has), the reviews `listing` lists for it, and the first of `files` passing
/// it (only with a review open and `query` non-empty).
pub fn build(
    query: &str,
    open: Scope,
    listing: &items::Listing,
    files: &[Candidate],
    offered: Option<&PrOffer>,
) -> Vec<Row> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut rows: Vec<Row> = match offered {
        Some(PrOffer::Clones {
            reference,
            name,
            clones,
        }) => clones
            .iter()
            .map(|clone| Row {
                key: format!("pr/{}", clone.key),
                section: Section::PullRequest,
                kind: RowKind::Pr {
                    name: name.clone(),
                    title: clone.title.clone(),
                    clone: (clones.len() > 1).then(|| clone.name.clone()),
                    location: clone.location.clone(),
                    reference: reference.clone(),
                },
            })
            .collect(),
        Some(PrOffer::Unknown {
            reference,
            repository,
        }) => vec![Row {
            key: "pr/choose_folder".to_string(),
            section: Section::PullRequest,
            kind: RowKind::ChooseFolder {
                repository: repository.clone(),
                reference: reference.clone(),
            },
        }],
        None => Vec::new(),
    };
    rows.extend(
        COMMANDS
            .iter()
            .filter(|command| command.needs <= open)
            .filter(|command| {
                let text = command.text();
                words.iter().all(|word| text.contains(word.as_str()))
            })
            .map(|command| Row {
                key: format!("command/{}", command.name()),
                section: Section::Commands,
                kind: RowKind::Command(*command),
            }),
    );
    for (ix, item) in listing.items.iter().enumerate() {
        let target = match &item.kind {
            ItemKind::Target(offer) => Some(offer.kind),
            ItemKind::Review { .. } | ItemKind::Pr { .. } => None,
            // The branch picker is the switcher's: the palette lists the reviews it opens.
            ItemKind::Start { .. }
            | ItemKind::Retry { .. }
            | ItemKind::OpenFolder
            | ItemKind::CompareBranches
            | ItemKind::Branch { .. }
            | ItemKind::Base { .. } => continue,
        };
        let (Some((location, request)), Some(repo)) = (
            listing.request(ix),
            item.repo.and_then(|repo| listing.repos.get(repo)),
        ) else {
            continue;
        };
        rows.push(Row {
            key: format!("review/{}", item.key),
            section: Section::Reviews,
            kind: RowKind::Review {
                repo: repo.name.clone(),
                label: item.label().to_string(),
                target,
                badge: item.review().cloned(),
                location,
                request,
            },
        });
    }
    if open >= Scope::Review && !words.is_empty() {
        rows.extend(
            crate::file_finder::filter(files, query)
                .into_iter()
                .take(FILE_ROWS)
                .map(|index| {
                    let file = &files[index];
                    Row {
                        key: format!("file/{}", file.path),
                        section: Section::Files,
                        kind: RowKind::File {
                            path: file.path.clone(),
                            status: file.status,
                        },
                    }
                }),
        );
    }
    rows
}

/// What was picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteEvent {
    /// Run this command.
    Run(Command),
    /// Open or resume this review.
    Open {
        location: Location,
        request: OpenRequest,
    },
    /// Show this file of the open review in the file view.
    OpenFile(String),
    /// Ask for the folder of the repository this pull request is in, then open it there.
    /// The palette stays open meanwhile.
    ChooseFolder(PrRef),
    /// Closed without picking (`Esc`).
    Dismiss,
}

/// The palette (see the module docs).
pub struct Palette {
    reviews: Entity<Reviews>,
    github: Entity<GitHub>,
    /// The repository in view, which `#n` names.
    in_view: Option<Location>,
    /// Why the folder chosen for a pull request wasn't taken.
    notice: Option<String>,
    session: Option<Entity<ReviewSession>>,
    input: Entity<InputState>,
    query: String,
    /// The open review's files ([`crate::file_finder::candidates`]), and whether the side's
    /// paths were in them.
    files: Arc<Vec<Candidate>>,
    files_complete: bool,
    rows: Vec<Row>,
    /// Into `rows`.
    selected: usize,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Palette {
    /// A palette over the known repositories' reviews and, with `session`, the open review's
    /// commands and files (its side's paths read if they aren't). `in_view` is the
    /// repository `#n` names; `github` holds the titles of the pull requests the picker
    /// listed this session.
    pub fn new(
        reviews: Entity<Reviews>,
        github: Entity<GitHub>,
        in_view: Option<Location>,
        session: Option<Entity<ReviewSession>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(PLACEHOLDER));
        let mut subscriptions = vec![
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query_changed(cx);
                }
            }),
            cx.observe(&reviews, |this, _, cx| this.rebuild(cx)),
            cx.observe(&github, |this, _, cx| this.rebuild(cx)),
        ];
        if let Some(session) = &session {
            session.update(cx, |session, cx| session.ensure_side_paths(cx));
            // The side's paths land.
            subscriptions.push(cx.observe(session, |this, _, cx| {
                if !this.files_complete {
                    this.read_files(cx);
                    this.rebuild(cx);
                }
            }));
        }
        let mut this = Self {
            reviews,
            github,
            in_view,
            notice: None,
            session,
            input,
            query: String::new(),
            files: Arc::new(Vec::new()),
            files_complete: false,
            rows: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.read_files(cx);
        this.rows = this.build(cx);
        this
    }

    /// Puts the keyboard in the filter field.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Whether the filter field has the keyboard.
    pub fn is_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The selected row's key.
    pub fn selected_key(&self) -> Option<&str> {
        self.rows.get(self.selected).map(|row| row.key.as_str())
    }

    /// The open review's files, once its side's paths have landed (the review's own files
    /// meanwhile).
    fn read_files(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let session = session.read(cx);
        let side = session.side_paths().cloned();
        let changed: Vec<(String, FileStatus)> = session
            .target_files()
            .iter()
            .map(|entry| (entry.path.clone(), entry.status))
            .collect();
        self.files_complete = side.is_some();
        self.files = Arc::new(crate::file_finder::candidates(
            side.as_deref().map_or(&[], Vec::as_slice),
            &changed,
        ));
    }

    /// Whether the palette still offers to choose the folder of `reference`'s repository:
    /// what an answer to that choice is checked against (`Shell::choose_pr_folder`).
    pub fn asks_folder_for(&self, reference: &PrRef) -> bool {
        self.rows.iter().any(|row| {
            matches!(&row.kind, RowKind::ChooseFolder { reference: asked, .. } if asked == reference)
        })
    }

    /// Why the folder chosen for a pull request wasn't taken, until something else is typed.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The folder chosen for the pull request isn't its repository's: the palette stays open
    /// and says why under the row.
    pub fn folder_refused(&mut self, why: String, cx: &mut Context<Self>) {
        self.notice = Some(why);
        cx.notify();
    }

    fn build(&self, cx: &gpui::App) -> Vec<Row> {
        let reviews = self.reviews.read(cx);
        let listing = items::build(&reviews.rows(), &self.query);
        let offered = pr_reference(&self.query).and_then(|reference| {
            offer(
                &reference,
                self.in_view.as_ref(),
                reviews,
                self.github.read(cx),
            )
        });
        let open = match &self.session {
            None => Scope::Always,
            Some(session) if session.read(cx).review().is_pr_review() => Scope::PrReview,
            Some(_) => Scope::Review,
        };
        build(&self.query, open, &listing, &self.files, offered.as_ref())
    }

    /// Lists the rows again (the reviews or the files changed), keeping the selected row when
    /// it is still there.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let rows = self.build(cx);
        if rows == self.rows {
            return;
        }
        let keep = self.selected_key().map(str::to_string);
        self.rows = rows;
        self.selected = keep
            .and_then(|key| self.rows.iter().position(|row| row.key == key))
            .unwrap_or(0);
        self.scroll_to_selected();
        cx.notify();
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().to_string();
        if query == self.query {
            return;
        }
        self.query = query;
        self.notice = None;
        self.rows = self.build(cx);
        self.selected = 0;
        self.scroll_to_selected();
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = ix.min(self.rows.len().saturating_sub(1));
        self.scroll_to_selected();
        cx.notify();
    }

    /// The list's line for row `ix`: each section's label is a line before its first row.
    fn line_of(&self, ix: usize) -> usize {
        let labels = self.rows[..=ix.min(self.rows.len().saturating_sub(1))]
            .iter()
            .enumerate()
            .filter(|(at, row)| *at == 0 || self.rows[at - 1].section != row.section)
            .count();
        ix + labels
    }

    fn scroll_to_selected(&self) {
        if !self.rows.is_empty() {
            self.scroll.scroll_to_item(self.line_of(self.selected));
        }
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.rows.len();
        if len > 0 {
            self.select((self.selected + 1) % len, cx);
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.rows.len();
        if len > 0 {
            self.select((self.selected + len - 1) % len, cx);
        }
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.pick(self.selected, cx);
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PaletteEvent::Dismiss);
    }

    /// Picks row `ix` (Enter on it, or a click).
    fn pick(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else {
            return;
        };
        self.selected = ix;
        let event = match &row.kind {
            RowKind::Pr {
                location,
                reference,
                ..
            } => PaletteEvent::Open {
                location: location.clone(),
                request: OpenRequest::Pr(reference.clone()),
            },
            RowKind::ChooseFolder { reference, .. } => {
                PaletteEvent::ChooseFolder(reference.clone())
            }
            RowKind::Command(command) => PaletteEvent::Run(*command),
            RowKind::Review {
                location, request, ..
            } => PaletteEvent::Open {
                location: location.clone(),
                request: request.clone(),
            },
            RowKind::File { path, .. } => PaletteEvent::OpenFile(path.clone()),
        };
        cx.emit(event);
    }
}
