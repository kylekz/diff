//! Pull requests in the shell (PLAN §9.9, DESIGN §4.5): which repository is in view, the PR
//! picker behind the title bar's **PRs** and `Ctrl G`, the folder chosen for a pull request
//! whose repository dv knows no clone of, and `Ctrl R`, which refreshes the open PR review.
//! The strip under the title bar is [`super::pr_strip`].
//!
//! **The repository in view** ([`Shell::repo_in_view`]) is the one `#n` names in the palette
//! and the picker lists: the open review's (the repository it was opened from), else the
//! one being opened, else the one the main area's banner is about (an open that failed in a
//! repository that did open).
//!
//! **Choose its folder** ([`Shell::choose_pr_folder`], the palette's and the picker's row
//! for a pull request of a repository dv knows no clone of): the folder picker through the
//! system seam, then the folder is checked against the reference (`Reviews::pr_folder`). A
//! clone of that repository opens the pull request, which makes it a known repository as
//! any open does; any other folder adds nothing, and the palette or the picker, still open,
//! says why under the row. dv never clones. One choice at a time: while a folder is being
//! chosen or checked, asking again does nothing. And the answer belongs to what asked for
//! it ([`FolderAsker`]): the system's folder panel doesn't stop the window behind it, so by
//! the time a folder is picked the palette may be closed, another review open, or another
//! reference typed. An answer whose asker no longer shows the row that asked is dropped,
//! whatever it is: nothing opens, and no reason lands under another row.

use dv_core::Location;
use dv_core::github::PrRef;
use dv_core::review::ReviewId;
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _,
    IntoElement as _, ParentElement as _, Pixels, Role, StatefulInteractiveElement as _,
    Styled as _, Window, anchored, deferred, div, point, prelude::FluentBuilder as _, px,
};

use super::render::MENU_PRIORITY;
use super::{MainBanner, Opened, Overlay, Shell, Unread};
use crate::actions::shell::{RefreshPr, TogglePrPicker};
use crate::automation::AutomationTargetExt as _;
use crate::components::{icon, kbd, text_width, tooltip};
use crate::failure::Failure;
use crate::icons::DvIcon;
use crate::keymap;
use crate::pr_picker::{PrPicker, PrPickerEvent};
use crate::services::ActiveServices as _;
use crate::services::reviews::OpenRequest;
use crate::session::ReviewSession;
use crate::theme::{DvTheme, StyledText as _};

/// What asked for a pull request's folder: the palette or the picker that showed the row,
/// as it was then (a menu closed and opened again is another one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderAsker {
    Palette(gpui::EntityId),
    Picker(gpui::EntityId),
}

/// The prompt of the folder picker a pull request's "Choose its folder" opens.
pub const CHOOSE_FOLDER_PROMPT: &str = "Choose its folder";

impl Shell {
    /// The banner over the pull request's review `review`, which shows as it was last read
    /// because GitHub couldn't be read (`stale`, PLAN §9.3): why, and a Retry of the open.
    /// It is that review's failed read whole, so the strip's "details" shows no second
    /// banner while it is up (`pr_strip`), and it goes once that read is no longer how the
    /// pull request stands ([`Shell::pr_changed`]).
    pub(super) fn show_unread(
        &mut self,
        location: Location,
        request: OpenRequest,
        review: ReviewId,
        stale: Failure,
    ) {
        let OpenRequest::Pr(reference) = &request else {
            return;
        };
        let failure = Failure {
            message: format!(
                "couldn't read #{} from GitHub: {}",
                reference.number(),
                stale.message
            ),
            ..stale.clone()
        };
        self.banner = Some(MainBanner {
            location: Some(location.clone()),
            retry: failure.retryable.then_some((location, request)),
            failure,
            expanded: false,
            pick: false,
            unread: Some(Unread { review, stale }),
        });
        self.pr_error_open = false;
    }

    /// Whether the open's banner about the open review's unread pull request is up.
    pub(super) fn unread_banner_is_up(&self, cx: &App) -> bool {
        let (Some(banner), Some(session)) = (&self.banner, &self.session) else {
            return false;
        };
        banner
            .unread
            .as_ref()
            .is_some_and(|unread| &unread.review == session.read(cx).review_id())
    }

    /// What `session` holds of its pull request changed: the open's banner saying GitHub
    /// couldn't be read goes once the failure it says is no longer the session's, the pull
    /// request read or its read failed another way. Another failure shows in its place as
    /// the strip's own banner, so a Retry that fails differently says how.
    pub(super) fn pr_changed(&mut self, session: &Entity<ReviewSession>, cx: &mut Context<Self>) {
        let session = session.read(cx);
        let stale = session.pr().and_then(|read| read.stale.as_ref());
        if self
            .banner
            .as_ref()
            .and_then(|banner| banner.unread.as_ref())
            .is_some_and(|unread| {
                &unread.review == session.review_id() && Some(&unread.stale) != stale
            })
        {
            self.banner = None;
            self.pr_error_open = stale.is_some();
            self.pr_banner_expanded = false;
            cx.notify();
        }
    }

    /// The open of the pull request already showing is done ([`Shell::begin_open`]): its
    /// session refreshed it, and a refresh that failed is the open's banner as well as the
    /// strip's line.
    pub(super) fn finish_refresh(
        &mut self,
        generation: u64,
        location: Location,
        request: OpenRequest,
        session: &Entity<ReviewSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Opened, Failure> {
        if self
            .opening
            .as_ref()
            .is_none_or(|opening| opening.generation != generation)
        {
            return Err(Failure::new("another review was opened meanwhile"));
        }
        self.opening = None;
        cx.notify();
        if self.sheet.is_none() && self.switcher.is_none() && self.source_picker.is_none() {
            self.main_focus(cx).focus(window, cx);
        }
        let session = session.read(cx);
        let review = session.review_id().clone();
        if let Some(stale) = session.pr().and_then(|read| read.stale.clone()) {
            self.show_unread(location, request, review.clone(), stale);
        }
        Ok(Opened {
            review,
            created: false,
        })
    }

    /// The repository in view (module docs).
    pub fn repo_in_view(&self, cx: &App) -> Option<Location> {
        self.session
            .as_ref()
            .map(|session| session.read(cx).origin().clone())
            .or_else(|| {
                self.opening
                    .as_ref()
                    .map(|opening| opening.location.clone())
            })
            .or_else(|| {
                self.banner
                    .as_ref()
                    .and_then(|banner| banner.location.clone())
            })
    }

    /// The PR picker, while it is open.
    pub fn pr_picker(&self) -> Option<&Entity<PrPicker>> {
        self.pr_picker.as_ref().map(|overlay| &overlay.view)
    }

    /// `Ctrl G`: the PR picker, or closed when it is open.
    pub(super) fn toggle_pr_picker(
        &mut self,
        _: &TogglePrPicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pr_picker.is_some() {
            self.close_menu(window, cx);
        } else {
            self.open_pr_picker(window, cx);
        }
    }

    /// Opens the PR picker over the repository in view, in place of an open menu and over an
    /// open sheet. The repository's list is read again meanwhile (`PrPicker::new`).
    pub fn open_pr_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let return_focus = self.take_menu().unwrap_or_else(|| window.focused(cx));
        self.close_comments_menu(cx);
        let (reviews, github, locations) = {
            let services = cx.services();
            (
                services.reviews.clone(),
                services.github.clone(),
                services.locations.clone(),
            )
        };
        let in_view = self.repo_in_view(cx);
        let view = cx.new(|cx| PrPicker::new(reviews, github, &locations, in_view, window, cx));
        let subscription = cx.subscribe_in(&view, window, Self::on_pr_picker_event);
        view.update(cx, |picker, cx| picker.focus(window, cx));
        self.pr_picker = Some(Overlay {
            view,
            return_focus,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn on_pr_picker_event(
        &mut self,
        picker: &Entity<PrPicker>,
        event: &PrPickerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PrPickerEvent::Dismiss => {
                self.close_menu(window, cx);
            }
            PrPickerEvent::Open {
                location,
                reference,
            } => {
                self.open(
                    location.clone(),
                    OpenRequest::Pr(reference.clone()),
                    window,
                    cx,
                )
                .detach();
            }
            PrPickerEvent::ChooseFolder(reference) => {
                let asker = FolderAsker::Picker(picker.entity_id());
                self.choose_pr_folder(reference.clone(), asker, window, cx);
            }
        }
    }

    /// "Choose its folder" for the pull request `reference` names, asked by `asker` (module
    /// docs). The menu that asked stays open until a folder that is the repository's clone
    /// was chosen.
    pub fn choose_pr_folder(
        &mut self,
        reference: PrRef,
        asker: FolderAsker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.choosing_pr_folder {
            return;
        }
        self.choosing_pr_folder = true;
        self.adding += 1;
        let picked = crate::system::pick_folder(CHOOSE_FOLDER_PROMPT, cx);
        cx.spawn_in(window, async move |this, cx| {
            let picked = picked.await;
            // Asked again once the folder is picked, and once more when it is checked.
            let still_asked = |this: &gpui::WeakEntity<Self>, cx: &mut gpui::AsyncWindowContext| {
                this.update(cx, |this, cx| this.still_asks_folder(asker, &reference, cx))
                    .unwrap_or(false)
            };
            // The folder as picked, once it is: what an answer nobody waits for lets go of.
            let mut given = None;
            let checked = match picked {
                _ if !still_asked(&this, cx) => None,
                Ok(Some(path)) => {
                    // A folder picked through `\\wsl.localhost\<distro>\…` is a WSL location.
                    let text = path.to_string_lossy().into_owned();
                    let location = Location::parse(&text).unwrap_or(Location::Local(path));
                    given = Some(location.clone());
                    let check = this.update(cx, |_, cx| {
                        let reviews = cx.services().reviews.clone();
                        reviews.update(cx, |reviews, cx| {
                            reviews.pr_folder(location, reference.clone(), cx)
                        })
                    });
                    match check {
                        Ok(check) => Some(check.await),
                        Err(_) => None,
                    }
                }
                Ok(None) => None,
                Err(error) => Some(Err(Failure::new(format!(
                    "the folder picker failed: {error}"
                )))),
            };
            this.update_in(cx, |this, window, cx| {
                this.choosing_pr_folder = false;
                let asked = this.still_asks_folder(asker, &reference, cx);
                match checked {
                    Some(Ok(clone)) if asked => {
                        this.open(clone, OpenRequest::Pr(reference), window, cx)
                            .detach();
                    }
                    // Its clone, but nobody waits for it: nothing opens, and nothing stays
                    // open for it.
                    Some(Ok(clone)) => {
                        let reviews = cx.services().reviews.clone();
                        reviews.update(cx, |reviews, cx| {
                            reviews.pr_folder_dropped(given.as_ref().unwrap_or(&clone), &clone, cx)
                        });
                    }
                    Some(Err(failure)) if asked => this.pr_folder_refused(asker, failure, cx),
                    Some(Err(_)) | None => {}
                }
                // After `open` counted itself, so `is_settled` sees no gap.
                this.adding -= 1;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Whether `asker` still shows the row that asked for `reference`'s folder.
    fn still_asks_folder(&self, asker: FolderAsker, reference: &PrRef, cx: &App) -> bool {
        match asker {
            FolderAsker::Palette(id) => self.palette.as_ref().is_some_and(|palette| {
                palette.view.entity_id() == id && palette.view.read(cx).asks_folder_for(reference)
            }),
            FolderAsker::Picker(id) => self.pr_picker.as_ref().is_some_and(|picker| {
                picker.view.entity_id() == id && picker.view.read(cx).asks_folder_for(reference)
            }),
        }
    }

    /// Whether a pull request's folder is being chosen or checked.
    pub fn is_choosing_pr_folder(&self) -> bool {
        self.choosing_pr_folder
    }

    /// The folder chosen for a pull request wasn't taken: `asker`, which still shows the
    /// row, says why under it.
    fn pr_folder_refused(&mut self, asker: FolderAsker, failure: Failure, cx: &mut Context<Self>) {
        match asker {
            FolderAsker::Palette(_) => {
                if let Some(palette) = &self.palette {
                    palette.view.update(cx, |palette, cx| {
                        palette.folder_refused(failure.message, cx)
                    });
                }
            }
            FolderAsker::Picker(_) => {
                if let Some(picker) = &self.pr_picker {
                    picker
                        .view
                        .update(cx, |picker, cx| picker.folder_refused(failure.message, cx));
                }
            }
        }
    }

    /// `Ctrl R`: refreshes the open PR review from GitHub (PLAN §9.4); nothing on a local
    /// review, or with none open.
    pub(super) fn refresh_pr(&mut self, _: &RefreshPr, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_pull_request(cx);
    }

    /// The strip's **Refresh**, `Ctrl R` and the refresh banner's Retry: one more refresh
    /// through the session's queue (`ReviewSession::refresh_pr`: one thing asked of GitHub
    /// at a time).
    pub fn refresh_pull_request(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |session, cx| session.refresh_pr(cx));
        }
    }

    /// The title bar's **PRs** (`Ctrl G`), and the picker under it while open. `compact`: its
    /// glyph alone, where the left group needs the room (`super::title_bar`).
    pub(super) fn render_prs_button(
        &self,
        theme: &DvTheme,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let hover = c.control_hover;
        let open = self.pr_picker.as_ref().map(|overlay| overlay.view.clone());
        div()
            .relative()
            .flex_none()
            .mr(px(2.))
            .child(
                div()
                    .id("titlebar-prs")
                    .role(Role::Button)
                    .aria_label(format!(
                        "Open a pull request ({})",
                        keymap::label("secondary-g")
                    ))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(theme.sizes.control_height)
                    .px(theme.spacing.md)
                    .rounded(theme.radii.control)
                    .text_spec(&theme.text.control)
                    .text_color(c.fg2)
                    // A control inside the drag region.
                    .occlude()
                    .cursor_pointer()
                    .when(open.is_some(), |this| this.bg(c.control_hover))
                    .hover(move |style| style.bg(hover))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_pr_picker(&TogglePrPicker, window, cx)
                    }))
                    .child(icon(DvIcon::GitPullRequest, px(14.), c.fg2))
                    .map(|this| {
                        if compact {
                            this.tooltip(tooltip(format!(
                                "Pull requests  {}",
                                keymap::label("secondary-g")
                            )))
                        } else {
                            this.child("PRs")
                                .child(kbd(keymap::label("secondary-g"), theme))
                        }
                    })
                    .automation_target("titlebar/prs"),
            )
            // Hung from the button's bottom-right corner, 8px down, kept inside the window.
            .children(open.map(|view| {
                div().absolute().right_0().bottom_0().child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopRight)
                            .offset(point(px(0.), px(8.)))
                            .snap_to_window_with_margin(px(8.))
                            .child(view),
                    )
                    .with_priority(MENU_PRIORITY),
                )
            }))
            .into_any_element()
    }
}

/// The room **PRs** takes as its glyph alone, its margin included.
pub(super) const PRS_COMPACT_ROOM: Pixels = px(32.);

/// How wide the title bar's **PRs** button draws, its margin included: the room the command
/// field keeps clear of it (`super::title_bar`).
pub(super) fn prs_button_room(window: &Window, theme: &DvTheme) -> Pixels {
    let keycap = (theme.spacing.xs + px(1.)) * 2.
        + px(2.)
        + text_width(window, &keymap::label("secondary-g"), &theme.text.kbd);
    theme.spacing.md * 2.
        + px(14.)
        + px(6.) * 2.
        + text_width(window, "PRs", &theme.text.control)
        + keycap
        + px(2.)
}
