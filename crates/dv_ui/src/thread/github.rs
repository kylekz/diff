//! The card of one of GitHub's threads (DESIGN §4.5, PLAN §9.6): the thread card's frame with
//! GitHub's mark as each comment's avatar, the login as its name and an "on GitHub" tag; no
//! state chip but **Resolved**, and one saying why it has no line when it has none
//! ([`GithubCard::standing_chip`]); **Resolve** / **Unresolve** where GitHub allows the
//! viewer, and **Open on GitHub** when its link is a page of the pull request's host. A
//! resolved one collapses to its header like a resolved thread of the review's own.
//!
//! Whatever of GitHub's is drawn on one line (the path, a login, a row of code) is drawn as
//! one line whatever it holds (`one_line`); a comment's body keeps its line breaks as
//! GitHub shows them (`markdown::with_line_breaks`).
//!
//! A comment of the viewer's own that isn't submitted yet wears "Pending on GitHub". A thread
//! without a place in the diff (outdated on GitHub, or one dv can't place) shows the end of
//! its diff hunk as GitHub shows it, the commented lines marked; one dv can't find in the
//! head it shows, the lines it is on.
//!
//! There is no reply field: a reply to a GitHub thread posts with the review, which arrives
//! with submitting (PLAN §9.12). `R` on the card says so under its comments, and `X` on a
//! thread GitHub doesn't let the viewer resolve says that there ([`GithubNotice`]).
//!
//! Automation targets: `thread/gh:<node id>` with `/resolve` or `/unresolve`, `/toggle`,
//! `/open`, `/view_file`, `/error`, and `/reply_notice` or `/resolve_notice`.

use std::rc::Rc;

use dv_core::diff::LineKind;
use dv_core::github::{GithubComment, Standing, hunk_rows};
use dv_core::review::ThreadKey;
use gpui::{
    AnyElement, App, ElementId, FontFeatures, FontWeight, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, RenderOnce, StatefulInteractiveElement as _, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};

use super::render::{comment_frame, tag};
use super::{first_line, one_line, shown_anchor_label};
use crate::automation::AutomationTargetExt as _;
use crate::components::{chip, icon, kbd, small_button};
use crate::failure::Failure;
use crate::icons::DvIcon;
use crate::keymap;
use crate::session::GithubShown;
use crate::theme::{ActiveTheme as _, DvTheme, StyledText as _};

type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

/// What a GitHub card's reply notice says (`R` on the card): replies post with the review.
pub const REPLY_NOTICE: &str = "Replies to GitHub's threads post with your review, and \
    submitting to GitHub arrives in a later version. Until then, reply on GitHub.";

/// What a GitHub card says under its comments about a key that did nothing on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GithubNotice {
    /// `R`: [`REPLY_NOTICE`].
    Reply,
    /// `X` on a thread GitHub doesn't let the viewer resolve (`true`) or unresolve.
    NotAllowed(bool),
}

impl GithubNotice {
    pub fn text(self) -> &'static str {
        match self {
            Self::Reply => REPLY_NOTICE,
            Self::NotAllowed(true) => "GitHub doesn't let this account resolve this thread.",
            Self::NotAllowed(false) => "GitHub doesn't let this account unresolve this thread.",
        }
    }

    /// The notice's automation target under its card's.
    fn target(self) -> &'static str {
        match self {
            Self::Reply => "reply_notice",
            Self::NotAllowed(_) => "resolve_notice",
        }
    }
}

/// A GitHub thread's card (module docs). The stream places it; the card fills its width.
#[derive(IntoElement)]
pub struct GithubCard {
    shown: GithubShown,
    now: u64,
    /// dv can't find its code in the diff shown (PLAN §8): the yellow chip and its lines.
    outdated: bool,
    path: Option<String>,
    lines: Option<(u32, u32)>,
    moved: bool,
    unlisted: Option<Handler>,
    focused: bool,
    collapsed: bool,
    /// A Resolve (`true`) or an Unresolve is on its way to GitHub.
    resolving: Option<bool>,
    notice: Option<GithubNotice>,
    error: Option<Failure>,
    on_resolve: Option<Handler>,
    on_toggle: Option<Handler>,
    on_open: Option<Handler>,
    on_focus: Option<Handler>,
}

impl GithubCard {
    /// A card for `shown`, its times relative to `now` (ms since the epoch).
    pub fn new(shown: GithubShown, now: u64) -> Self {
        Self {
            shown,
            now,
            outdated: false,
            path: None,
            lines: None,
            moved: false,
            unlisted: None,
            focused: false,
            collapsed: false,
            resolving: None,
            notice: None,
            error: None,
            on_resolve: None,
            on_toggle: None,
            on_open: None,
            on_focus: None,
        }
    }

    /// Its code isn't found in the diff shown: the yellow chip and its lines as they were.
    pub fn outdated(mut self, outdated: bool) -> Self {
        self.outdated = outdated;
        self
    }

    /// Names `path` before the anchor, for a card in the "Not in the diff" section.
    pub fn path(mut self, path: Option<String>) -> Self {
        self.path = path;
        self
    }

    /// Where it shows now and whether those are other lines than GitHub has it at.
    pub fn shown_at(mut self, lines: Option<(u32, u32)>, moved: bool) -> Self {
        self.lines = lines;
        self.moved = moved;
        self
    }

    /// On a file the review doesn't change, found there: the "Unchanged file" chip and View
    /// file, which runs `view`.
    pub fn unlisted(mut self, view: Option<impl Fn(&mut Window, &mut App) + 'static>) -> Self {
        self.unlisted = view.map(|view| Rc::new(view) as Handler);
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// One line instead of the whole thread (a resolved one until it is opened).
    pub fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }

    /// A Resolve (`Some(true)`) or an Unresolve is running: the button says so and takes no
    /// click.
    pub fn resolving(mut self, resolving: Option<bool>) -> Self {
        self.resolving = resolving;
        self
    }

    /// What the card says under its comments, when a key did nothing on it.
    pub fn notice(mut self, notice: Option<GithubNotice>) -> Self {
        self.notice = notice;
        self
    }

    /// Why the last Resolve or Unresolve failed.
    pub fn error(mut self, error: Option<Failure>) -> Self {
        self.error = error;
        self
    }

    /// Resolve or Unresolve, whichever GitHub's state makes it.
    pub fn on_resolve(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_resolve = Some(Rc::new(f));
        self
    }

    pub fn on_toggle(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_toggle = Some(Rc::new(f));
        self
    }

    /// Open on GitHub: the thread's page.
    pub fn on_open(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_open = Some(Rc::new(f));
        self
    }

    pub fn on_focus(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_focus = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for GithubCard {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.dv_theme().clone();
        let c = &theme.colors;
        let thread = &self.shown.thread;
        let name = format!("thread/{}", ThreadKey::Github(thread.id.clone()));
        let anchor = shown_anchor_label(&self.shown.anchor, self.lines, self.moved);
        let anchor_text = match &self.path {
            Some(path) => format!("{} · {anchor}", one_line(path)),
            None => anchor,
        };
        let standing = self.standing_chip();
        let first = thread.comments.first();

        let header = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .h(px(32.))
            .pl(theme.spacing.lg)
            .pr(theme.spacing.sm)
            .when(!self.collapsed, |this| {
                this.border_b_1().border_color(c.line)
            })
            .when(thread.resolved, |this| {
                this.child(
                    chip(c.green, c.green_soft, &theme)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(icon(DvIcon::CircleCheck, px(12.), c.green))
                        .child("Resolved"),
                )
            })
            .when_some(standing, |this, standing| {
                this.child(
                    chip(c.yellow, c.yellow_soft, &theme)
                        .flex_none()
                        .child(icon(DvIcon::TriangleAlert, px(12.), c.yellow))
                        .child(standing),
                )
            })
            .when(self.unlisted.is_some(), |this| {
                this.child(
                    chip(c.fg2, c.layer, &theme)
                        .flex_none()
                        .child(icon(DvIcon::File, px(12.), c.fg3))
                        .child("Unchanged file"),
                )
            })
            // The anchor and a collapsed thread's first line give way first; the actions
            // never do.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .overflow_hidden()
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .map(|this| {
                                if self.path.is_some() {
                                    this.text_ellipsis_start()
                                } else {
                                    this.text_ellipsis()
                                }
                            })
                            .text_spec(&theme.text.secondary)
                            .text_color(c.fg3)
                            .child(anchor_text),
                    )
                    .when(self.collapsed, |this| {
                        this.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_spec(&theme.text.secondary)
                                .text_color(c.fg2)
                                .child(first_line(
                                    first.map_or("", |comment| comment.body.as_str()),
                                )),
                        )
                    }),
            )
            .children(self.unlisted.clone().map(|view| {
                small_button(
                    ElementId::Name(format!("{name}/view_file").into()),
                    "View file",
                    &theme,
                )
                .on_click(move |_, window, cx| view(window, cx))
                .automation_target(format!("{name}/view_file"))
                .into_any_element()
            }))
            .children(self.resolve_button(&name, &theme))
            .children(self.on_open.clone().map(|open| {
                small_button(
                    ElementId::Name(format!("{name}/open").into()),
                    "Open on GitHub",
                    &theme,
                )
                .on_click(move |_, window, cx| open(window, cx))
                .automation_target(format!("{name}/open"))
                .into_any_element()
            }))
            .when(thread.resolved, |this| {
                let toggle = self.on_toggle.clone();
                let hover = c.control_hover;
                this.child(
                    div()
                        .id(ElementId::Name(format!("{name}/toggle").into()))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(theme.sizes.control_height)
                        .rounded(theme.radii.control)
                        .cursor_pointer()
                        .hover(move |style| style.bg(hover))
                        .child(icon(
                            if self.collapsed {
                                DvIcon::ChevronRight
                            } else {
                                DvIcon::ChevronDown
                            },
                            px(14.),
                            c.fg2,
                        ))
                        .on_click(move |_, window, cx| {
                            if let Some(toggle) = &toggle {
                                toggle(window, cx);
                            }
                        })
                        .automation_target(format!("{name}/toggle")),
                )
            });

        let card = div()
            .w_full()
            .flex()
            .flex_col()
            .rounded(theme.radii.card)
            .border_1()
            .border_color(if self.focused { c.accent_ring } else { c.line2 })
            .bg(c.thread)
            .shadow(theme.shadows.card.clone())
            .overflow_hidden()
            .text_spec(&theme.text.body)
            .text_color(c.fg)
            .when_some(self.on_focus.clone(), |this, focus| {
                this.on_mouse_down(MouseButton::Left, move |_, window, cx| focus(window, cx))
            })
            .child(header)
            .children(self.error.as_ref().map(|error| {
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.sm)
                    .px(theme.spacing.lg)
                    .py(theme.spacing.sm)
                    .border_b_1()
                    .border_color(c.line)
                    .text_spec(&theme.text.secondary)
                    .text_color(c.red)
                    .child(icon(DvIcon::TriangleAlert, px(13.), c.red))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(one_line(&error.message).into_owned()),
                    )
                    .automation_target(format!("{name}/error"))
            }));
        if self.collapsed {
            return card.automation_target(name);
        }

        let comments: Vec<AnyElement> = thread
            .comments
            .iter()
            .enumerate()
            .map(|(index, comment)| self.comment(&name, comment, index == 0, &theme, cx))
            .collect();
        let notice = self.notice.map(|notice| {
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .pt(theme.spacing.sm)
                .pr(theme.spacing.lg)
                .pl(px(44.))
                .text_spec(&theme.text.secondary)
                .text_color(c.fg3)
                .child(div().flex_1().min_w_0().child(notice.text()))
                .automation_target(format!("{name}/{}", notice.target()))
        });
        card.children(self.code(&theme))
            .children(comments)
            .children(notice)
            .child(div().h(px(10.)))
            .automation_target(name)
    }
}

impl GithubCard {
    /// Why the thread has no line in the diff shown, for its yellow chip (DESIGN §4.5):
    /// "Outdated on GitHub" when GitHub calls it outdated and it was made on another commit
    /// than the review shows; "Can't be placed" when dv can't tell which code it is on
    /// (`Standing::Unplaced`); "Outdated", as on a thread of the review's own, when dv read
    /// its code and can't find it in the head it shows.
    fn standing_chip(&self) -> Option<&'static str> {
        match self.shown.standing {
            Standing::OutdatedOnGithub => Some("Outdated on GitHub"),
            Standing::Unplaced => Some("Can't be placed"),
            Standing::Anchored => self.outdated.then_some("Outdated"),
        }
    }

    /// Resolve or Unresolve, where GitHub allows the viewer: with `X` beside it on the focused
    /// card, and saying so, inert, while the call runs.
    fn resolve_button(&self, name: &str, theme: &DvTheme) -> Option<AnyElement> {
        let thread = &self.shown.thread;
        let resolve = self.resolving.unwrap_or(!thread.resolved);
        let allowed = if thread.resolved {
            thread.viewer_can_unresolve
        } else {
            thread.viewer_can_resolve
        };
        if !allowed && self.resolving.is_none() {
            return None;
        }
        let key = if resolve { "resolve" } else { "unresolve" };
        let label = match (self.resolving, resolve) {
            (Some(_), true) => "Resolving…",
            (Some(_), false) => "Unresolving…",
            (None, true) => "Resolve",
            (None, false) => "Unresolve",
        };
        let button = small_button(
            ElementId::Name(format!("{name}/{key}").into()),
            label,
            theme,
        )
        .when(self.focused && self.resolving.is_none(), |this| {
            this.child(kbd(keymap::label("x"), theme))
        });
        let button = match (&self.on_resolve, self.resolving) {
            (Some(on_resolve), None) => {
                let on_resolve = on_resolve.clone();
                button.on_click(move |_, window, cx| on_resolve(window, cx))
            }
            _ => button.opacity(0.5).cursor_default(),
        };
        Some(
            button
                .automation_target(format!("{name}/{key}"))
                .into_any_element(),
        )
    }

    /// The code the thread is on, where the stream doesn't show it under the card: the end
    /// of its diff hunk as GitHub shows it, for one without a place in the diff; else, for
    /// one dv can't find or one on a file the review doesn't change, its lines.
    fn code(&self, theme: &DvTheme) -> Option<AnyElement> {
        let c = &theme.colors;
        let anchor = &self.shown.anchor;
        let block = || {
            div()
                .mx(theme.spacing.lg)
                .mt(px(10.))
                .py(theme.spacing.sm)
                .rounded(theme.radii.control)
                .bg(c.recess)
                .font_family(theme.fonts.mono.clone())
                // Code shows the characters as typed: no ligatures.
                .font_features(FontFeatures::disable_ligatures())
                .text_size(px(11.5))
                .line_height(px(17.))
                .text_color(c.fg2)
        };
        if self.shown.standing != Standing::Anchored {
            let hunk = self
                .shown
                .thread
                .comments
                .first()
                .map_or("", |comment| comment.diff_hunk.as_str());
            let count = anchor.end_line.saturating_sub(anchor.start_line) + 1;
            let rows = hunk_rows(hunk, anchor.side, count);
            if rows.is_empty() {
                return None;
            }
            return Some(
                block()
                    .children(rows.into_iter().map(|row| {
                        let (mark, color) = match row.kind {
                            LineKind::Added => ("+", c.green),
                            LineKind::Removed => ("−", c.red),
                            LineKind::Context => (" ", c.fg3),
                        };
                        div()
                            .flex()
                            .when(row.commented, |this| this.bg(c.sel))
                            // The commented lines wear the accent bar, as the cursor's line.
                            .child(div().flex_none().w(px(2.)).bg(if row.commented {
                                c.accent
                            } else {
                                gpui::transparent_black()
                            }))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(18.))
                                    .text_center()
                                    .text_color(color)
                                    .child(mark),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .child(one_line(&row.text.replace('\t', "    ")).into_owned()),
                            )
                    }))
                    .into_any_element(),
            );
        }
        ((self.outdated || self.unlisted.is_some()) && !anchor.text.is_empty()).then(|| {
            block()
                .px(px(10.))
                .children(anchor.text.iter().map(|line| {
                    div()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(one_line(&line.replace('\t', "    ")).into_owned())
                }))
                .into_any_element()
        })
    }

    /// One of the thread's comments: GitHub's mark, the login, "on GitHub", "Pending on
    /// GitHub" for the viewer's own that isn't submitted, the time and the body as Markdown.
    fn comment(
        &self,
        name: &str,
        comment: &GithubComment,
        first: bool,
        theme: &DvTheme,
        cx: &App,
    ) -> AnyElement {
        let c = &theme.colors;
        let avatar = div()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(px(22.))
            .rounded_full()
            .bg(c.layer_strong)
            .child(icon(DvIcon::Github, px(13.), c.fg2))
            .into_any_element();
        let mut tags = vec![tag("on GitHub", theme)];
        if comment.is_pending() {
            tags.push(tag("Pending on GitHub", theme).text_color(c.yellow));
        }
        comment_frame(
            ElementId::Name(format!("{name}/{}", comment.id).into()),
            avatar,
            one_line(&comment.author).into_owned(),
            tags,
            // GitHub shows a line break inside a paragraph as one.
            &crate::markdown::with_line_breaks(&comment.body),
            (comment.created_at, self.now),
            first,
            theme,
            cx,
        )
        .into_any_element()
    }
}
