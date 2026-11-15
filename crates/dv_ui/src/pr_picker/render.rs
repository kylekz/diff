//! The PR picker's panel (DESIGN §4.5, the switcher's look: `overlay` over the scrim, radius
//! 12, the overlay shadow): the field, the repository's line, the rows or what stands in
//! their place, and a footer with the keys.
//!
//! Everything a row draws of a pull request is GitHub's text (a title, a login): each is
//! drawn as one line whatever it holds (`crate::thread::one_line`) and truncates, so the
//! chips and the time at the row's end keep their place.

use dv_core::github::ListedPr;
use gpui::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::Input;

use super::{ListState, PICKER_WIDTH, PrPicker, Row, RowKind, listed, repo_title};
use crate::automation::AutomationTargetExt as _;
use crate::components::{chip, icon, kbd, skeleton, tooltip};
use crate::icons::DvIcon;
use crate::keymap;
use crate::services::github::ListFailureKind;
use crate::theme::{ActiveTheme as _, DvTheme, StyledText as _};
use crate::thread::one_line;

/// A row's height.
const ROW_HEIGHT: f32 = 30.;

impl Render for PrPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.dv_theme().clone();
        let c = &theme.colors;
        let viewport = window.viewport_size();
        let max_height = (viewport.height - theme.sizes.title_bar_height - px(56.)).max(px(200.));
        div()
            .key_context(keymap::PR_PICKER)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .w(PICKER_WIDTH)
            .max_h(max_height)
            .flex()
            .flex_col()
            .rounded(theme.radii.sheet)
            .border_1()
            .border_color(c.overlay_line)
            .bg(c.overlay)
            .shadow(theme.shadows.overlay.clone())
            .overflow_hidden()
            .occlude()
            .text_spec(&theme.text.body)
            .text_color(c.fg)
            .child(self.render_field(&theme))
            .children(self.render_repo(&theme, cx))
            .child(self.render_list(&theme, cx))
            .child(render_footer(&theme))
            .automation_target("pr_picker")
    }
}

impl PrPicker {
    fn render_field(&self, theme: &DvTheme) -> AnyElement {
        let c = &theme.colors;
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(46.))
            .px(px(14.))
            .border_b_1()
            .border_color(c.line)
            .child(icon(DvIcon::GitPullRequest, px(15.), c.fg3))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.input)
                        .appearance(false)
                        .aria_label(super::PLACEHOLDER)
                        .px_0()
                        .text_size(px(14.)),
                ),
            )
            .automation_target("pr_picker/input")
    }

    /// The repository in view over its list: GitHub's mark, its name, how many are open, and
    /// at the right that the list is being read again; under it, why the last read failed
    /// while an earlier list still shows.
    fn render_repo(&self, theme: &DvTheme, cx: &Context<Self>) -> Option<AnyElement> {
        let c = &theme.colors;
        // Not over another repository's rows (`PrPicker::names_another_repository`).
        let location = self
            .location
            .as_ref()
            .filter(|_| !self.names_another_repository())?;
        let list = self.list(cx);
        let count = listed(list).map(|prs| prs.len());
        let refreshing = list.is_some_and(|list| list.loading) && count.is_some();
        let stale = list
            .and_then(|list| list.failure.as_ref())
            .filter(|_| count.is_some());
        let line = div()
            .flex()
            .items_center()
            .gap(theme.spacing.sm)
            .h(px(28.))
            .px(theme.spacing.lg)
            .text_spec(&theme.text.secondary)
            .text_color(c.fg3)
            // GitHub's mark once a read told which repository on GitHub it is.
            .child(icon(
                if list.is_some_and(|list| list.repo.is_some()) {
                    DvIcon::Github
                } else {
                    DvIcon::Folder
                },
                px(13.),
                c.fg3,
            ))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.fg2)
                    .child(SharedString::from(repo_title(location, list))),
            )
            .children(count.map(|count| {
                div().flex_none().child(match count {
                    1 => "1 open pull request".to_string(),
                    n => format!("{n} open pull requests"),
                })
            }))
            .child(div().flex_1())
            .when(refreshing, |this| {
                this.child(div().flex_none().child("Reading the list again…"))
            })
            .automation_target("pr_picker/repo");
        let stale = stale.map(|failure| {
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.sm)
                .min_h(px(24.))
                .px(theme.spacing.lg)
                .pb(theme.spacing.xs)
                .text_spec(&theme.text.secondary)
                .text_color(c.yellow)
                .child(icon(DvIcon::TriangleAlert, px(12.), c.yellow))
                .child(div().min_w_0().truncate().child(SharedString::from(format!(
                    "Showing the list read before: {}",
                    one_line(&failure.failure.message)
                ))))
                .automation_target("pr_picker/stale")
        });
        // The marks couldn't be read: the list is whole, and nothing in it says who asks.
        let unmarked = list
            .and_then(|list| list.unmarked.as_ref())
            .filter(|_| count.is_some())
            .map(|failure| {
                div()
                    .id("pr-picker-unmarked")
                    .flex()
                    .items_center()
                    .min_h(px(22.))
                    .px(theme.spacing.lg)
                    .pb(theme.spacing.xs)
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg3)
                    .child(div().min_w_0().truncate().child(super::UNMARKED))
                    .tooltip(tooltip(one_line(&failure.message).into_owned()))
                    .automation_target("pr_picker/unmarked")
            });
        Some(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .pt(theme.spacing.xs)
                .child(line)
                .children(stale)
                .children(unmarked)
                .into_any_element(),
        )
    }

    fn render_list(&self, theme: &DvTheme, cx: &mut Context<Self>) -> AnyElement {
        let list = div()
            .id("pr-picker-list")
            .track_scroll(&self.scroll)
            .flex_shrink(1.)
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .px(theme.spacing.sm)
            .pb(theme.spacing.sm);
        let now = dv_core::review::event::now_ms();
        let mut lines: Vec<AnyElement> = Vec::with_capacity(self.rows.len() + 2);
        for (ix, row) in self.rows.iter().enumerate() {
            lines.push(self.render_row(ix, row, now, theme, cx));
            // Why the folder chosen wasn't taken, under the row that asked for it.
            if matches!(row.kind, RowKind::ChooseFolder { .. })
                && let Some(notice) = &self.notice
            {
                lines.push(notice_line(notice, theme).automation_target("pr_picker/notice"));
            }
        }
        // What stands in the list's place is about the repository in view's list: not
        // drawn under another repository's rows.
        let state = (!self.names_another_repository())
            .then(|| self.render_state(theme))
            .flatten();
        list.children(lines).children(state).into_any_element()
    }

    /// What stands where the list goes (`ListState`), under the field's own rows.
    fn render_state(&self, theme: &DvTheme) -> Option<AnyElement> {
        let c = &theme.colors;
        let (heading, note): (String, String) = match &self.state {
            ListState::Listed => return None,
            ListState::Loading => {
                return Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .px(theme.spacing.md)
                        .py(theme.spacing.md)
                        .children(
                            [300., 380., 240., 340.]
                                .into_iter()
                                .map(|width| skeleton(px(width), px(10.), theme)),
                        )
                        .automation_target("pr_picker/state"),
                );
            }
            ListState::NoRepository => (
                "No repository is open".to_string(),
                format!(
                    "Open one with {}, or paste a pull request's URL: dv finds its clone.",
                    keymap::label("secondary-o")
                ),
            ),
            ListState::Empty => (
                "No open pull requests".to_string(),
                "A closed or merged one opens by its number or URL.".to_string(),
            ),
            ListState::NoMatch => (
                format!("No open pull request matches “{}”", self.query.trim()),
                "Every word must be in its number, title or author.".to_string(),
            ),
            ListState::Failed(failure) => (
                match failure.kind {
                    ListFailureKind::NotOnGithub => "This repository isn't on GitHub",
                    ListFailureKind::GhMissing => "GitHub's CLI isn't installed",
                    ListFailureKind::SignedOut => "Not signed in to GitHub",
                    ListFailureKind::Unreachable => "The repository can't be reached",
                    ListFailureKind::Other => "Couldn't list the pull requests",
                }
                .to_string(),
                one_line(&failure.failure.message).into_owned(),
            ),
        };
        Some(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(theme.spacing.xs)
                .px(theme.spacing.xl)
                .py(theme.spacing.xl)
                .child(div().text_color(c.fg2).child(SharedString::from(heading)))
                .child(
                    div()
                        .max_w(px(460.))
                        .text_center()
                        .text_spec(&theme.text.secondary)
                        .text_color(c.fg3)
                        .child(SharedString::from(note)),
                )
                .automation_target("pr_picker/state"),
        )
    }

    fn render_row(
        &self,
        ix: usize,
        row: &Row,
        now: u64,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let selected = ix == self.selected;
        let hover = c.control_hover;
        let glyph_color = if selected { c.fg } else { c.fg2 };
        let line = div()
            .id(ElementId::NamedInteger("pr-picker-row".into(), ix as u64))
            .debug_selector({
                let key = row.key.clone();
                move || format!("pr-picker-row-{key}")
            })
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .h(px(ROW_HEIGHT))
            .px(theme.spacing.md)
            .rounded(theme.radii.control)
            .cursor_pointer()
            .map(|this| {
                if selected {
                    this.bg(c.sel)
                } else {
                    this.hover(move |style| style.bg(hover))
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| this.pick(ix, cx)));
        let line = match &row.kind {
            RowKind::Reference {
                name, title, clone, ..
            } => line
                .child(icon(DvIcon::Github, px(14.), glyph_color))
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::MEDIUM)
                        .child("Open pull request"),
                )
                .child(
                    div()
                        .flex_none()
                        .max_w(px(220.))
                        .truncate()
                        .font_family(theme.fonts.mono.clone())
                        .text_size(px(12.))
                        .text_color(c.fg2)
                        .child(SharedString::from(name.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(c.fg2)
                        .children(title.clone().map(SharedString::from)),
                )
                .children(clone.clone().map(|clone| {
                    div()
                        .flex_none()
                        .max_w(px(140.))
                        .truncate()
                        .text_spec(&theme.text.secondary)
                        .text_color(c.fg3)
                        .child(SharedString::from(clone))
                })),
            RowKind::ChooseFolder { repository, .. } => line
                .child(icon(DvIcon::Folder, px(14.), glyph_color))
                .child(div().min_w_0().truncate().child(SharedString::from(format!(
                    "{repository} isn't open in dv yet"
                ))))
                .child(div().flex_1())
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(c.accent_text)
                        .child("Choose its folder"),
                ),
            RowKind::Listed { pr, .. } => listed_row(line, pr, now, theme),
        };
        line.automation_target(format!("pr_picker/row/{}", row.key))
    }
}

/// One of the list's pull requests: `#482` in mono `fg3`, the title, the author in `fg3`, a
/// `draft` chip, "Review requested" in the accent, and when it last changed.
fn listed_row(
    line: gpui::Stateful<gpui::Div>,
    pr: &ListedPr,
    now: u64,
    theme: &DvTheme,
) -> gpui::Stateful<gpui::Div> {
    let c = &theme.colors;
    line.child(
        div()
            .flex_none()
            .min_w(px(40.))
            .font_family(theme.fonts.mono.clone())
            .text_size(px(12.))
            .text_color(c.fg3)
            .child(format!("#{}", pr.number)),
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .child(SharedString::from(pr.title.clone())),
    )
    .child(
        div()
            .flex_none()
            .max_w(px(120.))
            .truncate()
            .text_spec(&theme.text.secondary)
            .text_color(c.fg3)
            .child(SharedString::from(pr.author.clone())),
    )
    .when(pr.draft, |this| {
        this.child(
            chip(c.fg2, c.ctrl, theme)
                .border_1()
                .border_color(c.ctrl_line)
                .child("draft"),
        )
    })
    .when(pr.review_requested, |this| {
        this.child(chip(c.accent_text, c.accent_soft, theme).child("Review requested"))
    })
    .children(pr.updated_at.map(|at| {
        div()
            .flex_none()
            .text_right()
            .text_spec(&theme.text.secondary)
            .text_color(c.fg3)
            .child(crate::thread::relative_time(at, now))
    }))
}

/// Why the folder chosen for a pull request wasn't taken: in `red` under its row, wrapping
/// rather than cutting what it says (a long path comes before the reason). The caller names
/// its automation target.
pub(crate) fn notice_line(notice: &str, theme: &DvTheme) -> gpui::Div {
    let c = &theme.colors;
    div()
        .flex_none()
        .flex()
        .items_start()
        .gap(theme.spacing.sm)
        .py(px(3.))
        .pl(px(34.))
        .pr(theme.spacing.md)
        .text_spec(&theme.text.secondary)
        .text_color(c.red)
        .child(
            div()
                .flex_none()
                .h(theme.text.secondary.line_height)
                .flex()
                .items_center()
                .child(icon(DvIcon::TriangleAlert, px(12.), c.red)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(SharedString::from(one_line(notice).into_owned())),
        )
}

/// The keys: `↑` `↓` move, `Enter` open, `Esc` close.
fn render_footer(theme: &DvTheme) -> AnyElement {
    let c = &theme.colors;
    let hint = |keys: Vec<&'static str>, what: &'static str| {
        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .children(keys.into_iter().map(|key| kbd(keymap::label(key), theme)))
            .child(what)
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.spacing.lg)
        .h(px(40.))
        .px(theme.spacing.lg)
        .border_t_1()
        .border_color(c.line)
        .text_spec(&theme.text.secondary)
        .text_color(c.fg3)
        .child(div().flex_1())
        .child(hint(vec!["up", "down"], "move"))
        .child(hint(vec!["enter"], "open"))
        .child(hint(vec!["escape"], "close"))
        .into_any_element()
}
