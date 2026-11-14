//! The shell's layout (DESIGN §2): title bar, then the panel and the main area side by side, then
//! the status bar, each on its surface token (DESIGN §3), and the scrim under an open overlay.

use gpui::{
    AnyElement, AppContext as _, Context, DragMoveEvent, InteractiveElement as _, IntoElement,
    MouseButton, MouseUpEvent, NavigationDirection, ParentElement as _, Render, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, Window, deferred, div, linear_color_stop,
    linear_gradient, prelude::FluentBuilder as _, px,
};

use dv_core::review::ReviewStatus;

use super::{EmptyState, ReadyNote, Shell};
use crate::actions::composer;
use crate::actions::diff_pane::ClearSelection;
use crate::automation::AutomationTargetExt as _;
use crate::banner::{Banner, BannerAction};
use crate::components::{icon, inline_code, kbd, skeleton, small_button, status_label};
use crate::diff_pane::DiffPane;
use crate::icons::DvIcon;
use crate::keymap;
use crate::services::ActiveServices as _;
use crate::services::reviews::OpenRequest;
use crate::services::settings::PANEL_WIDTH_DEFAULT;
use crate::theme::{ActiveTheme as _, DvTheme, StyledText as _};
use crate::thread::first_line;

/// Deferred paint order: the panel's resize handle over its neighbour, the scrim over the
/// window, the sheet over the scrim, and a menu (the switcher, the source picker) over
/// everything. A menu open over the sheet has the scrim over the sheet too.
pub(super) const HANDLE_PRIORITY: usize = 0;
pub(super) const SCRIM_PRIORITY: usize = 1;
pub(super) const SHEET_PRIORITY: usize = 2;
pub(super) const SCRIM_OVER_SHEET_PRIORITY: usize = 3;
pub(super) const MENU_PRIORITY: usize = 4;

/// The drag payload of the panel's resize handle (Zed's `DraggedDock` pattern).
#[derive(Clone, Copy, Debug)]
pub(super) struct DraggedPanelEdge;

impl Render for DraggedPanelEdge {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The ready banner's open note knows its scroll extent once it is laid out: draw once
        // more then, for its fade.
        if self.ready_note_open && !self.ready_note_measured {
            self.ready_note_measured = true;
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        }
        let theme = cx.dv_theme().clone();
        let c = &theme.colors;
        let overlay_open = self.switcher.is_some()
            || self.source_picker.is_some()
            || self.file_finder.is_some()
            || self.palette.is_some()
            || self.settings.is_some()
            || self.sheet.is_some();
        // The first-run screen stands in for the columns and the status bar (`setup`).
        let first_run = self.first_run_open(cx);
        div()
            .id("shell")
            .key_context(keymap::SHELL)
            .on_action(cx.listener(Self::toggle_switcher))
            .on_action(cx.listener(Self::toggle_panel))
            .on_action(cx.listener(Self::finish_review))
            .on_action(cx.listener(Self::toggle_file_finder))
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::on_go_back))
            .on_action(cx.listener(Self::on_go_forward))
            .on_action(cx.listener(Self::copy_field))
            .on_action(cx.listener(Self::cut_field))
            .on_action(cx.listener(Self::toggle_settings))
            .on_action(cx.listener(Self::continue_first_run))
            // `Esc` closes the comments list's row menu before the stream hears it (DESIGN §5:
            // the top overlay first).
            .capture_action(cx.listener(|this, _: &ClearSelection, _, cx| {
                if this.close_popover(cx) {
                    cx.stop_propagation();
                }
            }))
            // The same in the composer: a popover over it closes first, and the draft stays.
            .capture_action(cx.listener(|this, _: &composer::Cancel, _, cx| {
                if this.close_popover(cx) {
                    cx.stop_propagation();
                }
            }))
            .on_drag_move(cx.listener(Self::resize_panel))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(c.window_background)
            .text_color(c.fg)
            .text_spec(&theme.text.body)
            // First, so its capture listener runs before any other.
            .child(crate::automation::pointer_guard())
            .child(self.render_title_bar(window, &theme, cx))
            .map(|this| {
                if first_run {
                    this.child(self.render_first_run(&theme, window, cx))
                } else {
                    this.child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_row()
                            .when(self.panel_visible, |this| {
                                this.child(self.render_panel(&theme, cx))
                            })
                            .child(self.render_main(&theme, cx)),
                    )
                    .child(self.render_status_bar(&theme, cx))
                }
            })
            .when(overlay_open, |this| {
                this.child(self.render_scrim(&theme, cx))
            })
            .children(self.render_sheet(window, &theme))
            .children(self.render_settings(window, &theme))
            .children(self.render_server_details(window, &theme, cx))
            .children(self.render_update_confirm(window, &theme, cx))
    }
}

impl Shell {
    /// Closes the popover `Esc` reaches before the main area's chain or the composer (DESIGN
    /// §5): the comments list's row menu, else a language server's details from the status
    /// bar. `false` when neither was open.
    fn close_popover(&mut self, cx: &mut Context<Self>) -> bool {
        if self.comments_list.read(cx).menu_thread().is_some() {
            self.close_comments_menu(cx);
            true
        } else if self.server_details.take().is_some() {
            cx.notify();
            true
        } else {
            false
        }
    }

    /// The panel (DESIGN §2, §4.1): the file list, and the handle on its right edge.
    fn render_panel(&self, theme: &DvTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let width = cx.services().settings.read(cx).panel_width();
        div()
            .relative()
            .flex_none()
            .flex()
            .flex_col()
            .w(px(width))
            .h_full()
            .bg(c.panel)
            .border_r_1()
            .border_color(c.line)
            .debug_selector(|| "panel".into())
            .children(self.render_panel_tabs(theme, cx))
            .map(|this| match self.panel_tab(cx) {
                super::intel::PanelTab::Review => this
                    .child(self.file_list.clone())
                    .child(self.comments_list.clone()),
                super::intel::PanelTab::References => this.child(self.references_panel.clone()),
            })
            .child(self.render_resize_handle(theme, cx))
            .automation_target("shell/panel")
    }

    /// The panel's right edge: dragging it resizes the panel, a double click restores 280.
    fn render_resize_handle(&self, theme: &DvTheme, cx: &mut Context<Self>) -> AnyElement {
        let line = theme.colors.accent_ring;
        let id = "panel-resize-handle";
        deferred(
            div()
                .id(id)
                .debug_selector(move || id.into())
                .group(id)
                .absolute()
                .top_0()
                .right(px(-4.))
                .h_full()
                .w(px(8.))
                .flex()
                .justify_center()
                .cursor_col_resize()
                .occlude()
                .child(
                    div()
                        .w(px(2.))
                        .h_full()
                        .group_hover(id, move |style| style.bg(line)),
                )
                .on_drag(DraggedPanelEdge, |edge, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| *edge)
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|_, event: &MouseUpEvent, _, cx| {
                        if event.click_count == 2 {
                            let settings = cx.services().settings.clone();
                            settings.update(cx, |settings, cx| {
                                settings.set_panel_width(PANEL_WIDTH_DEFAULT, cx)
                            });
                            cx.stop_propagation();
                        }
                    }),
                )
                .automation_target("shell/panel/resize"),
        )
        .with_priority(HANDLE_PRIORITY)
        .into_any_element()
    }

    fn resize_panel(
        &mut self,
        event: &DragMoveEvent<DraggedPanelEdge>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let width = f32::from(event.event.position.x - event.bounds.left());
        let settings = cx.services().settings.clone();
        settings.update(cx, |settings, cx| settings.set_panel_width(width, cx));
    }

    /// The main area on the `code` surface: a banner when opening failed, then the stream, a
    /// loading state or DESIGN §4.4's empty states. It always renders and holds the keyboard
    /// whenever no overlay or text field does (the diff pane's focus handle and key context),
    /// so the stream's keys reach the pane whatever the area shows.
    fn render_main(&self, theme: &DvTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let banner = self.banner.as_ref().map(|banner| {
            let expanded = banner.expanded;
            let mut view = Banner::new("banner/main", banner.failure.clone()).disclosure(
                expanded,
                handler(cx, |this, _, cx| {
                    if let Some(banner) = &mut this.banner {
                        banner.expanded = !banner.expanded;
                        cx.notify();
                    }
                }),
            );
            if banner.retry.is_some() {
                view = view.action(BannerAction::new(
                    "retry",
                    "Retry",
                    handler(cx, |this, window, cx| this.retry(window, cx)),
                ));
            }
            view.action(BannerAction::new(
                "dismiss",
                "Dismiss",
                handler(cx, |this, _, cx| this.dismiss_banner(cx)),
            ))
        });
        // The connection failed for good, or found its distro stopped (DESIGN §4.4): its
        // banner says all there is, so the review's own refresh banner stays down.
        let backend_down = self.backend_down(cx);
        let backend_banner = backend_down.clone().map(|down| {
            Banner::new("banner/backend", down.failure)
                .disclosure(
                    self.backend_banner_expanded,
                    handler(cx, |this, _, cx| {
                        this.backend_banner_expanded = !this.backend_banner_expanded;
                        cx.notify();
                    }),
                )
                .action(BannerAction::new(
                    if down.stopped { "start" } else { "retry" },
                    down.action,
                    handler(cx, |this, _, cx| this.retry_backend(cx)),
                ))
                .action(BannerAction::new(
                    "setup",
                    "Open setup",
                    handler(cx, |this, window, cx| this.open_settings(window, cx)),
                ))
        });
        let session_banner = self.session.as_ref().and_then(|session| {
            if backend_down.is_some() {
                return None;
            }
            let failure = session.read(cx).error()?.clone();
            let session = session.clone();
            Some(
                Banner::new("banner/session", failure)
                    .disclosure(
                        self.session_banner_expanded,
                        handler(cx, |this, _, cx| {
                            this.session_banner_expanded = !this.session_banner_expanded;
                            cx.notify();
                        }),
                    )
                    .action(BannerAction::new("retry", "Retry", move |_, cx| {
                        session.update(cx, |session, cx| session.rescan(cx))
                    })),
            )
        });
        // A viewed toggle that didn't land: its checkbox can't say why (DESIGN §4.4).
        let viewed_banner = self.session.as_ref().and_then(|session| {
            let failure = session.read(cx).viewed_error()?.clone();
            let session = session.clone();
            Some(
                Banner::new("banner/viewed", failure).action(BannerAction::new(
                    "dismiss",
                    "Dismiss",
                    move |_, cx| session.update(cx, |session, cx| session.dismiss_viewed_error(cx)),
                )),
            )
        });
        // An interdiff that fell back to all changes: its round's snapshot is missing.
        let comparison_banner = self.session.as_ref().and_then(|session| {
            let failure = session.read(cx).comparison_error()?.clone();
            let session = session.clone();
            Some(
                Banner::new("banner/comparison", failure).action(BannerAction::new(
                    "dismiss",
                    "Dismiss",
                    move |_, cx| {
                        session.update(cx, |session, cx| session.dismiss_comparison_error(cx))
                    },
                )),
            )
        });
        let finished = self
            .session
            .as_ref()
            .map(|session| session.read(cx).review().status)
            .filter(|status| status.is_final())
            .map(|status| finished_banner(status, theme, cx));
        let ready = self.ready_note(cx).map(|ready| {
            let scroll = self.ready_note_scroll.clone();
            ready_banner(ready, self.ready_note_open, &scroll, theme, cx)
        });
        // The first listing only: a refresh keeps the stream (or the empty state) on screen, and
        // so does opening the review that is already open (it stays as it is).
        let reopening = match (&self.opening, &self.session) {
            (Some(opening), Some(session)) => {
                let session = session.read(cx);
                session.backend().repo().location == opening.location
                    && match &opening.request {
                        OpenRequest::Review(id) => session.review_id() == id,
                        OpenRequest::Target(target) => {
                            !session.review().status.is_final()
                                && &session.review().target == target
                        }
                    }
            }
            _ => false,
        };
        let loading = (self.opening.is_some() && !reopening)
            || self
                .session
                .as_ref()
                .is_some_and(|session| session.read(cx).is_first_load());
        let content: AnyElement = match self.empty_state(cx) {
            Some(empty) => empty_view(empty, theme, cx),
            None if loading => loading_view(self.opening.is_some(), theme),
            None if self.session.is_some() => div()
                .flex_1()
                .min_h_0()
                .child(self.main_pane().clone())
                .into_any_element(),
            None => div().flex_1().into_any_element(),
        };
        let main = div()
            .id("main")
            .key_context(keymap::DIFF_PANE)
            .track_focus(&self.main_focus(cx))
            // The mouse's back and forward buttons step through the history (PLAN §18.1).
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, window, cx| this.go_back(window, cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _, window, cx| this.go_forward(window, cx)),
            );
        // Clipped: a banner never draws over the status bar (DESIGN §4.4).
        DiffPane::bind_actions(main, self.main_pane())
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(c.code)
            .debug_selector(|| "main".into())
            .children(banner)
            .children(backend_banner)
            .children(session_banner)
            .children(viewed_banner)
            .children(comparison_banner)
            .children(self.intel_notice(theme, cx))
            .children(finished)
            .children(ready)
            .child(content)
            .automation_target("shell/main")
    }

    /// The "Finish round" sheet, centred over the scrim (DESIGN §4.2).
    fn render_sheet(&self, window: &Window, theme: &DvTheme) -> Option<AnyElement> {
        let sheet = self.sheet.as_ref()?;
        let top = crate::submit::sheet_top(window.viewport_size().height);
        Some(
            deferred(
                div()
                    .absolute()
                    .top(theme.sizes.title_bar_height)
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .flex()
                    .justify_center()
                    .items_start()
                    .pt(top)
                    .child(sheet.view.clone()),
            )
            .with_priority(SHEET_PRIORITY)
            .into_any_element(),
        )
    }

    /// The scrim under the top overlay (DESIGN §3): below the title bar, taking the mouse from
    /// everything under it (a sheet with a menu open over it too); a click on it closes that
    /// overlay.
    fn render_scrim(&self, theme: &DvTheme, cx: &mut Context<Self>) -> AnyElement {
        let menu_open = self.switcher.is_some()
            || self.source_picker.is_some()
            || self.file_finder.is_some()
            || self.palette.is_some()
            || self.settings.is_some();
        let priority = if menu_open && self.sheet.is_some() {
            SCRIM_OVER_SHEET_PRIORITY
        } else {
            SCRIM_PRIORITY
        };
        deferred(
            div()
                .id("scrim")
                .debug_selector(|| "scrim".into())
                .absolute()
                .top(theme.sizes.title_bar_height)
                .left_0()
                .right_0()
                .bottom_0()
                .bg(theme.colors.scrim)
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.close_top_overlay(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .automation_target("shell/scrim"),
        )
        .with_priority(priority)
        .into_any_element()
    }
}

/// A handler for a stateless element (a banner's buttons) that runs `f` on the shell.
fn handler(
    cx: &Context<Shell>,
    f: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
) -> impl Fn(&mut Window, &mut gpui::App) + 'static {
    let this = cx.weak_entity();
    move |window, cx| {
        this.update(cx, |this, cx| f(this, window, cx)).ok();
    }
}

/// The banner over a finished review (DESIGN §4.4 "Read-only"): "Approved · this review is
/// finished", with "Start a new review". Its composer and thread actions stay, disabled.
fn finished_banner(status: ReviewStatus, theme: &DvTheme, cx: &mut Context<Shell>) -> AnyElement {
    let c = &theme.colors;
    let approved = status == ReviewStatus::Approved;
    let (glyph, color, fill) = if approved {
        (DvIcon::CircleCheck, c.green, c.green_soft)
    } else {
        (DvIcon::CircleMinus, c.fg2, c.layer)
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .m(theme.spacing.lg)
        .min_h(px(40.))
        .py(theme.spacing.sm)
        .pl(theme.spacing.lg)
        .pr(theme.spacing.md)
        .rounded(theme.radii.control)
        .border_1()
        .border_color(color.opacity(0.35))
        .bg(fill)
        .child(icon(glyph, px(15.), color))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(format!(
                    "{} · this review is finished",
                    status_label(status)
                )),
        )
        .child(
            small_button("start-new-review", "Start a new review", theme)
                .on_click(cx.listener(|this, _, window, cx| this.start_new_review(window, cx)))
                .automation_target("banner/finished/new_review"),
        )
        .automation_target("banner/finished")
}

/// How tall the ready banner's open note grows before it scrolls (DESIGN §4.4), like the
/// comments list.
const READY_NOTE_MAX_HEIGHT: f32 = 240.;

/// How tall the fade at the foot of a scrolling note is (DESIGN §4.4).
const READY_NOTE_FADE: f32 = 28.;

/// The agent's handoff over a `ready` review (DESIGN §4.4): on `blueSoft` with a `blue` outline
/// at 35%, the agent's glyph, "Claude Fable 5.2 marked round 2 ready" (the agent as it shows,
/// `crate::agents::shown_name`), the note's first line as
/// plain text and "Show note", which opens the whole note as Markdown under it, scrolling
/// (`scroll`) past READY_NOTE_MAX_HEIGHT with a fade at its foot while more lies below.
fn ready_banner(
    ready: ReadyNote,
    open: bool,
    scroll: &ScrollHandle,
    theme: &DvTheme,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let c = &theme.colors;
    let who = ready.author.as_ref().map_or_else(
        || "The agent".to_string(),
        |author| crate::agents::shown_name(author).to_string(),
    );
    // The first line stands in for the note while it is closed.
    let first = ready
        .note
        .as_deref()
        .filter(|_| !open)
        .map(first_line)
        .filter(|line| !line.is_empty());
    let hover = c.control_hover;
    let disclosure = ready.note.is_some().then(|| {
        div()
            .id("ready-note")
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.))
            .h(theme.sizes.control_small_height)
            .px(theme.spacing.sm)
            .rounded(theme.radii.control)
            .text_spec(&theme.text.secondary)
            .text_color(c.fg2)
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .child(icon(
                if open {
                    DvIcon::ChevronDown
                } else {
                    DvIcon::ChevronRight
                },
                px(12.),
                c.fg2,
            ))
            .child(if open { "Hide note" } else { "Show note" })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_ready_note(cx)))
            .debug_selector(|| "ready-note-toggle".into())
            .automation_target("banner/ready/note")
    });
    let header = div()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .min_h(px(40.))
        .py(theme.spacing.sm)
        .pl(theme.spacing.lg)
        .pr(theme.spacing.md)
        // Its model family's mark, when it named a model dv knows (`crate::agents`).
        .child(crate::agents::glyph(ready.author.as_ref(), px(15.), theme))
        // The agent's name truncates before anything else does; "marked round n ready" and
        // the disclosure always show (DESIGN §4.4).
        .child(
            div()
                .min_w_0()
                .flex_shrink(1.)
                .flex()
                .items_center()
                .gap(px(4.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(who),
                )
                .child(
                    div()
                        .flex_none()
                        .child(format!("marked round {} ready", ready.round)),
                )
                // The annotations it wrote that round, which the stream shows beside the code
                // (PLAN §19.3).
                .when(ready.annotations > 0, |this| {
                    this.child(
                        div()
                            .flex_none()
                            .font_weight(gpui::FontWeight::NORMAL)
                            .text_color(c.fg2)
                            .child(match ready.annotations {
                                1 => "· 1 annotation".to_string(),
                                n => format!("· {n} annotations"),
                            }),
                    )
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(c.fg2)
                .children(first),
        )
        .children(disclosure);
    // An open note grows to READY_NOTE_MAX_HEIGHT, then scrolls, so the stream keeps its room.
    // Its inset at the foot sits outside the scrolled text, so a clipped line stops short of
    // the outline, and a fade there says the note goes on while more of it lies below.
    let max = scroll.max_offset().y;
    let more_below = max > px(0.5) && -scroll.offset().y < max - px(0.5);
    // The banner is `blueSoft` over an opaque `code` (DESIGN §4.4), so the fade can end in
    // exactly its colour: over a translucent surface the material shows through the banner and
    // through the note behind the fade alike, and no colour painted on top matches both.
    let fill = c.code.alpha(1.).blend(c.blue_soft);
    let note = ready.note.filter(|_| open).map(|note| {
        div()
            .relative()
            .pb(theme.spacing.md)
            .child(
                div()
                    .id("ready-note-scroll")
                    .max_h(px(READY_NOTE_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .track_scroll(scroll)
                    .on_scroll(cx.listener(|_, _, _, cx| cx.notify()))
                    .px(theme.spacing.lg)
                    .pl(px(43.))
                    .child(
                        crate::markdown::view("ready-note-body", &note, cx)
                            .text_size(theme.text.body.size)
                            .line_height(px(20.))
                            .text_color(c.fg),
                    )
                    .debug_selector(|| "ready-note-body".into())
                    .automation_target("banner/ready/body"),
            )
            .when(more_below, |this| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(theme.spacing.md)
                        .h(px(READY_NOTE_FADE))
                        .bg(linear_gradient(
                            180.,
                            linear_color_stop(fill.opacity(0.), 0.),
                            linear_color_stop(fill, 1.),
                        ))
                        .debug_selector(|| "ready-note-fade".into()),
                )
            })
    });
    div()
        .flex_none()
        .flex()
        .flex_col()
        .m(theme.spacing.lg)
        .rounded(theme.radii.control)
        .border_1()
        .border_color(c.blue.opacity(0.35))
        .bg(fill)
        .debug_selector(|| "ready-banner".into())
        .child(header)
        .children(note)
        .automation_target("banner/ready")
}

/// The main area while a review opens or lists its files (DESIGN §4.4): a quiet progress line
/// where the file header goes, over skeleton rows, never a spinner.
fn loading_view(opening: bool, theme: &DvTheme) -> AnyElement {
    let c = &theme.colors;
    div()
        .flex_1()
        .flex()
        .flex_col()
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .h(px(44.))
                .pl(theme.spacing.xl)
                .bg(c.code_head)
                .border_b_1()
                .border_color(c.line)
                .text_spec(&theme.text.secondary)
                .text_color(c.fg3)
                .child(if opening {
                    "Opening the review…"
                } else {
                    "Listing changes…"
                }),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(theme.spacing.md)
                .p(theme.spacing.xl)
                .children(
                    [420., 360., 500., 280., 440., 320.]
                        .into_iter()
                        .map(|width| skeleton(px(width), px(10.), theme)),
                ),
        )
        .automation_target("shell/main/loading")
}

/// DESIGN §4.4's empty states.
fn empty_view(empty: EmptyState, theme: &DvTheme, cx: &mut Context<Shell>) -> AnyElement {
    let c = &theme.colors;
    let centred = div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(theme.spacing.md);
    let heading = |text: String| {
        div()
            .text_spec(&theme.text.heading)
            .text_color(c.fg)
            .child(text)
    };
    let note = |text: &'static str| div().text_color(c.fg2).child(text);
    match empty {
        EmptyState::NoReview => centred.child(heading(empty.heading())).child(
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.sm)
                .text_color(c.fg2)
                .child("Open a folder with")
                // The comma hugs the keycap.
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(kbd(keymap::label("secondary-o"), theme))
                        .child(","),
                )
                .child("or run")
                .child(inline_code("dv", theme))
                .child("inside a repo"),
        ),
        EmptyState::WorkingTreeClean | EmptyState::NoChanges => centred
            .child(heading(empty.heading()))
            .child(note(empty.note())),
        EmptyState::NoChangesSince(_) => centred
            .child(heading(empty.heading()))
            .child(note(empty.note()))
            .child(
                crate::components::button("show-all-changes", theme)
                    .debug_selector(|| "show-all-changes".into())
                    .mt(theme.spacing.sm)
                    .child("Show all changes")
                    .child(kbd(keymap::label("i"), theme))
                    .on_click(cx.listener(|this, _, _, cx| this.show_all_changes(cx)))
                    .automation_target("shell/main/empty/show_all"),
            ),
    }
    .automation_target("shell/main/empty")
}
