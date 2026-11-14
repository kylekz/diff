//! The root view (PLAN §11 `shell`, DESIGN §2): the title bar, the panel, the main area and the
//! status bar, over the window material (DESIGN §3).
//!
//! The shell keeps its window in step with [`crate::appearance`]: it tracks it with
//! `gpui_appearance`, which applies the resolved material and light/dark appearance to the
//! window, reports the OS's light/dark changes and re-probes the platform when the window is
//! activated; and it repaints whenever [`crate::theme::install`] puts a new [`DvTheme`] in
//! place.
//!
//! **Reviews.** The shell owns the open review's [`ReviewSession`] and hands it to the panel's
//! [`FileList`] and [`CommentsList`] and to the [`DiffPane`]; opening another review swaps it
//! ([`Shell::open`], which the switcher, the source picker and automation's `open_review` all
//! go through). Opening runs
//! in the background; until it lands the main area shows that it is opening, and a failure shows
//! as a banner at the top of the main area with Retry (DESIGN §4.4). A pick whose connection is
//! the trouble waits for it as the pending pick ([`pending`]): it runs again by itself once the
//! connection is back and the open review holds no unsaved text (an open sheet, a composer's
//! draft), and the main area raises nothing for it while the open review's backend-down banner
//! and status bar report that connection. With no review open the main area shows "Pick a repo
//! to start a review"; a review with nothing to show says so.
//!
//! **Keyboard.** It rests on the main area, which always renders with the diff pane's focus
//! handle and key context inside the shell's, so `Ctrl O` and `Ctrl B` work at once and the
//! stream's keys whenever a review shows (DESIGN §5). The chrome around it takes no focus, so a
//! click on the title bar leaves the keyboard where it was. `Ctrl C` / `Ctrl X` in any text
//! field land here (`actions::field`, ahead of the field's own) and go through the clipboard
//! seam ([`crate::system::write_clipboard`]), so an automation run never touches the clipboard.
//!
//! **Overlays.** The switcher (`Ctrl O`) and the source picker are menus that hang under their
//! title-bar buttons over the scrim (DESIGN §3); the "Finish round" sheet (`Ctrl Enter`, DESIGN
//! §4.2) is centred over it. One menu is open at a time. The sheet holds the reviewer's unsaved
//! summary, so a menu opens over it rather than in its place (the scrim then covers the sheet
//! too), and closing the menu returns to it; the sheet closes only by its own Cancel or `Esc`, a
//! click on the scrim while it is the top overlay, finishing its round, or its review going
//! away, and never while its round is being finished. Each overlay takes the keyboard while open
//! and gives it back where it was when it closes (the main area, when that place is gone).
//!
//! **Focus.** Should the keyboard ever rest on an element that is no longer drawn (a hidden
//! panel's filter, an overlay's field), gpui would dispatch keys from the window's root, above
//! the shell's key context, and every key would go nowhere; the shell's focus-lost listener
//! hands it to the nearest drawn ancestor that takes the keyboard, else the main area.
//!
//! **A remote repository's connection** (PLAN §5.2, DESIGN §4.4). Once the connection to the
//! open review's `dv backend --stdio` has failed for good, the main area's top shows "Lost the
//! connection to dv in Ubuntu" with Retry (`Connection::retry`, off the UI thread), the error
//! and the connection's diagnostics in Details, and Copy diagnostics; the status bar says
//! "disconnected". Once it found its distro stopped (the user shut it down), the banner reads
//! "Ubuntu stopped" and offers **Start Ubuntu**, the same explicit start, and the status bar
//! says "stopped". The review's own refresh banner stays down meanwhile: the connection says
//! all there is. While it only reconnects there is no banner, the status bar saying
//! "reconnecting…".
//!
//! A finished review shows "Approved · this review is finished" at the top of the main area,
//! with "Start a new review"; a `ready` one the agent's handoff, "claude-code marked round 2
//! ready" with the first line of its note and "Show note" for the rest; an interdiff whose
//! round snapshot went missing why it shows all changes instead (DESIGN §4.4). An empty
//! interdiff says "No changes since round n" and offers "Show all changes".
//!
//! **Panel.** `Ctrl B` shows or hides it; dragging its right edge resizes it within 220–420,
//! saved in `settings.json` (DESIGN §2).

mod file_view;
mod frame;
pub mod intel;
mod palette;
pub mod pending;
mod render;
mod setup;
mod status_bar;
pub mod title_bar;

#[cfg(test)]
mod tests;

use anyhow::{Context as _, Result};
use dv_backend::ConnectionState;
use dv_core::review::{Author, ReviewId, ReviewStatus, ThreadState};
use dv_core::{Location, Target};
use futures::channel::oneshot;
use gpui::{
    App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable, ScrollHandle, Size,
    Subscription, Task, TitlebarOptions, Window, WindowBounds, WindowDecorations, WindowHandle,
    WindowOptions, point, px, size,
};
use gpui_component::Root;

use crate::actions::field;
use crate::actions::shell::{FinishReview, TogglePanel, ToggleSwitcher};
use crate::comments_list::CommentsList;
use crate::diff_pane::{DiffPane, DiffPaneEvent};
use crate::failure::Failure;
use crate::file_finder::FileFinder;
use crate::file_list::{FileList, FileListEvent};
use crate::history::History;
use crate::palette::Palette;
use crate::services::ActiveServices as _;
use crate::services::LocationsEvent;
use crate::services::reviews::OpenRequest;
use crate::session::{Comparison, ReviewSession, SessionEvent};
use crate::submit::{FinishSheet, SheetEvent};
use crate::switcher::source::{SourcePicker, SourcePickerEvent};
use crate::switcher::{Switcher, SwitcherEvent};
use crate::theme::{ActiveTheme as _, DvTheme, Material};
use gpui_appearance::Backing;

pub use pending::PendingPick;

/// What the main area shows when there is nothing to review (DESIGN §4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmptyState {
    /// No review is open: "Pick a repo to start a review".
    NoReview,
    /// A `working` review with no changes: "Working tree clean — nothing to review".
    WorkingTreeClean,
    /// Any other review whose target has no changes.
    NoChanges,
    /// An empty interdiff: "No changes since round n", with **Show all changes**.
    NoChangesSince(u32),
}

impl EmptyState {
    /// What `session` has to say for want of changes, whatever else the stream shows: its file
    /// list has landed, without an error, and lists nothing (DESIGN §4.4). A refresh of an
    /// empty list keeps saying so until it lands.
    pub fn of_session(session: &ReviewSession) -> Option<Self> {
        let empty = session.spec().is_some()
            && !session.is_first_load()
            && session.error().is_none()
            && session.files().is_empty();
        empty.then(|| match (session.comparison(), &session.review().target) {
            (Comparison::SinceRound(n), _) => Self::NoChangesSince(n),
            (Comparison::All, Target::Working) => Self::WorkingTreeClean,
            (Comparison::All, _) => Self::NoChanges,
        })
    }

    /// Its heading (DESIGN §4.4).
    pub fn heading(self) -> String {
        match self {
            Self::NoReview => "Pick a repo to start a review".to_string(),
            Self::WorkingTreeClean => "Working tree clean — nothing to review".to_string(),
            Self::NoChanges => "No changes — nothing to review".to_string(),
            Self::NoChangesSince(n) => format!("No changes since round {n}"),
        }
    }

    /// The line under the heading (none for [`EmptyState::NoReview`], whose line has a
    /// keycap and a command in it).
    pub fn note(self) -> &'static str {
        match self {
            Self::NoReview => "",
            Self::WorkingTreeClean => "The review picks up changes as soon as files change.",
            Self::NoChanges => "The two sides of this review are the same.",
            Self::NoChangesSince(_) => "Nothing changed after the round was handed over.",
        }
    }

    /// `"no_review"`, `"working_tree_clean"`, `"no_changes"` or `"no_changes_since"`, as
    /// automation state spells it.
    pub fn key(self) -> &'static str {
        match self {
            Self::NoReview => "no_review",
            Self::WorkingTreeClean => "working_tree_clean",
            Self::NoChanges => "no_changes",
            Self::NoChangesSince(_) => "no_changes_since",
        }
    }
}

/// The agent's handoff while the review is `ready` (DESIGN §4.4): who marked which round ready,
/// with the note they left (`dv review done --note`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyNote {
    /// The agent who set `ready`, when the event says.
    pub author: Option<Author>,
    /// The round in progress, which they handed back.
    pub round: u32,
    /// Markdown, `None` without one.
    pub note: Option<String>,
    /// The annotations the agent wrote in that round that nobody has asked about (PLAN §19).
    pub annotations: usize,
}

/// A review being opened.
#[derive(Clone, Debug)]
pub struct Opening {
    pub location: Location,
    pub request: OpenRequest,
    generation: u64,
    /// How many times this pick has run again by itself ([`pending`]); 0 for a pick.
    runs: u32,
}

/// The backend-down banner ([`Shell::backend_down`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendDown {
    pub failure: Failure,
    /// Its action's label: "Retry", or "Start Ubuntu" for a stopped distro; either is the
    /// connection's explicit start ([`Shell::retry_backend`]).
    pub action: String,
    /// The connection found its host stopped (the action starts it).
    pub stopped: bool,
}

/// A failure the main area's banner shows, and how to retry it.
#[derive(Clone, Debug)]
pub struct MainBanner {
    pub failure: Failure,
    /// The open that failed, which Retry runs again.
    pub retry: Option<(Location, OpenRequest)>,
    /// Whether its detail is disclosed.
    pub expanded: bool,
    /// It reports the pending pick ([`pending`]), which its Dismiss drops too.
    pub pick: bool,
}

/// What [`Shell::open_repo`] shows when the location doesn't open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IfNotOpened {
    /// Why, in the main area's banner (DESIGN §4.4).
    Banner,
    /// Nothing: a guess that didn't pan out (bare `dv` outside any repository).
    Ignore,
}

/// What [`Shell::open`] opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub review: ReviewId,
    /// A new review rather than a resumed one.
    pub created: bool,
}

/// An overlay while it is open.
struct Overlay<V> {
    view: Entity<V>,
    /// Where the keyboard goes back to when it closes.
    return_focus: Option<FocusHandle>,
    _subscription: Subscription,
}

/// The root view of the main window.
pub struct Shell {
    /// Keeps the window's material and light/dark in step (module docs); what it applied is
    /// `state.shell`'s.
    appearance: gpui_appearance::WindowTracker,
    /// The open review, if any.
    session: Option<Entity<ReviewSession>>,
    file_list: Entity<FileList>,
    comments_list: Entity<CommentsList>,
    diff_pane: Entity<DiffPane>,
    /// The file view's pane (`file_view`), shown over the stream while `file_view_open`.
    file_view: Entity<DiffPane>,
    file_view_open: bool,
    /// The positions jumps left (`file_view`, PLAN §18.1).
    history: History,
    /// The open review's code intelligence (`intel`, PLAN §18.6).
    intel: Option<Entity<crate::intel::CodeIntel>>,
    /// The panel's References tab and which tab shows (`intel`).
    references_panel: Entity<crate::intel::references::ReferencesPanel>,
    panel_tab: intel::PanelTab,
    /// The status bar item whose details are open (`intel`).
    server_details: Option<intel::ServerDetails>,
    /// Where the reviewer was when the last go to definition was asked for (`intel`).
    definition_from: Option<Option<crate::history::Position>>,
    /// A place waiting for its listed file's diff (`intel`).
    pending_goto: Option<intel::PendingGoTo>,
    _intel_subscriptions: Vec<Subscription>,
    /// The file finder (`Ctrl P`), a menu like the switcher.
    file_finder: Option<Overlay<FileFinder>>,
    palette: Option<Overlay<Palette>>,
    switcher: Option<Overlay<Switcher>>,
    source_picker: Option<Overlay<SourcePicker>>,
    /// The "Finish round" sheet (DESIGN §4.2).
    sheet: Option<Overlay<FinishSheet>>,
    /// Settings (`Ctrl ,`, `setup`), which opens and closes like a menu.
    settings: Option<Overlay<crate::setup::settings::SettingsView>>,
    /// The first-run screen's keyboard (`setup`).
    first_run_focus: FocusHandle,
    /// Whether the first-run screen showed when the setup service last changed.
    first_run_shown: bool,
    panel_visible: bool,
    opening: Option<Opening>,
    /// A pick that waits for its connection ([`pending`]).
    pending: Option<PendingPick>,
    /// Bumped by every open: an open that lands after a newer one started is dropped.
    open_generation: u64,
    /// Folder-picker and [`Shell::open_repo`] flows still running.
    adding: usize,
    banner: Option<MainBanner>,
    /// Whether the session's error banner shows its detail.
    session_banner_expanded: bool,
    /// Whether the backend-down banner shows its detail.
    backend_banner_expanded: bool,
    /// Whether the ready banner shows the agent's whole note.
    ready_note_open: bool,
    /// The handoff the banner's disclosure state belongs to ([`handoff`]): a new one starts
    /// closed and is measured afresh.
    ready_note_for: Option<u64>,
    /// The open note's scroll position, for its fade.
    ready_note_scroll: ScrollHandle,
    /// Whether the open note has been drawn once since it opened, so its scroll extent is
    /// known.
    ready_note_measured: bool,
    _session_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let diff_pane = cx.new(DiffPane::new);
        let file_view = cx.new(DiffPane::new_file_view);
        let file_list = cx.new(|cx| FileList::new(diff_pane.clone(), window, cx));
        let comments_list = cx.new(|cx| CommentsList::new(diff_pane.clone(), cx));
        let references_panel = cx.new(|_| crate::intel::references::ReferencesPanel::new());
        let settings = cx.services().settings.clone();
        let locations = cx.services().locations.clone();
        let setup = cx.services().setup.clone();
        let updates = cx.services().updates.clone();
        let subscriptions = vec![
            // The first-run screen, the setup cards' state and the status bar's notice.
            cx.observe_in(&setup, window, |this, _, window, cx| {
                this.setup_changed(window, cx)
            }),
            // A connection's state shows in the status bar while a review opens.
            cx.observe(&locations, |_, _, cx| cx.notify()),
            // A connection back runs the pick that waits for it ([`pending`]).
            cx.subscribe_in(
                &locations,
                window,
                |this, _, event: &LocationsEvent, window, cx| match event {
                    LocationsEvent::ConnectionChanged { key, up: true } => {
                        this.connection_back(key, window, cx)
                    }
                    LocationsEvent::ConnectionChanged { up: false, .. } => {}
                },
            ),
            cx.observe_global_in::<DvTheme>(window, |_, _, cx| cx.notify()),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    // `settings.json`'s `lsp` edited by hand meanwhile, a review open or not.
                    crate::intel::reload_lsp_settings(cx);
                    if let Some(session) = &this.session {
                        session.update(cx, |session, cx| session.window_activated(cx));
                    }
                    if let Some(intel) = this.intel().cloned() {
                        intel.update(cx, |intel, cx| intel.window_activated(cx));
                    }
                }
            }),
            // The panel's width lives in the settings.
            cx.observe(&settings, |_, _, cx| cx.notify()),
            // The status bar's Restart to update (PLAN D9).
            cx.observe(&updates, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &file_list,
                window,
                |this, _, event, window, cx| match event {
                    FileListEvent::Done => this.main_focus(cx).focus(window, cx),
                },
            ),
            // A composer gone, its draft with it, runs a pick that waited for it ([`pending`]).
            cx.subscribe_in(
                &diff_pane,
                window,
                |this, _, event: &DiffPaneEvent, window, cx| {
                    this.on_stream_event(event, window, cx)
                },
            ),
            cx.subscribe_in(
                &file_view,
                window,
                |this, _, event: &DiffPaneEvent, window, cx| {
                    this.on_file_view_event(event, window, cx)
                },
            ),
            // The keyboard never stays on an element that is no longer drawn (module docs).
            cx.on_focus_lost(window, |this, window, cx| {
                let target = window
                    .focus_lost_restore_target(cx)
                    .unwrap_or_else(|| this.main_focus(cx));
                target.focus(window, cx);
            }),
        ];
        // gpui opened the window light or dark as the OS is; dv's mode may say otherwise.
        let appearance = gpui_appearance::track_window(Backing::Solid, window, cx);
        // The main area always renders, inside the shell's key context, so the shell's keys
        // (`Ctrl O`) work at once and the stream's as soon as a review shows.
        diff_pane.read(cx).focus_handle(cx).focus(window, cx);
        // The first-run screen takes it while it shows (`setup`).
        let first_run_focus = cx.focus_handle();
        let first_run_shown = setup.read(cx).first_run();
        if first_run_shown {
            first_run_focus.focus(window, cx);
        }
        Self {
            appearance,
            session: None,
            file_list,
            comments_list,
            diff_pane,
            file_view,
            file_view_open: false,
            history: History::default(),
            intel: None,
            references_panel,
            panel_tab: intel::PanelTab::default(),
            server_details: None,
            definition_from: None,
            pending_goto: None,
            _intel_subscriptions: Vec::new(),
            file_finder: None,
            palette: None,
            switcher: None,
            source_picker: None,
            sheet: None,
            settings: None,
            first_run_focus,
            first_run_shown,
            panel_visible: true,
            opening: None,
            pending: None,
            open_generation: 0,
            adding: 0,
            banner: None,
            session_banner_expanded: false,
            backend_banner_expanded: false,
            ready_note_open: false,
            ready_note_for: None,
            ready_note_scroll: ScrollHandle::new(),
            ready_note_measured: false,
            _session_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    /// The open review, if any.
    pub fn session(&self) -> Option<&Entity<ReviewSession>> {
        self.session.as_ref()
    }

    pub fn file_list(&self) -> &Entity<FileList> {
        &self.file_list
    }

    pub fn diff_pane(&self) -> &Entity<DiffPane> {
        &self.diff_pane
    }

    pub fn switcher(&self) -> Option<&Entity<Switcher>> {
        self.switcher.as_ref().map(|overlay| &overlay.view)
    }

    pub fn source_picker(&self) -> Option<&Entity<SourcePicker>> {
        self.source_picker.as_ref().map(|overlay| &overlay.view)
    }

    pub fn comments_list(&self) -> &Entity<CommentsList> {
        &self.comments_list
    }

    /// The "Finish round" sheet, while it is open.
    pub fn sheet(&self) -> Option<&Entity<FinishSheet>> {
        self.sheet.as_ref().map(|overlay| &overlay.view)
    }

    /// The review being opened, if any.
    pub fn opening(&self) -> Option<&Opening> {
        self.opening.as_ref()
    }

    /// The main area's banner (a review that didn't open), if any.
    pub fn banner(&self) -> Option<&MainBanner> {
        self.banner.as_ref()
    }

    /// The backend-down banner (module docs): what it says once the open review's connection
    /// has failed, or found its distro stopped, with why and the connection's diagnostics as
    /// its detail, and the label of its action (Retry, or Start Ubuntu).
    pub fn backend_down(&self, cx: &App) -> Option<BackendDown> {
        let session = self.session.as_ref()?.read(cx);
        let connection = session.connection()?;
        let name = connection.name();
        let diagnostics = connection.diagnostics();
        let diagnostics = diagnostics.trim_end();
        match session.connection_state()? {
            ConnectionState::Failed { error } => Some(BackendDown {
                failure: Failure::with_detail(
                    format!("Lost the connection to dv in {name}"),
                    format!("{error}\n\n{diagnostics}"),
                ),
                action: "Retry".to_string(),
                stopped: false,
            }),
            ConnectionState::Stopped => Some(BackendDown {
                failure: Failure::with_detail(
                    format!("{name} stopped"),
                    format!(
                        "{name} was shut down, so dv stopped too; dv starts it again only when \
                         asked.\n\n{diagnostics}"
                    ),
                ),
                action: format!("Start {name}"),
                stopped: true,
            }),
            _ => None,
        }
    }

    /// The backend-down banner's Retry or Start: the connection's explicit start. The banner
    /// goes while it tries; one that comes back starts with its detail closed.
    pub fn retry_backend(&mut self, cx: &mut Context<Self>) {
        self.backend_banner_expanded = false;
        if let Some(session) = &self.session {
            session.update(cx, |session, cx| session.retry_connection(cx));
        }
    }

    /// Shows `session`'s review, replacing (and so closing) the one open before.
    pub fn open_session(
        &mut self,
        session: Entity<ReviewSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The row the review belongs to in the switcher: the repository it was opened from.
        let location = session.read(cx).origin().clone();
        self._session_subscriptions = vec![
            cx.observe(&session, |_, _, cx| cx.notify()),
            // A go to definition waiting for a file's diff goes on once it lands (`intel`).
            cx.subscribe_in(
                &session,
                window,
                |this, _, event: &SessionEvent, window, cx| {
                    if let SessionEvent::FileLoaded(path) = event {
                        this.file_loaded(path, window, cx);
                    }
                },
            ),
            cx.subscribe(&session, move |this, session, event: &SessionEvent, cx| {
                if !matches!(event, SessionEvent::ReviewChanged) {
                    return;
                }
                // A review changing under an open switcher shows in its row at once.
                if this.switcher.is_some() {
                    let reviews = cx.services().reviews.clone();
                    reviews.update(cx, |reviews, cx| reviews.refresh(location.clone(), cx));
                }
                let handoff = handoff(session.read(cx).review());
                if handoff != this.ready_note_for {
                    this.reset_ready_note(handoff);
                    cx.notify();
                }
            }),
        ];
        self.session = Some(session.clone());
        self.session_banner_expanded = false;
        self.backend_banner_expanded = false;
        self.reset_ready_note(handoff(session.read(cx).review()));
        // A sheet over the review that was open goes with it.
        self.sheet = None;
        self.file_list
            .update(cx, |list, cx| list.set_session(Some(session.clone()), cx));
        self.comments_list
            .update(cx, |list, cx| list.set_session(Some(session.clone()), cx));
        // Another review's files: the file view and the history go.
        self.file_view_open = false;
        self.file_finder = None;
        self.history.clear();
        self.file_view
            .update(cx, |pane, cx| pane.set_session(Some(session.clone()), cx));
        self.diff_pane
            .update(cx, |pane, cx| pane.set_session(Some(session.clone()), cx));
        // Its code intelligence, made before the one it replaces goes, so a review on the same
        // worktree keeps its servers (`intel`).
        self.start_intel(&session, window, cx);
        cx.notify();
    }

    /// Whether the panel column shows (`Ctrl B` toggles it, DESIGN §2).
    pub fn panel_visible(&self) -> bool {
        self.panel_visible
    }

    pub fn set_panel_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.panel_visible != visible {
            self.panel_visible = visible;
            if !visible {
                self.close_comments_menu(cx);
            }
            cx.notify();
        }
    }

    /// Closes the comments list's row menu, if it is open: an overlay opening (the switcher,
    /// the source picker, the sheet) or the panel hiding takes it away.
    pub(super) fn close_comments_menu(&mut self, cx: &mut Context<Self>) {
        self.comments_list
            .update(cx, |list, cx| list.close_menu(cx));
    }

    /// The empty state the main area shows, if any.
    pub fn empty_state(&self, cx: &App) -> Option<EmptyState> {
        let Some(session) = &self.session else {
            return self.opening.is_none().then_some(EmptyState::NoReview);
        };
        let session = session.read(cx);
        // Threads whose files left the review keep the stream: its "Not in the diff" section
        // shows them, with the empty state's words and action as a row under it (DESIGN §4.1).
        if session.threads_not_in_diff().next().is_some() {
            return None;
        }
        EmptyState::of_session(session)
    }

    /// The agent's handoff the main area shows while the review is `ready` (DESIGN §4.4).
    pub fn ready_note(&self, cx: &App) -> Option<ReadyNote> {
        let review = self.session.as_ref()?.read(cx).review();
        if review.status != ReviewStatus::Ready {
            return None;
        }
        let change = review
            .status_change
            .as_ref()
            .filter(|change| change.status == ReviewStatus::Ready);
        let round = review.current_round();
        Some(ReadyNote {
            author: change.and_then(|change| change.author.clone()),
            round,
            note: change
                .and_then(|change| change.note.clone())
                .filter(|note| !note.trim().is_empty()),
            annotations: review
                .live_threads()
                .filter(|thread| thread.state == ThreadState::Annotation)
                .filter(|thread| thread.first_anchor().round == round)
                .count(),
        })
    }

    /// Whether the ready banner shows the whole note ("Show note").
    pub fn ready_note_open(&self) -> bool {
        self.ready_note_open
    }

    /// The ready banner's disclosure, closed, for `handoff`.
    fn reset_ready_note(&mut self, handoff: Option<u64>) {
        self.ready_note_for = handoff;
        self.ready_note_open = false;
        self.ready_note_measured = false;
        self.ready_note_scroll = ScrollHandle::new();
    }

    /// "Show note" / "Hide note" on the ready banner.
    pub fn toggle_ready_note(&mut self, cx: &mut Context<Self>) {
        self.ready_note_open = !self.ready_note_open;
        self.ready_note_measured = false;
        self.ready_note_scroll = ScrollHandle::new();
        cx.notify();
    }

    /// "Show all changes" on an empty interdiff.
    pub fn show_all_changes(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |session, cx| {
                session.set_comparison(Comparison::All, cx)
            });
        }
    }

    /// The material applied to the window.
    pub fn applied_material(&self) -> Material {
        self.appearance.applied().material
    }

    /// Nothing is opening or being picked, and no round is being finished: automation's
    /// `settled`.
    pub fn is_settled(&self, cx: &App) -> bool {
        self.opening.is_none()
            && self.adding == 0
            && self
                .sheet
                .as_ref()
                .is_none_or(|sheet| !sheet.view.read(cx).is_submitting())
    }

    /// Opens or resumes a review (the switcher's rows, the source picker, `open_review`) and
    /// shows it; a review already open stays as it is, with its sheet if one is open. The work
    /// runs in the background and lands even if the returned task, which only carries the
    /// result, is dropped. A failure shows as the main area's banner, or waits for its
    /// connection as the pending pick ([`pending`]), which this pick replaces.
    pub fn open(
        &mut self,
        location: Location,
        request: OpenRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<Opened, Failure>> {
        // A round being finished keeps its sheet (and its session) until it lands, to show how
        // it went; opening another review meanwhile would drop both mid-write.
        if self
            .sheet
            .as_ref()
            .is_some_and(|sheet| sheet.view.read(cx).is_submitting())
        {
            return Task::ready(Err(Failure::new(
                "a round is being finished; open the review once it lands",
            )));
        }
        self.pending = None;
        self.close_menu(window, cx);
        self.begin_open(location, request, 0, window, cx)
    }

    /// The open itself ([`Shell::open`]), for a pick that has run again by itself `runs`
    /// times ([`pending`]).
    fn begin_open(
        &mut self,
        location: Location,
        request: OpenRequest,
        runs: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<Opened, Failure>> {
        self.open_generation += 1;
        let generation = self.open_generation;
        self.opening = Some(Opening {
            location: location.clone(),
            request: request.clone(),
            generation,
            runs,
        });
        self.banner = None;
        let reviews = cx.services().reviews.clone();
        let opening = reviews.update(cx, |reviews, cx| {
            reviews.open(location.clone(), request.clone(), cx)
        });
        let (sender, receiver) = oneshot::channel();
        cx.spawn_in(window, async move |this, cx| {
            let opened = opening.await;
            let result = this
                .update_in(cx, |this, window, cx| {
                    this.finish_open(generation, location, request, opened, window, cx)
                })
                .unwrap_or_else(|_| Err(Failure::new("dv is closing")));
            let _ = sender.send(result);
        })
        .detach();
        cx.notify();
        cx.background_spawn(async move {
            receiver
                .await
                .unwrap_or_else(|_| Err(Failure::new("dv is closing")))
        })
    }

    fn finish_open(
        &mut self,
        generation: u64,
        location: Location,
        request: OpenRequest,
        opened: Result<crate::services::reviews::OpenedReview, Failure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Opened, Failure> {
        let Some(runs) = self
            .opening
            .as_ref()
            .filter(|opening| opening.generation == generation)
            .map(|opening| opening.runs)
        else {
            return Err(Failure::new("another review was opened meanwhile"));
        };
        self.opening = None;
        cx.notify();
        match opened {
            Ok(opened) => {
                let result = Opened {
                    review: opened.loaded.review.id.clone(),
                    created: opened.created,
                };
                let same = self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.read(cx).review_id() == &result.review);
                if same {
                    if let Some(session) = &self.session {
                        session.update(cx, |session, cx| session.reload_fold(cx));
                    }
                } else {
                    let session = cx.new(|cx| ReviewSession::new(opened, cx));
                    self.open_session(session, window, cx);
                }
                // A sheet still open (the same review, resumed) keeps the keyboard, and so does
                // a menu open while a pending pick ran again by itself.
                if self.sheet.is_none() && self.switcher.is_none() && self.source_picker.is_none() {
                    self.main_focus(cx).focus(window, cx);
                }
                Ok(result)
            }
            Err(failure) => {
                self.open_failed(location, request, &failure, runs, window, cx);
                Err(failure)
            }
        }
    }

    /// Opens the switcher, its selection on `focus`'s first row (`dv open <path>`) or on the
    /// open review. Every known repository's row is recomputed meanwhile. It replaces an open
    /// menu and opens over an open sheet.
    pub fn open_switcher(
        &mut self,
        focus: Option<&Location>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let return_focus = self.take_menu().unwrap_or_else(|| window.focused(cx));
        self.close_comments_menu(cx);
        let reviews = cx.services().reviews.clone();
        reviews.update(cx, |reviews, cx| reviews.refresh_all(cx));
        let current = self
            .session
            .as_ref()
            .map(|session| session.read(cx).review_id().clone());
        let in_use = self
            .session
            .as_ref()
            .map(|session| session.read(cx).origin().clone());
        let view = cx.new(|cx| Switcher::new(reviews, focus, current.as_ref(), in_use, window, cx));
        let subscription = cx.subscribe_in(&view, window, Self::on_switcher_event);
        view.update(cx, |switcher, cx| switcher.focus(window, cx));
        self.switcher = Some(Overlay {
            view,
            return_focus,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Opens the switcher as the branch picker on the repository at `location` (the source
    /// picker's "Compare branches…"), in place of the open menu.
    pub fn open_branch_picker(
        &mut self,
        location: Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_switcher(Some(&location), window, cx);
        if let Some(switcher) = &self.switcher {
            switcher.view.update(cx, |switcher, cx| {
                switcher.compare_branches(location, window, cx)
            });
        }
    }

    /// Opens the source picker over the open review's repository. Like the switcher, it
    /// replaces an open menu and opens over an open sheet.
    pub fn open_source_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        // The targets of the repository the review was opened from; the review's own is checked
        // when that repository is its home (not when it is another worktree's, PLAN §2.4).
        let (location, target) = {
            let session = session.read(cx);
            (
                session.origin().clone(),
                session
                    .is_home_origin()
                    .then(|| session.review().target.clone()),
            )
        };
        let return_focus = self.take_menu().unwrap_or_else(|| window.focused(cx));
        self.close_comments_menu(cx);
        let reviews = cx.services().reviews.clone();
        reviews.update(cx, |reviews, cx| reviews.refresh(location.clone(), cx));
        let view = cx.new(|cx| SourcePicker::new(reviews, location, target, cx));
        let subscription = cx.subscribe_in(&view, window, Self::on_source_picker_event);
        let handle = view.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        self.source_picker = Some(Overlay {
            view,
            return_focus,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Opens the "Finish round" sheet over the open review (DESIGN §4.2), in place of an open
    /// menu; nothing happens with no review open, a finished one, or a sheet already open.
    pub fn open_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        if session.read(cx).is_read_only() || self.sheet.is_some() {
            return;
        }
        let return_focus = self.take_menu().unwrap_or_else(|| window.focused(cx));
        self.close_comments_menu(cx);
        let view = cx.new(|cx| FinishSheet::new(session, window, cx));
        let subscription = cx.subscribe_in(&view, window, Self::on_sheet_event);
        view.update(cx, |sheet, cx| sheet.focus(window, cx));
        self.sheet = Some(Overlay {
            view,
            return_focus,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// "Start a new review" on a finished one: a fresh review of the same target (PLAN §2.4:
    /// the finished one no longer counts as open).
    pub fn start_new_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else {
            return;
        };
        let (location, target) = {
            let session = session.read(cx);
            (
                session.backend().repo().location.clone(),
                session.review().target.clone(),
            )
        };
        self.open(location, OpenRequest::Target(target), window, cx)
            .detach();
    }

    fn on_sheet_event(
        &mut self,
        _: &Entity<FinishSheet>,
        event: &SheetEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SheetEvent::Dismiss => {
                self.close_sheet(window, cx);
            }
            SheetEvent::Finished(_) => {
                self.sheet = None;
                // A pick that waited for the sheet runs now, if its connection is up.
                self.run_pending(window, cx);
                // The keyboard returns to the stream, which shows the new round: now, or when
                // a menu open over the sheet closes.
                let menu_focus = match (&mut self.switcher, &mut self.source_picker) {
                    (Some(menu), _) => Some(&mut menu.return_focus),
                    (None, Some(menu)) => Some(&mut menu.return_focus),
                    (None, None) => None,
                };
                match menu_focus {
                    Some(return_focus) => *return_focus = None,
                    None => self.main_focus(cx).focus(window, cx),
                }
                cx.notify();
            }
        }
    }

    fn finish_review(&mut self, _: &FinishReview, window: &mut Window, cx: &mut Context<Self>) {
        self.open_sheet(window, cx);
    }

    /// `Ctrl C` in a text field: its selected text through the system seam
    /// ([`crate::system::write_clipboard`]), never gpui's clipboard directly. Away from any
    /// field (macOS's Edit › Copy, `crate::app_menu`, sends this wherever the keyboard is), the
    /// stream's selection, as its own `Ctrl C` copies it.
    fn copy_field(&mut self, _: &field::Copy, window: &mut Window, cx: &mut Context<Self>) {
        if !field_clipboard(false, window, cx) {
            self.diff_pane.update(cx, |pane, cx| pane.copy(cx));
        }
    }

    /// `Ctrl X` in a text field: as `Ctrl C`, then the selected text removed.
    fn cut_field(&mut self, _: &field::Cut, window: &mut Window, cx: &mut Context<Self>) {
        field_clipboard(true, window, cx);
    }

    /// Takes the open menu (the switcher or the source picker) away without moving the
    /// keyboard: `Some` with where the keyboard was before it opened, `None` when no menu was
    /// open.
    fn take_menu(&mut self) -> Option<Option<FocusHandle>> {
        // One is open at a time; each is taken either way.
        [
            self.switcher.take().map(|menu| menu.return_focus),
            self.source_picker.take().map(|menu| menu.return_focus),
            self.file_finder.take().map(|menu| menu.return_focus),
            self.palette.take().map(|menu| menu.return_focus),
            self.settings.take().map(|menu| menu.return_focus),
        ]
        .into_iter()
        .flatten()
        .next()
    }

    /// Closes the open menu, the keyboard going back to where it was before the menu opened;
    /// `false` when no menu was open.
    pub fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(return_focus) = self.take_menu() else {
            return false;
        };
        self.restore_focus(return_focus, window, cx);
        cx.notify();
        true
    }

    /// Closes the sheet, the keyboard going back to where it was before it opened, unless its
    /// round is being finished: it then stays until the round lands, to show how it went.
    /// `false` when it didn't close.
    pub fn close_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(sheet) = self
            .sheet
            .take_if(|sheet| !sheet.view.read(cx).is_submitting())
        else {
            return false;
        };
        self.restore_focus(sheet.return_focus, window, cx);
        // A pick that waited for the sheet runs now, if its connection is up ([`pending`]).
        self.run_pending(window, cx);
        cx.notify();
        true
    }

    /// Closes the top overlay (`Esc`, a click on the scrim): the open menu, else the sheet
    /// ([`Shell::close_sheet`]). `false` when nothing closed.
    pub fn close_top_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.close_menu(window, cx) || self.close_sheet(window, cx)
    }

    /// Gives the keyboard back to `handle`, or to the main area when there is none or it went
    /// away meanwhile (the file list's filter while the panel is hidden).
    fn restore_focus(&self, handle: Option<FocusHandle>, window: &mut Window, cx: &mut App) {
        let filter = self.file_list.read(cx).filter_focus(cx);
        match handle {
            Some(handle) if self.panel_visible || handle != filter => handle.focus(window, cx),
            _ => self.main_focus(cx).focus(window, cx),
        }
    }

    /// "Open folder…": the folder picker (through the system seam), then the folder's
    /// repository added and selected in the switcher ([`Shell::open_repo`]). A folder that
    /// isn't a repository shows why in the main area's banner.
    pub fn pick_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.adding += 1;
        let picked = crate::system::pick_folder("Open repository", cx);
        cx.spawn_in(window, async move |this, cx| {
            let picked = picked.await;
            this.update_in(cx, |this, window, cx| {
                match picked {
                    Ok(Some(path)) => {
                        // A folder picked through `\\wsl.localhost\<distro>\…` is a WSL
                        // location.
                        let text = path.to_string_lossy().into_owned();
                        let location = Location::parse(&text).unwrap_or(Location::Local(path));
                        this.open_repo(location, IfNotOpened::Banner, window, cx)
                            .detach();
                    }
                    Ok(None) => {}
                    Err(error) => {
                        this.banner = Some(MainBanner {
                            failure: Failure::new(format!("the folder picker failed: {error}")),
                            retry: None,
                            expanded: false,
                            pick: false,
                        });
                    }
                }
                // After `open_repo` counted itself, so `is_settled` sees no gap.
                this.adding -= 1;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// `dv open <path>` (PLAN §10.1), automation's `open` and "Open folder…": the repository at
    /// `location` becomes a known one (its worktree root; a WSL location as given, whose row
    /// says why it can't be opened yet) and the switcher opens with its first row selected. A
    /// location that doesn't open shows why in the main area's banner, or, with
    /// [`IfNotOpened::Ignore`] (bare `dv` started outside any repository), nothing. Resolves to
    /// the location as known once the switcher is open; `is_settled` waits for it.
    pub fn open_repo(
        &mut self,
        location: Location,
        if_not_opened: IfNotOpened,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<Location, Failure>> {
        self.adding += 1;
        let reviews = cx.services().reviews.clone();
        let added = reviews.update(cx, |reviews, cx| reviews.add_repo(location, cx));
        let (sender, receiver) = oneshot::channel();
        cx.spawn_in(window, async move |this, cx| {
            let added = added.await;
            let result = this
                .update_in(cx, |this, window, cx| {
                    this.adding -= 1;
                    match &added {
                        Ok(known) => this.open_switcher(Some(known), window, cx),
                        Err(failure) if if_not_opened == IfNotOpened::Banner => {
                            this.banner = Some(MainBanner {
                                failure: failure.clone(),
                                retry: None,
                                expanded: false,
                                pick: false,
                            });
                        }
                        Err(failure) => log::info!("nothing to open: {}", failure.message),
                    }
                    cx.notify();
                    added
                })
                .unwrap_or_else(|_| Err(Failure::new("dv is closing")));
            let _ = sender.send(result);
        })
        .detach();
        cx.background_spawn(async move {
            receiver
                .await
                .unwrap_or_else(|_| Err(Failure::new("dv is closing")))
        })
    }

    /// Opens the review the banner's Retry names again.
    pub fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((location, request)) = self.banner.as_ref().and_then(|b| b.retry.clone()) {
            self.open(location, request, window, cx).detach();
        }
    }

    fn on_switcher_event(
        &mut self,
        switcher: &Entity<Switcher>,
        event: &SwitcherEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SwitcherEvent::Open { location, request } => {
                self.open(location.clone(), request.clone(), window, cx)
                    .detach();
            }
            SwitcherEvent::OpenFolder => self.pick_folder(window, cx),
            SwitcherEvent::Start { location } => {
                let reviews = cx.services().reviews.clone();
                reviews
                    .update(cx, |reviews, cx| reviews.start(location.clone(), cx))
                    .detach();
            }
            SwitcherEvent::Remove { location } => {
                let reviews = cx.services().reviews.clone();
                reviews.update(cx, |reviews, cx| reviews.remove_repo(location, cx));
            }
            // The open review, when it is the one closed, follows its log like any write
            // from outside (the session's watch); a close that fails is the switcher's to say.
            SwitcherEvent::Close { location, review } => {
                let reviews = cx.services().reviews.clone();
                let close = reviews.update(cx, |reviews, cx| {
                    reviews.close_review(location.clone(), review.clone(), cx)
                });
                let switcher = switcher.downgrade();
                cx.spawn(async move |_, cx| {
                    if let Err(failure) = close.await {
                        switcher
                            .update(cx, |switcher, cx| switcher.close_failed(&failure, cx))
                            .ok();
                    }
                })
                .detach();
            }
            SwitcherEvent::Dismiss => {
                self.close_menu(window, cx);
            }
        }
    }

    fn on_source_picker_event(
        &mut self,
        _: &Entity<SourcePicker>,
        event: &SourcePickerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SourcePickerEvent::Open { location, target } => {
                self.open(
                    location.clone(),
                    OpenRequest::Target(target.clone()),
                    window,
                    cx,
                )
                .detach();
            }
            SourcePickerEvent::CompareBranches { location } => {
                self.open_branch_picker(location.clone(), window, cx);
            }
            SourcePickerEvent::Dismiss => {
                self.close_menu(window, cx);
            }
        }
    }

    fn toggle_switcher(&mut self, _: &ToggleSwitcher, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.is_some() {
            self.close_menu(window, cx);
        } else {
            self.open_switcher(None, window, cx);
        }
    }

    fn toggle_panel(&mut self, _: &TogglePanel, window: &mut Window, cx: &mut Context<Self>) {
        // The keyboard leaves a filter field that is about to disappear.
        if self.panel_visible && self.file_list.read(cx).is_filter_focused(window, cx) {
            self.main_focus(cx).focus(window, cx);
        }
        self.set_panel_visible(!self.panel_visible, cx);
    }
}

impl Focusable for Shell {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.main_focus(cx)
    }
}

/// The agent's handoff the ready banner shows for `review`, by when it landed: the `ready`
/// status change (DESIGN §4.4), or `None` while the review isn't `ready`. Another handoff
/// (the next round's, or a second `dv review done --note`) is another banner, whose "Show note"
/// starts closed.
fn handoff(review: &dv_core::review::Review) -> Option<u64> {
    let change = review.status_change.as_ref()?;
    (review.status == ReviewStatus::Ready && change.status == ReviewStatus::Ready)
        .then_some(change.at)
}

/// Options for the main window: centred at `window_size` (logical px), DESIGN's minimum size, a
/// custom title bar, and the installed theme's material.
pub fn window_options(window_size: Size<gpui::Pixels>, cx: &App) -> WindowOptions {
    let theme = cx.dv_theme();
    let min_size = size(theme.sizes.window_min_width, theme.sizes.window_min_height);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            window_size,
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("dv".into()),
            // Hides the system title bar; the shell draws its own (`title_bar`).
            appears_transparent: true,
            // macOS: centred on the 40px title bar.
            traffic_light_position: Some(point(px(12.), px(12.))),
        }),
        window_min_size: Some(min_size),
        window_background: gpui_appearance::window_background(Backing::Solid, cx),
        // Linux: dv's title bar is the window's, and its frame gpui-component's `Root`
        // (`frame`); the platform keeps its own where it can't do without (X11 with no
        // compositor). Windows and macOS take their frame from `titlebar` above.
        window_decorations: cfg!(target_os = "linux").then_some(WindowDecorations::Client),
        app_id: Some("dv".into()),
        ..Default::default()
    }
}

/// The main window and its root view.
pub struct MainWindow {
    pub handle: WindowHandle<Root>,
    pub shell: Entity<Shell>,
}

/// Opens the main window, gpui-component's `Root` around a [`Shell`], through
/// [`crate::automation::open_window`] like every dv window. Call after
/// [`crate::appearance::init`], so the window opens in the resolved theme and material.
pub fn open_main_window(window_size: Size<gpui::Pixels>, cx: &mut App) -> Result<MainWindow> {
    let options = window_options(window_size, cx);
    let mut shell = None;
    let handle = crate::automation::open_window(cx, options, |window, cx| {
        let view = cx.new(|cx| Shell::new(window, cx));
        shell = Some(view.clone());
        cx.new(|cx| Root::new(view, window, cx))
    })
    .context("opening the main window")?;
    let shell = shell.context("the main window built no shell")?;
    Ok(MainWindow { handle, shell })
}

/// The focused text field's selected text onto the clipboard through the system seam, and with
/// `cut` out of the field. Nothing happens without a selection. `false` when there was neither
/// a focused field nor selected Markdown to act on.
fn field_clipboard(cut: bool, window: &mut Window, cx: &mut App) -> bool {
    use gpui_component::WindowExt as _;
    use gpui_component::input::AnyInputState;
    macro_rules! clip {
        ($state:expr) => {{
            let text = $state.read(cx).selected_text().to_string();
            if !text.is_empty() {
                crate::system::write_clipboard(text, cx);
                if cut {
                    $state.update(cx, |state, cx| state.replace("", window, cx));
                }
            }
            true
        }};
    }
    match window.focused_input(cx) {
        Some(AnyInputState::Input(state)) => clip!(state),
        Some(AnyInputState::Textarea(state)) => clip!(state),
        // Selectable Markdown (the hover card's): the window's text selection, never cut.
        _ if !cut => {
            let text = gpui_base::TextSelection::selected_text(window, cx);
            let text = text.trim();
            if text.is_empty() {
                return false;
            }
            crate::system::write_clipboard(text.to_string(), cx);
            true
        }
        _ => false,
    }
}
