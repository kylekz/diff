//! The PR picker (`Ctrl G`, the title bar's **PRs**; PLAN §9.9, DESIGN §4.5): a field that
//! takes a pull request's number, `#n`, `owner/repo#n` or URL, over the open pull requests of
//! the repository in view, in an overlay hung under the **PRs** button over the scrim, in the
//! switcher's look.
//!
//! **The list** is the `GitHub` service's for the repository in view (`gh pr list`, the ones
//! asking for the viewer's review first), kept for the app's session and read again in the
//! background each time the picker opens: the list read before shows meanwhile, skeleton
//! rows the first time. Typing filters it in memory (every word in a row's number, title or
//! author); nothing typed asks `gh` anything.
//!
//! **A reference** the field parses puts its own row first ([`build`]), which `Enter` opens:
//! a number or `#n` in the repository in view; `owner/repo#n` or a URL in that repository's
//! known clones, the window moving there, or, when dv knows none, through **Choose its
//! folder** ([`reference`], the palette's rule).
//!
//! **Without a list** the picker says why in its place ([`ListState`]): no repository in
//! view, one that isn't on GitHub, `gh` missing or signed out (with the command that fixes
//! it), GitHub out of reach, no open pull requests. The field works all the same: a
//! reference opens whatever the list did.
//!
//! `↑`/`↓` move the selection (wrapping), `Enter` opens, `Esc` closes. The picker only
//! reports what was picked ([`PrPickerEvent`]); the shell opens it.
//!
//! Automation targets: `pr_picker`, `pr_picker/input`, `pr_picker/repo`, `pr_picker/state`,
//! `pr_picker/stale`, `pr_picker/notice`, `pr_picker/row/<key>` for each row ([`Row::key`]).

pub mod reference;
mod render;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use dv_core::Location;
use dv_core::github::{ListedPr, PrRef};
use gpui::{
    AppContext as _, Context, Entity, EventEmitter, Focusable as _, ScrollHandle, Subscription,
    Task, Window,
};
use gpui_component::input::{InputEvent, InputState};

use crate::actions::pr_picker::{Confirm, Dismiss, SelectNext, SelectPrevious};
use crate::failure::Failure;
use crate::services::github::{GitHub, ListFailure, PrList};
use crate::services::reviews::Reviews;
use crate::services::{Locations, Reach};
use crate::thread::drawn;
use reference::{PrOffer, offer};
pub(crate) use render::notice_line;

/// The panel's width (the switcher's).
pub const PICKER_WIDTH: gpui::Pixels = gpui::px(560.);
/// The field's placeholder.
pub const PLACEHOLDER: &str = "PR number, #n, owner/repo#n or URL";
/// The line under the repository's when a read listed the open pull requests but couldn't
/// tell which of them ask for the viewer's review; why is its tooltip.
pub const UNMARKED: &str = "Couldn't read which pull requests ask for your review";
/// How many rows `state.pr_picker` reports.
pub const STATE_ROWS: usize = 60;

/// What a row opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// The pull request the field names, in a known clone of its repository.
    Reference {
        /// `#482`, `acme/frontend#482`.
        name: String,
        /// Its title, when the list read this session holds it.
        title: Option<String>,
        /// The clone's name, shown when it isn't the repository in view.
        clone: Option<String>,
        location: Location,
        reference: PrRef,
    },
    /// The pull request the field names, whose repository dv knows no clone of: its folder
    /// is to be chosen.
    ChooseFolder {
        /// `acme/frontend`.
        repository: String,
        reference: PrRef,
    },
    /// One of the repository's open pull requests: `pr` with its title and its author as
    /// the row draws them (`crate::thread::drawn`), whatever GitHub's text holds.
    /// `reference` names it in the list's own repository (`PrList::reference`).
    Listed {
        pr: ListedPr,
        location: Location,
        reference: PrRef,
    },
}

/// One pickable row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// A stable name: `open/<the clone's key>` for the field's reference, `choose_folder`, or
    /// `pr/<number>`.
    pub key: String,
    pub kind: RowKind,
}

/// What stands where the list goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListState {
    /// The rows are the list (or what of it passes the filter).
    Listed,
    /// No repository is in view.
    NoRepository,
    /// The first read of the repository's list runs.
    Loading,
    /// The list couldn't be read, and none was read before.
    Failed(ListFailure),
    /// The repository has no open pull requests.
    Empty,
    /// Nothing of the list passes what is typed.
    NoMatch,
}

impl ListState {
    /// `"listed"`, `"no_repository"`, `"loading"`, `"failed"`, `"empty"` or `"no_match"`, as
    /// automation state spells it.
    pub fn key(&self) -> &'static str {
        match self {
            Self::Listed => "listed",
            Self::NoRepository => "no_repository",
            Self::Loading => "loading",
            Self::Failed(_) => "failed",
            Self::Empty => "empty",
            Self::NoMatch => "no_match",
        }
    }
}

/// The rows for `query` (module docs): the reference it names, as `offered` resolved it,
/// then what of `list` passes it; and what stands where the list goes.
pub fn build(
    query: &str,
    in_view: Option<&Location>,
    list: Option<&PrList>,
    offered: Option<&PrOffer>,
) -> (Vec<Row>, ListState) {
    let mut rows = Vec::new();
    let reference = PrRef::parse(query);
    match offered {
        Some(PrOffer::Clones {
            reference,
            name,
            clones,
        }) => rows.extend(clones.iter().map(|clone| {
            Row {
                key: format!("open/{}", clone.key),
                kind: RowKind::Reference {
                    name: name.clone(),
                    title: clone.title.clone(),
                    clone: (Some(&clone.location) != in_view || clones.len() > 1)
                        .then(|| clone.name.clone()),
                    location: clone.location.clone(),
                    reference: reference.clone(),
                },
            }
        })),
        Some(PrOffer::Unknown {
            reference,
            repository,
        }) => rows.push(Row {
            key: "choose_folder".to_string(),
            kind: RowKind::ChooseFolder {
                repository: repository.clone(),
                reference: reference.clone(),
            },
        }),
        None => {}
    }
    let Some(location) = in_view else {
        return (rows, ListState::NoRepository);
    };
    let prs = list.and_then(|list| list.prs.as_deref());
    let listed: Vec<&ListedPr> = match (&reference, prs) {
        (_, None) => Vec::new(),
        // A reference to a repository by name has its own rows; the list is the repository
        // in view's.
        (Some(PrRef::Short { .. } | PrRef::Url { .. }), Some(_)) => Vec::new(),
        // A number filters by its digits; the pull request it names is the reference's row.
        (Some(PrRef::Number(number) | PrRef::Hash(number)), Some(prs)) => {
            let digits = number.to_string();
            let offered = !rows.is_empty();
            prs.iter()
                .filter(|pr| {
                    !(offered && pr.number == *number) && pr.number.to_string().contains(&digits)
                })
                .collect()
        }
        (None, Some(prs)) => {
            let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
            prs.iter()
                .filter(|pr| {
                    let text = format!("#{} {} {}", pr.number, pr.title, pr.author).to_lowercase();
                    words.iter().all(|word| text.contains(word.as_str()))
                })
                .collect()
        }
    };
    let state = match (list, prs) {
        (_, Some(prs)) if prs.is_empty() => ListState::Empty,
        (_, Some(_)) if listed.is_empty() && rows.is_empty() => ListState::NoMatch,
        (_, Some(_)) => ListState::Listed,
        (Some(list), None) => match &list.failure {
            Some(failure) if !list.loading => ListState::Failed(failure.clone()),
            _ => ListState::Loading,
        },
        (None, None) => ListState::Loading,
    };
    rows.extend(listed.into_iter().map(|pr| Row {
        key: format!("pr/{}", pr.number),
        kind: RowKind::Listed {
            pr: ListedPr {
                title: drawn(&pr.title),
                author: drawn(&pr.author),
                ..pr.clone()
            },
            location: location.clone(),
            reference: list.map_or(PrRef::Hash(pr.number), |list| list.reference(pr.number)),
        },
    }));
    (rows, state)
}

/// What was picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrPickerEvent {
    /// Open this pull request in the repository at `location`.
    Open {
        location: Location,
        reference: PrRef,
    },
    /// Ask for the folder of the repository this pull request is in, then open it there.
    ChooseFolder(PrRef),
    /// Closed without picking (`Esc`).
    Dismiss,
}

/// The PR picker (see the module docs).
pub struct PrPicker {
    reviews: Entity<Reviews>,
    github: Entity<GitHub>,
    /// The repository in view.
    location: Option<Location>,
    input: Entity<InputState>,
    query: String,
    rows: Vec<Row>,
    state: ListState,
    /// Into `rows`.
    selected: usize,
    scroll: ScrollHandle,
    /// Why the folder chosen for a pull request wasn't taken.
    notice: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PrPickerEvent> for PrPicker {}

impl PrPicker {
    /// A picker over the repository at `location`, whose list is read again in the
    /// background (module docs, "The list"): the repository is reached as a listing reaches
    /// it, so a stopped distro stays stopped and the picker says so.
    pub fn new(
        reviews: Entity<Reviews>,
        github: Entity<GitHub>,
        locations: &Entity<Locations>,
        location: Option<Location>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(PLACEHOLDER));
        let subscriptions = vec![
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query_changed(cx);
                }
            }),
            cx.observe(&github, |this, _, cx| this.rebuild(cx)),
            cx.observe(&reviews, |this, _, cx| this.rebuild(cx)),
        ];
        if let Some(location) = &location {
            let (locations, reached) = (locations.clone(), location.clone());
            github.update(cx, |github, cx| {
                github.refresh_list(
                    location.clone(),
                    move |cx| {
                        let reach =
                            locations.update(cx, |locations, cx| locations.reach(reached, cx));
                        let backend: Task<Result<_, Failure>> = cx.background_spawn(async move {
                            match reach.await {
                                Reach::Opened(backend) => Ok(backend),
                                Reach::Stopped { distro, .. } => {
                                    Err(Failure::new(format!("{distro} isn't running")))
                                }
                                Reach::Reconnecting => Err(Failure::new(
                                    "the connection to the repository is reconnecting",
                                )),
                                Reach::Failed { error, .. } => Err(Failure::from(&*error)),
                            }
                        });
                        backend
                    },
                    cx,
                )
            });
        }
        let mut this = Self {
            reviews,
            github,
            location,
            input,
            query: String::new(),
            rows: Vec::new(),
            state: ListState::Loading,
            selected: 0,
            scroll: ScrollHandle::new(),
            notice: None,
            _subscriptions: subscriptions,
        };
        (this.rows, this.state) = this.build(cx);
        this
    }

    /// Puts the keyboard in the field.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Whether the field has the keyboard.
    pub fn is_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// What stands where the list goes.
    pub fn list_state(&self) -> &ListState {
        &self.state
    }

    /// The repository in view.
    pub fn location(&self) -> Option<&Location> {
        self.location.as_ref()
    }

    /// The repository's list as far as it was read this session.
    pub fn list<'a>(&self, cx: &'a gpui::App) -> Option<&'a PrList> {
        self.github.read(cx).list(self.location.as_ref()?)
    }

    /// The selected row's key.
    pub fn selected_key(&self) -> Option<&str> {
        self.rows.get(self.selected).map(|row| row.key.as_str())
    }

    /// Whether the field names a pull request of another repository than the one in view:
    /// its rows are that repository's clones, or the folder to choose, so the line that
    /// names the repository in view and counts its list isn't drawn over them.
    pub fn names_another_repository(&self) -> bool {
        self.rows.iter().any(|row| match &row.kind {
            RowKind::Reference { clone, .. } => clone.is_some(),
            RowKind::ChooseFolder { .. } => true,
            RowKind::Listed { .. } => false,
        })
    }

    /// Whether the picker still offers to choose the folder of `reference`'s repository:
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

    /// The folder chosen for the pull request isn't its repository's: the picker stays open
    /// and says why under the row.
    pub fn folder_refused(&mut self, why: String, cx: &mut Context<Self>) {
        self.notice = Some(why);
        cx.notify();
    }

    fn build(&self, cx: &gpui::App) -> (Vec<Row>, ListState) {
        let github = self.github.read(cx);
        let offered = PrRef::parse(&self.query).and_then(|reference| {
            offer(
                &reference,
                self.location.as_ref(),
                self.reviews.read(cx),
                github,
            )
        });
        build(
            &self.query,
            self.location.as_ref(),
            self.location
                .as_ref()
                .and_then(|location| github.list(location)),
            offered.as_ref(),
        )
    }

    /// Lists the rows again (the list landed, a repository's GitHub name was read), keeping
    /// the selected row when it is still there.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let (rows, state) = self.build(cx);
        // The service's `loading` shows too, whatever the rows.
        cx.notify();
        if rows == self.rows && state == self.state {
            return;
        }
        let keep = self.selected_key().map(str::to_string);
        self.rows = rows;
        self.state = state;
        self.selected = keep
            .and_then(|key| self.rows.iter().position(|row| row.key == key))
            .unwrap_or(0);
        self.scroll_to_selected();
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().to_string();
        if query == self.query {
            return;
        }
        self.query = query;
        self.notice = None;
        (self.rows, self.state) = self.build(cx);
        self.selected = 0;
        self.scroll_to_selected();
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = ix.min(self.rows.len().saturating_sub(1));
        self.scroll_to_selected();
        cx.notify();
    }

    fn scroll_to_selected(&self) {
        if !self.rows.is_empty() {
            self.scroll.scroll_to_item(self.selected);
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
        cx.emit(PrPickerEvent::Dismiss);
    }

    /// Picks row `ix` (Enter on it, or a click).
    fn pick(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else {
            return;
        };
        self.selected = ix;
        let event = match &row.kind {
            RowKind::Reference {
                location,
                reference,
                ..
            } => PrPickerEvent::Open {
                location: location.clone(),
                reference: reference.clone(),
            },
            RowKind::ChooseFolder { reference, .. } => {
                PrPickerEvent::ChooseFolder(reference.clone())
            }
            RowKind::Listed {
                location,
                reference,
                ..
            } => PrPickerEvent::Open {
                location: location.clone(),
                reference: reference.clone(),
            },
        };
        cx.emit(event);
    }
}

/// The repository's name over the list: its name on GitHub once a read told it, else the
/// folder's.
pub fn repo_title(location: &Location, list: Option<&PrList>) -> String {
    match list.and_then(|list| list.repo.as_ref()) {
        Some(repo) => repo.name_with_owner(),
        None => location.short_name(),
    }
}

/// The list's pull requests, for what the picker counts.
pub fn listed(list: Option<&PrList>) -> Option<&Arc<Vec<ListedPr>>> {
    list.and_then(|list| list.prs.as_ref())
}
