//! The file view's header and notices (DESIGN §4.1 "File view"): a 44px `codeHead` bar like a
//! file header's — a file glyph, the path with its directory dimmed, the side's label ("working
//! tree", "main"), the review's status pill for a file it changes or "Unchanged" for one it
//! doesn't, and the close button with its `Esc` keycap — pinned at the top while the lines
//! scroll; and what shows in place of lines (skeleton rows, a file the side doesn't hold, a read
//! that failed, binary, too large, an empty file).

use gpui::{
    AnyElement, Context, FontWeight, HighlightStyle, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, StyledText, div, px,
};

use super::super::DiffPane;
use super::super::rows::{Notice, heights};
use super::Source;
use crate::automation::AutomationTargetExt as _;
use crate::banner::Banner;
use crate::components::{chip, elide_path_middle, icon, kbd, skeleton, tooltip};
use crate::failure::Failure;
use crate::file_list::{split_path, status_letter};
use crate::icons::DvIcon;
use crate::keymap;
use crate::session::ViewFrom;
use crate::theme::{DvTheme, StyledText as _};
use crate::thread::one_line;

impl DiffPane {
    /// The file view's header bar; `pinned` draws the copy that stays at the top.
    pub(in super::super) fn render_file_view_header(
        &self,
        pinned: bool,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let (Some(path), Some(side), Some(session)) = (
            self.file_view_path(),
            self.file_view_side(),
            self.session.as_ref().map(|session| session.read(cx)),
        ) else {
            return div().into_any_element();
        };
        let (old_label, new_label) = session.target_side_labels();
        let worktree = self.file_view_worktree();
        let outside = self.file_view_from() == Some(ViewFrom::Outside);
        let label = match side {
            _ if worktree => "working tree".to_string(),
            dv_core::review::AnchorSide::Old => old_label,
            dv_core::review::AnchorSide::New => new_label,
        };
        // The review's own status for the file (all changes); in an interdiff, whether it
        // changed since the round too (DESIGN §4.1 "File view").
        let status = session
            .review_file(path)
            .map(|entry| status_letter(entry.status));
        let since = match session.comparison() {
            crate::session::Comparison::SinceRound(n) if status.is_some() => {
                Some(if session.view_from_stream(path) {
                    format!("changes since round {n}")
                } else {
                    format!("unchanged since round {n}")
                })
            }
            _ => None,
        };
        let quiet = |text: String| {
            div()
                .flex_none()
                .text_spec(&theme.text.secondary)
                .text_color(c.fg3)
                .child(text)
                .into_any_element()
        };
        let changed: AnyElement = match status {
            _ if outside => quiet("Outside the repository · read-only".to_string()),
            _ if worktree => {
                quiet("As it stands now, not this review's side · read-only".to_string())
            }
            Some(letter) => {
                let (fg, bg) = letter.colors(theme);
                chip(fg, bg, theme)
                    .px(px(7.))
                    .text_size(px(11.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(letter.word)
                    .into_any_element()
            }
            None => quiet("Unchanged in this review".to_string()),
        };
        let since = since.map(quiet);
        let hover = c.control_hover;
        let close = div()
            .id(if pinned {
                "file-view-close-pinned"
            } else {
                "file-view-close"
            })
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.spacing.sm)
            .h(theme.sizes.control_height)
            .pl(theme.spacing.md)
            .pr(theme.spacing.sm)
            .rounded(theme.radii.control)
            .text_spec(&theme.text.secondary)
            .font_weight(FontWeight::MEDIUM)
            .text_color(c.fg2)
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .child(icon(DvIcon::X, px(14.), c.fg2))
            .child("Close")
            .child(kbd(keymap::label("escape"), theme))
            .tooltip(tooltip("Back to the review (Esc)"))
            .on_click(cx.listener(|this, _, _, cx| this.ask_close(cx)))
            .automation_target("file_view/close");
        let background = if pinned {
            c.code_head.alpha(1.)
        } else {
            c.code_head
        };
        // A file outside the repository is named whole, its line too, cut in its middle when
        // it doesn't fit (DESIGN §4.1 "File view outside the repository").
        // Drawn as one line: git allows a path any character but NUL.
        let drawn = one_line(path);
        let shown_path = if outside {
            let named = match self.file_view_opened_at() {
                Some(line) => format!("{drawn}:{line}"),
                None => drawn.to_string(),
            };
            let advance = {
                let system = cx.text_system();
                let font = system.resolve_font(&gpui::font(theme.fonts.mono.clone()));
                system.ch_advance(font, px(13.)).unwrap_or(px(7.8))
            };
            // The bar less its padding, glyph and gaps, the quiet label and the close button.
            let room = f32::from(self.view_bounds.size.width) - 12. - 15. - 40. - 210. - 110.;
            let columns = (room / f32::from(advance).max(1.)).max(12.) as usize;
            elide_path_middle(&named, |text| text.chars().count() <= columns)
        } else {
            drawn.into_owned()
        };
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .w_full()
            .h(px(heights::HEADER_BAR))
            .pl(theme.spacing.lg)
            .pr(px(10.))
            .bg(background)
            .border_b_1()
            .border_color(c.line)
            .child(icon(DvIcon::File, px(15.), c.fg3))
            .child(
                div()
                    .min_w_0()
                    .flex_shrink(1.)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_start()
                    .font_family(theme.fonts.mono.clone())
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.fg)
                    .child(path_text(&shown_path, theme)),
            )
            .children((!outside).then(|| {
                chip(c.fg2, c.layer, theme)
                    .flex_none()
                    .child(icon(
                        if side == dv_core::review::AnchorSide::Old {
                            DvIcon::GitBranch
                        } else {
                            DvIcon::File
                        },
                        px(12.),
                        c.fg3,
                    ))
                    .child(label)
            }))
            .child(changed)
            .children(since)
            .child(div().flex_1())
            .child(close)
            .automation_target(if pinned {
                "file_view/pinned"
            } else {
                "file_view/header"
            })
    }

    /// What the file view shows in place of lines.
    pub(in super::super) fn render_file_view_notice(
        &self,
        notice: &Notice,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let name = "file_view/notice";
        match notice {
            Notice::Loading => div()
                .flex()
                .flex_col()
                .gap(theme.spacing.md)
                .h(px(heights::LOADING))
                .px(theme.spacing.xl)
                .py(theme.spacing.lg)
                .children(
                    [420., 300., 360.]
                        .into_iter()
                        .map(|width| skeleton(px(width), px(10.), theme)),
                )
                .automation_target(name),
            Notice::Failed => {
                let failure = match self.file_view_source(cx) {
                    Source::Failed(failure) => failure,
                    Source::Missing => {
                        let label = self
                            .session
                            .as_ref()
                            .map(|session| {
                                let (old, new) = session.read(cx).target_side_labels();
                                match self.file_view_side() {
                                    Some(dv_core::review::AnchorSide::Old) => old,
                                    _ => new,
                                }
                            })
                            .unwrap_or_else(|| "the review's side".to_string());
                        Failure::new(format!("{label} has no file at this path"))
                    }
                    _ => Failure::new("The file didn't load"),
                };
                Banner::new(name, failure).into_any_element()
            }
            notice => {
                let (glyph, title, mut detail) = super::super::render::notice_text(notice, None);
                // One side shown: its size alone, not the diff's "added" or "deleted".
                if let Notice::TooLarge { old_len, new_len } = notice
                    && let Some(len) = new_len.or(*old_len)
                {
                    let limit = if self.file_view_outside_unread(cx) {
                        "over the 4 MB dv reads of a file outside the repository"
                    } else {
                        "over 2 MB or 20,000 lines"
                    };
                    detail = format!("{} — {limit}", super::super::render::byte_size(len));
                }
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(heights::NOTICE))
                    .px(theme.spacing.xl)
                    .text_spec(&theme.text.body)
                    .child(icon(glyph, px(15.), c.fg3))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(c.fg2)
                            .child(title),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_spec(&theme.text.secondary)
                            .text_color(c.fg3)
                            .child(detail),
                    )
                    // A file outside the repository is read up to `probe_path`'s cap only:
                    // past it, there is nothing more to load.
                    .children(
                        (matches!(notice, Notice::TooLarge { .. })
                            && !self.file_view_outside_unread(cx))
                        .then(|| {
                            crate::components::small_button(
                                "file-view-load-anyway",
                                "Load anyway",
                                theme,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.file_view_load_anyway(cx)))
                            .automation_target("file_view/notice/load_anyway")
                        }),
                    )
                    .automation_target(name)
            }
        }
    }
}

/// The path: its directory in `fg3`, its name at 600 (a file outside the repository may be
/// spelled with `\`).
fn path_text(path: &str, theme: &DvTheme) -> StyledText {
    let dir = match path.rfind(['/', '\\']) {
        Some(at) => &path[..at + 1],
        None => split_path(path).0,
    };
    let dim = HighlightStyle {
        color: Some(theme.colors.fg3),
        font_weight: Some(FontWeight::MEDIUM),
        ..Default::default()
    };
    let highlights = if dir.is_empty() {
        Vec::new()
    } else {
        vec![(0..dir.len(), dim)]
    };
    StyledText::new(path.to_string()).with_highlights(highlights)
}
