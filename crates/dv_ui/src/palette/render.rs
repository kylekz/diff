//! The palette's panel (DESIGN §4.1 "Palette", the file finder's look: `overlay` over the
//! scrim, radius 12, the overlay shadow): the filter field, the rows under their sections'
//! labels, and a footer with the keys.

use gpui::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::Input;

use super::{PALETTE_WIDTH, Palette, Row, RowKind};
use crate::automation::AutomationTargetExt as _;
use crate::components::{icon, kbd, status_chip};
use crate::file_finder::render::path_text;
use crate::file_list::status_letter;
use crate::icons::DvIcon;
use crate::keymap;
use crate::switcher::items::TargetKind;
use crate::theme::{ActiveTheme as _, DvTheme, StyledText as _};

/// A row's height.
const ROW_HEIGHT: f32 = 30.;

impl Render for Palette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.dv_theme().clone();
        let c = &theme.colors;
        let viewport = window.viewport_size();
        let max_height = (viewport.height - theme.sizes.title_bar_height - px(56.)).max(px(200.));
        div()
            .key_context(keymap::PALETTE)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .w(PALETTE_WIDTH)
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
            .child(self.render_filter(&theme))
            .child(self.render_list(&theme, cx))
            .child(render_footer(&theme))
            .automation_target("palette")
    }
}

impl Palette {
    fn render_filter(&self, theme: &DvTheme) -> AnyElement {
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
            .child(icon(DvIcon::Search, px(15.), c.fg3))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.input)
                        .appearance(false)
                        .aria_label(super::PLACEHOLDER)
                        .px_0()
                        .text_size(px(14.)),
                ),
            )
            .automation_target("palette/input")
    }

    fn render_list(&self, theme: &DvTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let list = div()
            .id("palette-list")
            .track_scroll(&self.scroll)
            .flex_shrink(1.)
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .p(theme.spacing.sm);
        if self.rows.is_empty() {
            return list
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(theme.spacing.xs)
                        .py(theme.spacing.xxl)
                        .child(
                            div()
                                .text_color(c.fg2)
                                .child(format!("Nothing matches “{}”", self.query.trim())),
                        )
                        .child(
                            div()
                                .text_spec(&theme.text.secondary)
                                .text_color(c.fg3)
                                .child(
                                    "Every word must be in a command, a review or a file's path.",
                                ),
                        ),
                )
                .automation_target("palette/empty");
        }
        let mut lines: Vec<AnyElement> = Vec::with_capacity(self.rows.len() + 3);
        for (ix, row) in self.rows.iter().enumerate() {
            if ix == 0 || self.rows[ix - 1].section != row.section {
                lines.push(section_label(row.section.title(), ix == 0, theme));
            }
            lines.push(self.render_row(ix, row, theme, cx));
            // Why the folder chosen wasn't taken, under the row that asked for it.
            if matches!(row.kind, RowKind::ChooseFolder { .. })
                && let Some(notice) = &self.notice
            {
                lines.push(
                    crate::pr_picker::notice_line(notice, theme)
                        .automation_target("palette/notice"),
                );
            }
        }
        list.children(lines).into_any_element()
    }

    fn render_row(
        &self,
        ix: usize,
        row: &Row,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let selected = ix == self.selected;
        let hover = c.control_hover;
        let glyph_color = if selected { c.fg } else { c.fg2 };
        let line = div()
            .id(ElementId::NamedInteger("palette-row".into(), ix as u64))
            .debug_selector({
                let key = row.key.clone();
                move || format!("palette-row-{key}")
            })
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
            // A pull request's name and title are its author's text: one line each, and the
            // title truncates.
            RowKind::Pr {
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
            RowKind::Command(command) => line
                .child(icon(DvIcon::ChevronRight, px(14.), glyph_color))
                .child(div().min_w_0().truncate().child(command.title))
                .child(div().flex_1())
                .children(command.keys().map(|keys| kbd(keys, theme))),
            RowKind::Review {
                repo,
                label,
                target,
                badge,
                ..
            } => {
                let glyph = match target {
                    Some(TargetKind::Working) => DvIcon::FileDiff,
                    Some(TargetKind::Staged) => DvIcon::Package,
                    Some(TargetKind::Branch) => DvIcon::GitBranch,
                    None => DvIcon::GitCommit,
                };
                line.child(icon(glyph, px(14.), glyph_color))
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(200.))
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(SharedString::from(repo.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(c.fg2)
                            .child(SharedString::from(label.clone())),
                    )
                    .when_some(badge.as_ref(), |this, badge| {
                        this.child(status_chip(badge.status, theme)).child(
                            div()
                                .flex_none()
                                .text_spec(&theme.text.secondary)
                                .text_color(c.fg3)
                                .child(format!("round {}", badge.round)),
                        )
                    })
            }
            RowKind::File { path, status } => {
                let letter = status.map(|status| {
                    let letter = status_letter(status);
                    let (fg, _) = letter.colors(theme);
                    div()
                        .text_spec(&theme.text.secondary)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(fg)
                        .child(letter.letter)
                });
                line.child(icon(DvIcon::File, px(14.), glyph_color))
                    .child(div().flex_none().w(px(14.)).children(letter))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis_start()
                            .child(path_text(path, &self.query, theme)),
                    )
            }
        };
        line.automation_target(format!("palette/row/{}", row.key))
    }
}

/// A section's label over its rows: 12/600 in `fg3`.
fn section_label(title: &'static str, first: bool, theme: &DvTheme) -> AnyElement {
    div()
        .flex()
        .items_end()
        .h(px(if first { 22. } else { 30. }))
        .pb(px(4.))
        .px(theme.spacing.md)
        .text_spec(&theme.text.secondary)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.colors.fg3)
        .child(title)
        .into_any_element()
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
