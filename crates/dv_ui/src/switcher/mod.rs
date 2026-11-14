//! The switcher (PLAN §11 `switcher`, DESIGN §4.1; `Ctrl O`): every known repository with its
//! uncommitted changes, the targets it offers and its open reviews, in an overlay hung under the
//! title bar's switcher button over the scrim (DESIGN §3). Picking a row opens or resumes that
//! review; "Open folder…" adds a repository through the folder picker (`crate::system`); a
//! stopped WSL distro's **Start** row starts it, and a failed connection's **Retry** row
//! connects again, the switcher staying open while it lists the repository, which then has
//! the selection. Meanwhile the Start or Retry row stays, inert, with the selection: picking
//! it again does nothing.
//!
//! A filter field has the keyboard while it is open: typing filters ([`items::build`]), `↑`/`↓`
//! move the selection (wrapping), `Enter` picks, `Esc` closes. The switcher only reports what
//! was picked ([`SwitcherEvent`]); the shell opens it. Rows refresh as the reviews service
//! recomputes them (it does when the switcher opens, and when a connection a row goes through
//! starts reconnecting, stops or fails, or comes back), keeping the selected row.
//!
//! A repository's "Compare branches…" row turns the switcher into the branch picker
//! ([`branches`]): its branches, then the bases the picked one can be compared with, each pair
//! opening or resuming its `branch` review. There `Esc` steps back (bases → branches → the
//! repositories, when the picker was entered from them) before it closes.
//!
//! A repository's header has a menu (a right-click, or its `⋯` under the pointer) with **Remove
//! from list**, which `Ctrl Shift Backspace` does for the selected row's repository: the
//! repository leaves the known ones (`Reviews::remove_repo`), the selection moving to the next
//! one's first row. The open review's repository can't be removed (its menu item is disabled,
//! the key does nothing): the title bar's source picker lists its targets.
//!
//! A row beside an open review has a menu too (a right-click, or its `⋯` under the pointer)
//! with **Close review**, which closes that review for good (PLAN §2.4: the reviewer's
//! `closed`, as `dv review close`), the open one included; its target then offers a new
//! review. `Ctrl Shift W` asks the same of the selected row, which then reads "Close this
//! review?" until `Enter` closes it or `Esc`, or moving on, keeps it: nothing reopens a
//! closed review, so the key alone never closes one. A close that fails says why in the
//! footer.
//!
//! [`source`] is the title bar's other menu, the current repository's targets.
//!
//! Automation targets: `switcher`, `switcher/filter`, `switcher/repo/<repo>`,
//! `switcher/repo/<repo>/more` (the header's `⋯`), `switcher/menu`, `switcher/menu/remove`,
//! `switcher/menu/close`, `switcher/<key>` for each row ([`items::Item::key`]) and
//! `switcher/<key>/more` (the `⋯` of a row with a review).

pub mod branches;
pub mod items;
mod render;
pub mod source;

#[cfg(test)]
mod tests;

use dv_core::Location;
use dv_core::review::ReviewId;
use gpui::{
    AppContext as _, Context, Entity, EventEmitter, Focusable as _, ScrollHandle, Subscription,
    Window,
};
use gpui_component::input::{InputEvent, InputState};

use crate::actions::switcher::{
    CloseReview, Confirm, Dismiss, RemoveRepo, SelectNext, SelectPrevious,
};
use crate::failure::Failure;
use crate::services::reviews::{OpenRequest, Reviews};
use branches::Mode;
use items::{ItemKind, Listing};

/// The panel's width (one-off, like the board's sheets: wide enough for a path and a review).
pub const SWITCHER_WIDTH: gpui::Pixels = gpui::px(560.);
/// The filter field's placeholder.
pub const PLACEHOLDER: &str = "Filter repositories and reviews";

/// What was picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitcherEvent {
    /// Open or resume this review.
    Open {
        location: Location,
        request: OpenRequest,
    },
    /// "Open folder…".
    OpenFolder,
    /// **Start** on the repository at `location`, whose WSL distro isn't running, or **Retry**
    /// on one whose connection failed: both connect explicitly (`Reviews::start`).
    Start { location: Location },
    /// **Remove from list** on the repository at `location` (`Reviews::remove_repo`).
    Remove { location: Location },
    /// **Close review** on the open review `review` of the repository at `location`
    /// (`Reviews::close_review`).
    Close {
        location: Location,
        review: ReviewId,
    },
    /// Closed without picking (`Esc`).
    Dismiss,
}

/// What an open menu is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuTarget {
    /// A repository's header: **Remove from list**.
    Repo(Location),
    /// The row `row` (its key), beside an open review: **Close review**.
    Review { row: String },
}

/// The open menu: what for, and where it opened.
#[derive(Clone, Debug, PartialEq)]
pub struct SwitcherMenu {
    pub target: MenuTarget,
    pub position: gpui::Point<gpui::Pixels>,
}

/// The switcher (see the module docs).
pub struct Switcher {
    reviews: Entity<Reviews>,
    input: Entity<InputState>,
    query: String,
    mode: Mode,
    /// Whether `Esc` in the branch picker's first step returns to the repositories: it was
    /// entered from their "Compare branches…" row, not opened on it (the source picker's).
    back_to_repos: bool,
    listing: Listing,
    /// Into `listing.items`.
    selected: usize,
    /// The repository a **Start** or **Retry** was picked on: its first row takes the
    /// selection once it lists one.
    started: Option<Location>,
    /// The open review's repository (the one it was opened from), which can't be removed.
    in_use: Option<Location>,
    /// A repository header's menu or a review row's, while open.
    menu: Option<SwitcherMenu>,
    /// The row (its key) `Ctrl Shift W` asked to close the review of: it reads "Close this
    /// review?" while it has the selection, and `Enter` closes it.
    closing: Option<String>,
    /// Why the last close failed, for the footer, until the selection or the list changes.
    close_failure: Option<String>,
    /// The row a removal, or closing a review whose row then goes, moves the selection to
    /// once the listing drops it.
    after_remove: Option<String>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SwitcherEvent> for Switcher {}

impl Switcher {
    /// A switcher over the known repositories, its selection on `focus`'s first row (the
    /// repository `dv open` named), else on the open review `current`, else on the first row.
    /// `in_use` is the open review's repository, which it won't remove.
    pub fn new(
        reviews: Entity<Reviews>,
        focus: Option<&Location>,
        current: Option<&ReviewId>,
        in_use: Option<Location>,
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
            // The reviews service lists a row again when its connection goes down or comes
            // back, whether or not the switcher is open.
            cx.observe(&reviews, |this, _, cx| this.rebuild(cx)),
        ];
        let mut this = Self {
            reviews,
            input,
            query: String::new(),
            mode: Mode::Repos,
            back_to_repos: false,
            listing: Listing::default(),
            selected: 0,
            started: None,
            in_use,
            menu: None,
            closing: None,
            close_failure: None,
            after_remove: None,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.listing = this.build(cx);
        this.selected = focus
            .and_then(|location| this.listing.first_of(location))
            .or_else(|| current.and_then(|id| this.listing.review_row(id)))
            .unwrap_or(0);
        this.scroll_to_selected();
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

    pub fn listing(&self) -> &Listing {
        &self.listing
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    /// Opens the branch picker on the repository at `location` (the source picker's "Compare
    /// branches…"): `Esc` in its first step then closes the switcher.
    pub fn compare_branches(
        &mut self,
        location: Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.back_to_repos = false;
        self.set_mode(Mode::Branches { location }, None, window, cx);
    }

    /// Shows `mode`'s rows with the filter cleared, the selection on `select` (a row's key)
    /// when it is listed, else on the first row.
    fn set_mode(
        &mut self,
        mode: Mode,
        select: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = mode;
        self.menu = None;
        self.closing = None;
        self.close_failure = None;
        self.query.clear();
        let placeholder = self.mode.placeholder();
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.set_placeholder(placeholder, window, cx);
        });
        self.listing = self.build(cx);
        self.selected = select
            .and_then(|key| self.listing.position(&key))
            .unwrap_or(0);
        self.scroll_to_selected();
        cx.notify();
    }

    /// The selected row's key.
    pub fn selected_key(&self) -> Option<&str> {
        self.listing
            .items
            .get(self.selected)
            .map(|item| item.key.as_str())
    }

    /// The open menu.
    pub fn menu(&self) -> Option<&SwitcherMenu> {
        self.menu.as_ref()
    }

    /// Whether the menu of the repository at `location` is open.
    fn repo_menu_is(&self, location: &Location) -> bool {
        self.menu
            .as_ref()
            .is_some_and(|menu| menu.target == MenuTarget::Repo(location.clone()))
    }

    /// Whether the menu of the row `key` is open.
    fn row_menu_is(&self, key: &str) -> bool {
        self.menu
            .as_ref()
            .is_some_and(|menu| matches!(&menu.target, MenuTarget::Review { row } if row == key))
    }

    /// Whether the repository at `location` can be removed: it isn't the open review's.
    pub fn can_remove(&self, location: &Location) -> bool {
        self.in_use.as_ref() != Some(location)
    }

    /// The repository `Ctrl Shift Backspace` would remove: the selected row's, when it can be.
    /// Only the repositories' list removes: the branch picker's header is the repository it
    /// compares in.
    pub fn removable(&self) -> Option<&Location> {
        if self.mode != Mode::Repos {
            return None;
        }
        self.listing
            .location_of(self.selected)
            .filter(|location| self.can_remove(location))
    }

    /// Opens the header menu of the repository at `location`, at `position`.
    pub fn open_menu(
        &mut self,
        location: Location,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.closing = None;
        self.menu = Some(SwitcherMenu {
            target: MenuTarget::Repo(location),
            position,
        });
        cx.notify();
    }

    /// Opens the menu of the row `key`, which stands beside an open review, at `position`;
    /// the row takes the selection.
    pub fn open_row_menu(
        &mut self,
        key: &str,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self
            .listing
            .position(key)
            .filter(|ix| self.listing.items[*ix].review().is_some())
        else {
            return;
        };
        self.select(ix, cx);
        self.closing = None;
        self.menu = Some(SwitcherMenu {
            target: MenuTarget::Review {
                row: key.to_string(),
            },
            position,
        });
    }

    /// The row `Ctrl Shift W` asked to close the review of, while it still has the selection
    /// and a review.
    pub fn closing(&self) -> Option<&str> {
        let key = self.closing.as_deref()?;
        let selected = self.listing.items.get(self.selected)?;
        (selected.key == key && selected.review().is_some()).then_some(key)
    }

    /// Whether the selected row stands beside an open review, which `Ctrl Shift W` asks to
    /// close.
    pub fn closable(&self) -> bool {
        self.listing
            .items
            .get(self.selected)
            .is_some_and(|item| item.review().is_some())
    }

    /// Why the last close failed.
    pub fn close_failure(&self) -> Option<&str> {
        self.close_failure.as_deref()
    }

    /// Closes the open review the row `key` stands beside (module docs): the menu's **Close
    /// review**, and `Enter` on a row that asked. A row that goes with its review (another
    /// target's review) hands the selection to the row after it, else the one before.
    pub fn close_review(&mut self, key: &str, cx: &mut Context<Self>) {
        self.menu = None;
        self.closing = None;
        self.close_failure = None;
        cx.notify();
        let Some(ix) = self.listing.position(key) else {
            return;
        };
        let (Some(badge), Some(location)) = (
            self.listing.items[ix].review(),
            self.listing.location_of(ix),
        ) else {
            return;
        };
        if matches!(self.listing.items[ix].kind, ItemKind::Review { .. }) {
            self.after_remove = self
                .listing
                .items
                .get(ix + 1)
                .or_else(|| ix.checked_sub(1).and_then(|ix| self.listing.items.get(ix)))
                .map(|item| item.key.clone());
        }
        cx.emit(SwitcherEvent::Close {
            location: location.clone(),
            review: badge.id.clone(),
        });
    }

    /// A close the shell ran failed: the footer says why.
    pub fn close_failed(&mut self, failure: &Failure, cx: &mut Context<Self>) {
        self.close_failure = Some(failure.message.clone());
        cx.notify();
    }

    /// `Ctrl Shift W`: asks whether to close the selected row's review (module docs).
    fn ask_close(&mut self, _: &CloseReview, _: &mut Window, cx: &mut Context<Self>) {
        if self.closable() {
            self.menu = None;
            self.close_failure = None;
            self.closing = self.selected_key().map(str::to_string);
            cx.notify();
        }
    }

    pub fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    /// Removes the repository at `location` from the list, when it can be (module docs).
    pub fn remove(&mut self, location: &Location, cx: &mut Context<Self>) {
        self.menu = None;
        if self.mode != Mode::Repos || !self.can_remove(location) {
            cx.notify();
            return;
        }
        self.after_remove = self
            .listing
            .row_after_removing(location)
            .map(str::to_string);
        cx.emit(SwitcherEvent::Remove {
            location: location.clone(),
        });
        cx.notify();
    }

    fn remove_selected(&mut self, _: &RemoveRepo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(location) = self.removable().cloned() {
            self.remove(&location, cx);
        }
    }

    /// Selects the row `key`; `false` when it isn't listed.
    pub fn select_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        match self.listing.position(key) {
            Some(ix) => {
                self.select(ix, cx);
                true
            }
            None => false,
        }
    }

    fn build(&self, cx: &gpui::App) -> Listing {
        let reviews = self.reviews.read(cx);
        let rows = reviews.rows();
        match &self.mode {
            Mode::Repos => items::build(&rows, &self.query),
            Mode::Branches { location } => branches::build_branches(&rows, location, &self.query),
            Mode::Bases { location, branch } => {
                branches::build_bases(&rows, location, branch, &self.query)
            }
        }
    }

    /// Lists the rows again (the reviews changed, or the filter), keeping the selected row
    /// when it is still there; a new filter selects its first row.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let keep = self.selected_key().map(str::to_string);
        let listing = self.build(cx);
        if listing == self.listing {
            return;
        }
        self.listing = listing;
        // While it starts its repository lists its Start or Retry alone, inert, which keeps
        // the selection; then its first row has it: a target once it lists, or Start or Retry
        // again when the start failed.
        let started = self
            .started
            .as_ref()
            .and_then(|location| self.listing.first_ready_of(location));
        if started.is_some() {
            self.started = None;
        }
        // A row's menu goes with its review (closed from elsewhere meanwhile).
        if let Some(MenuTarget::Review { row }) = self.menu.as_ref().map(|menu| &menu.target)
            && !self
                .listing
                .position(row)
                .is_some_and(|ix| self.listing.items[ix].review().is_some())
        {
            self.menu = None;
        }
        let removed_to = self
            .after_remove
            .take()
            .and_then(|key| self.listing.position(&key));
        self.selected = started
            .or(removed_to)
            .or_else(|| keep.and_then(|key| self.listing.position(&key)))
            .unwrap_or(0);
        // A row that asked to close its review and lost the selection, or the review, asks
        // no more: `Esc` then closes the switcher at once.
        if self.closing().is_none() {
            self.closing = None;
        }
        self.scroll_to_selected();
        cx.notify();
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().to_string();
        if query == self.query {
            return;
        }
        self.closing = None;
        self.close_failure = None;
        self.query = query;
        self.listing = self.build(cx);
        self.selected = 0;
        self.scroll_to_selected();
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let ix = ix.min(self.listing.items.len().saturating_sub(1));
        if ix != self.selected {
            self.closing = None;
            self.close_failure = None;
        }
        self.selected = ix;
        self.scroll_to_selected();
        cx.notify();
    }

    fn scroll_to_selected(&self) {
        if let Some(entry) = self.listing.entry_of(self.selected) {
            self.scroll.scroll_to_item(entry);
        }
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.listing.items.len();
        if len > 0 {
            self.select((self.selected + 1) % len, cx);
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.listing.items.len();
        if len > 0 {
            self.select((self.selected + len - 1) % len, cx);
        }
    }

    /// `Enter`: closes the review of a row that asked ([`Switcher::closing`]), else picks
    /// the selected row.
    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        match self.closing().map(str::to_string) {
            Some(key) => self.close_review(&key, cx),
            None => self.pick(self.selected, window, cx),
        }
    }

    /// `Esc`: closes the open menu, else keeps the review a row asked to close, else a step
    /// back in the branch picker (module docs), else closes.
    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() || self.closing.take().is_some() {
            cx.notify();
            return;
        }
        let repo_key = self.listing.repos.first().map(|repo| repo.key.clone());
        match self.mode.clone() {
            Mode::Bases { location, branch } => {
                let select = repo_key.map(|key| format!("{key}/pick/{branch}"));
                self.set_mode(Mode::Branches { location }, select, window, cx);
            }
            Mode::Branches { .. } if self.back_to_repos => {
                let select = repo_key.map(|key| format!("{key}/compare"));
                self.set_mode(Mode::Repos, select, window, cx);
            }
            Mode::Branches { .. } | Mode::Repos => cx.emit(SwitcherEvent::Dismiss),
        }
    }

    /// Picks row `ix` (Enter on it, or a click); a start that runs already does nothing.
    fn pick(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.listing.items.get(ix) else {
            return;
        };
        match &item.kind {
            ItemKind::CompareBranches => {
                if let Some(location) = self.listing.location_of(ix).cloned() {
                    self.back_to_repos = true;
                    self.set_mode(Mode::Branches { location }, None, window, cx);
                }
                return;
            }
            ItemKind::Branch { name, .. } => {
                let branch = name.clone();
                if let Some(location) = self.listing.location_of(ix).cloned() {
                    self.set_mode(Mode::Bases { location, branch }, None, window, cx);
                }
                return;
            }
            _ => {}
        }
        self.closing = None;
        self.close_failure = None;
        self.selected = ix;
        if item.is_starting() {
            cx.notify();
            return;
        }
        if let Some(location) = self.listing.start(ix) {
            self.started = Some(location.clone());
            cx.emit(SwitcherEvent::Start { location });
            return;
        }
        match self.listing.request(ix) {
            Some((location, request)) => cx.emit(SwitcherEvent::Open { location, request }),
            None if item.kind == items::ItemKind::OpenFolder => cx.emit(SwitcherEvent::OpenFolder),
            None => {}
        }
    }
}
