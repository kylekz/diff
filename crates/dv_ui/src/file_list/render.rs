//! The file list's layout (DESIGN §4.1 "Panel", the canvas's `GlassReview` panel): the header
//! with totals, progress and the filter, then the rows.

use std::ops::Range;
use std::time::Instant;

use dv_core::diff::FileEntry;
use gpui::{
    AnyElement, Context, ElementId, FontWeight, HighlightStyle, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, Window, div,
    prelude::FluentBuilder as _, px, relative, uniform_list,
};
use gpui_component::input::Input;

use super::{FileList, SKELETON_FOR, split_path, status_letter};
use crate::automation::AutomationTargetExt as _;
use crate::components::{checkbox, count_badge, icon, kbd, skeleton, totals};
use crate::icons::DvIcon;
use crate::keymap;
use crate::services::ActiveServices as _;
use crate::session::{Comparison, ReviewSession};
use crate::theme::{ActiveTheme as _, DvTheme, StyledText as _};

/// The narrowest panel (DESIGN §2: 220–420) whose comparison control has room for "Since
/// round n" and "All changes" in full beside its keycap.
pub(super) const COMPARISON_LABEL_FROM: f32 = 270.;

impl Render for FileList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.dv_theme().clone();
        let body = self
            .session
            .clone()
            .map(|session| self.render_body(&session, &theme, cx));
        div()
            .key_context(keymap::FILE_LIST)
            .on_action(cx.listener(Self::reveal_first))
            .on_action(cx.listener(Self::clear_filter))
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.render_header(&theme, cx))
            .children(body)
    }
}

impl FileList {
    fn render_header(&self, theme: &DvTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let title = div()
            .text_spec(&theme.text.title)
            .text_color(c.fg)
            .child("Changes");
        let header = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(9.))
            .pt(theme.spacing.lg)
            .px(theme.spacing.lg)
            .pb(px(10.));
        let Some(session) = &self.session else {
            return header
                .gap(theme.spacing.xs)
                .child(title)
                .child(
                    div()
                        .text_spec(&theme.text.secondary)
                        .text_color(c.fg3)
                        .child("No review open"),
                )
                .automation_target("shell/panel/header");
        };
        let toggle = self.render_comparison(session, theme, cx);
        let session = session.read(cx);
        let files = session.files();
        let (total, viewed) = (files.len(), session.viewed_count());
        let additions: u64 = files.iter().map(|entry| u64::from(entry.additions)).sum();
        let deletions: u64 = files.iter().map(|entry| u64::from(entry.deletions)).sum();
        let listed = !session.is_first_load();
        let fraction = if total == 0 {
            0.
        } else {
            viewed as f32 / total as f32
        };
        header
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .child(title)
                    .when(listed, |this| {
                        this.child(
                            div()
                                .text_color(c.fg3)
                                .child(count_label(total, "file", "files")),
                        )
                        .child(div().flex_1())
                        .child(totals(
                            additions,
                            deletions,
                            px(12.),
                            theme,
                        ))
                    }),
            )
            .children(toggle)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .child(
                        div()
                            .flex_1()
                            .h(px(4.))
                            .rounded(px(2.))
                            .bg(c.track)
                            .overflow_hidden()
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(fraction))
                                    .rounded(px(2.))
                                    .bg(c.accent),
                            ),
                    )
                    .child(
                        div()
                            .text_spec(&theme.text.secondary)
                            .text_color(c.fg2)
                            .child(format!("{viewed} of {total} viewed")),
                    )
                    .automation_target("file_list/progress"),
            )
            .child(self.render_filter(theme))
            .automation_target("shell/panel/header")
    }

    /// "Since round n | All changes" with its `I` keycap (DESIGN §4.1), while the review offers
    /// the toggle; the comparison asked for is the chosen segment at once. Below
    /// [`COMPARISON_LABEL_FROM`] the second segment reads "All".
    fn render_comparison(
        &self,
        session: &gpui::Entity<ReviewSession>,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (asked, last) = {
            let session = session.read(cx);
            let last = session.review().last_round().map(|round| round.n);
            (
                session.asked_comparison(),
                last.filter(|_| session.comparison_available())?,
            )
        };
        // In a narrow panel "All changes" says "All", so "Since round n" keeps its round.
        let (since, all) = comparison_labels(last, cx.services().settings.read(cx).panel_width());
        let segment = |key: &'static str, label: String, comparison: Comparison| {
            let on = match comparison {
                Comparison::All => asked == Comparison::All,
                Comparison::SinceRound(_) => asked.is_interdiff(),
            };
            let session = session.clone();
            // Each segment starts at its label's width and shares what room is left.
            crate::components::segment(
                ElementId::Name(format!("comparison-{key}").into()),
                on,
                true,
                theme,
            )
            .flex_auto()
            .min_w_0()
            .justify_center()
            .px(theme.spacing.sm)
            .child(div().min_w_0().truncate().child(label))
            .on_click(move |_, _, cx| {
                session.update(cx, |session, cx| session.set_comparison(comparison, cx))
            })
            .debug_selector(move || format!("comparison-{key}"))
            .automation_target(format!("file_list/comparison/{key}"))
        };
        Some(
            crate::components::segmented(theme)
                .child(segment("since", since, Comparison::SinceRound(last)))
                .child(segment("all", all, Comparison::All))
                .child(
                    div()
                        .flex_none()
                        .ml(px(2.))
                        .mr(px(4.))
                        .child(kbd(keymap::label("i"), theme)),
                )
                .automation_target("file_list/comparison"),
        )
    }

    fn render_filter(&self, theme: &DvTheme) -> AnyElement {
        let c = &theme.colors;
        div()
            .relative()
            .flex()
            .items_center()
            .h(theme.sizes.control_height)
            .pl(px(28.))
            .pr(theme.spacing.md)
            .rounded(theme.radii.control)
            .border_1()
            .border_color(c.ctrl_line)
            .bg(c.input_bg)
            .child(div().absolute().left(px(8.)).top(px(6.)).child(icon(
                DvIcon::ListFilter,
                px(14.),
                c.fg3,
            )))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.filter)
                        .appearance(false)
                        .aria_label("Filter files")
                        .px_0()
                        .text_size(px(13.)),
                ),
            )
            .automation_target("file_list/filter")
    }

    fn render_body(
        &self,
        session: &gpui::Entity<ReviewSession>,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let (loading, empty, failed) = {
            let session = session.read(cx);
            (
                session.is_first_load(),
                session.files().is_empty(),
                session.error().is_some() && session.files().is_empty(),
            )
        };
        let quiet = |text: String| {
            div()
                .px(theme.spacing.md)
                .py(theme.spacing.sm)
                .text_spec(&theme.text.secondary)
                .text_color(c.fg3)
                .child(text)
                .automation_target("file_list/note")
        };
        let body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .px(theme.spacing.sm);
        if loading {
            let skeletons = self
                .loading_since
                .is_none_or(|since| Instant::now().duration_since(since) < SKELETON_FOR);
            return if skeletons {
                body.children((0..6).map(|ix| skeleton_row(ix, theme)))
                    .automation_target("file_list/loading")
            } else {
                body.child(quiet("Listing changes…".into()))
                    .automation_target("file_list/loading")
            };
        }
        if failed {
            return body
                .child(quiet("The changes couldn't be listed.".into()))
                .into_any_element();
        }
        if empty {
            return body
                .child(quiet("No changed files.".into()))
                .into_any_element();
        }
        if self.shown.is_empty() {
            return body
                .child(quiet(format!("No files match “{}”.", self.query.trim())))
                .into_any_element();
        }
        let theme = theme.clone();
        body.child(
            uniform_list(
                "file-list-rows",
                self.shown.len(),
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    this.render_rows(range, &theme, cx)
                }),
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .min_h_0(),
        )
        .into_any_element()
    }

    fn render_rows(
        &self,
        range: Range<usize>,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(session) = self.session.clone() else {
            return Vec::new();
        };
        let current = self.diff_pane.read(cx).current_file().map(str::to_string);
        let rows: Vec<(usize, FileEntry, RowState)> = {
            let session = session.read(cx);
            let read_only = session.is_read_only();
            range
                .filter_map(|row| {
                    let entry = session.files().get(*self.shown.get(row)?)?;
                    let state = RowState {
                        current: current.as_deref() == Some(entry.path.as_str()),
                        viewed: session.is_viewed(&entry.path),
                        open_threads: self.open_threads.get(&entry.path).copied().unwrap_or(0),
                        read_only,
                    };
                    Some((row, entry.clone(), state))
                })
                .collect()
        };
        rows.into_iter()
            .map(|(row, entry, state)| file_row(row, &entry, state, theme, cx))
            .collect()
    }
}

/// What a row shows beside its file.
#[derive(Clone, Copy)]
struct RowState {
    current: bool,
    viewed: bool,
    open_threads: usize,
    read_only: bool,
}

/// One file's row (the canvas's file rows: 28 high, radius 6, `sel` when current).
fn file_row(
    row: usize,
    entry: &FileEntry,
    state: RowState,
    theme: &DvTheme,
    cx: &mut Context<FileList>,
) -> AnyElement {
    let c = &theme.colors;
    let hover = c.control_hover;
    let letter = status_letter(entry.status);
    let (letter_fg, letter_bg) = letter.colors(theme);
    let path = entry.path.clone();
    let (dir, name) = split_path(&entry.path);
    let name_color = if state.viewed { c.fg3 } else { c.fg };
    // The element takes the name's weight: truncation measures every character in it, so the
    // heaviest run's keeps the current file's name from overflowing (DESIGN §4.1).
    let name_weight = if state.current {
        FontWeight::SEMIBOLD
    } else {
        FontWeight::NORMAL
    };
    let text = StyledText::new(SharedString::from(entry.path.clone())).with_highlights([
        (
            0..dir.len(),
            HighlightStyle {
                color: Some(c.fg3),
                font_weight: Some(FontWeight::NORMAL),
                ..Default::default()
            },
        ),
        (
            dir.len()..dir.len() + name.len(),
            HighlightStyle {
                color: Some(name_color),
                ..Default::default()
            },
        ),
    ]);
    let toggle_path = path.clone();
    let viewed_target = format!("file_list/{path}/viewed");
    let check = div()
        .id(ElementId::NamedInteger("file-viewed".into(), row as u64))
        .flex_none()
        .child(checkbox(state.viewed, theme))
        .when(state.read_only, |this| this.opacity(0.5))
        .when(!state.read_only, |this| this.cursor_pointer())
        // The row's own click must not also reveal the file.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.toggle_viewed(&toggle_path, cx);
        }))
        .automation_target(viewed_target);
    div()
        .id(ElementId::NamedInteger("file-row".into(), row as u64))
        .w_full()
        .flex()
        .items_center()
        .gap(px(7.))
        .h(theme.sizes.file_row_height)
        .pl(theme.spacing.sm)
        .pr(theme.spacing.md)
        .rounded(theme.radii.control)
        .cursor_pointer()
        .map(|this| {
            if state.current {
                this.bg(c.sel)
            } else {
                this.hover(move |style| style.bg(hover))
            }
        })
        .on_click(cx.listener(move |this, _, _, cx| this.reveal(&path, cx)))
        .child(check)
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(16.))
                .rounded(theme.radii.small)
                .bg(letter_bg)
                .text_color(letter_fg)
                .font_family(theme.fonts.mono.clone())
                .text_size(px(10.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(letter.letter),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis_start()
                .text_spec(&theme.text.body)
                .font_weight(name_weight)
                .child(text),
        )
        .when(state.open_threads > 0, |this| {
            this.child(
                count_badge(theme)
                    .child(icon(DvIcon::MessageSquare, px(10.), c.accent_text))
                    .child(state.open_threads.to_string()),
            )
        })
        .child(totals(
            u64::from(entry.additions),
            u64::from(entry.deletions),
            px(11.5),
            theme,
        ))
        .automation_target(format!("file_list/{}", entry.path))
}

/// A placeholder row while the list loads.
fn skeleton_row(ix: usize, theme: &DvTheme) -> AnyElement {
    let width = [150., 190., 120., 170., 140., 100.][ix % 6];
    div()
        .flex()
        .items_center()
        .gap(px(7.))
        .h(theme.sizes.file_row_height)
        .pl(theme.spacing.sm)
        .child(skeleton(px(16.), px(16.), theme))
        .child(skeleton(px(16.), px(16.), theme))
        .child(skeleton(px(width), px(10.), theme))
        .into_any_element()
}

/// `1 file`, `7 files`.
pub(super) fn count_label(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The comparison control's two labels (DESIGN §4.1): "Since round n" and "All changes", the
/// second "All" in a panel narrower than [`COMPARISON_LABEL_FROM`].
pub(super) fn comparison_labels(last: u32, panel_width: f32) -> (String, String) {
    let all = if panel_width < COMPARISON_LABEL_FROM {
        "All"
    } else {
        "All changes"
    };
    (format!("Since round {last}"), all.to_string())
}
