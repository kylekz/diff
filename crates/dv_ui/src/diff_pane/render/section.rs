//! The "Not in the diff" section's header (DESIGN §4.1): the file header's 44px `codeHead` bar
//! with the chevron, "Not in the diff", the count of its threads and what they are, pinned at
//! the top as a file's header is while its cards scroll under it; and, while no file is
//! listed, the row under its cards that says so in the main area's empty-state words.

use gpui::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement as _, IntoElement as _,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, div, px,
};

use super::super::rows::{Row, heights};
use super::super::{DiffPane, SECTION};
use super::header::opaque;
use crate::automation::AutomationTargetExt as _;
use crate::components::{count_badge, icon, kbd, small_button};
use crate::icons::DvIcon;
use crate::keymap;
use crate::session::Comparison;
use crate::shell::EmptyState;
use crate::theme::{DvTheme, StyledText as _};

impl DiffPane {
    /// The section's header (block `block`); `pinned` draws the copy that stays at the top.
    pub(in super::super) fn render_section_header(
        &self,
        block: usize,
        pinned: bool,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let count = self
            .session
            .as_ref()
            .map_or(0, |session| session.read(cx).cards_not_in_diff().count());
        let collapsed = match self.blocks.get(block).and_then(|block| block.rows.first()) {
            Some(&Row::Header { collapsed, .. }) => collapsed && !pinned,
            _ => false,
        };
        let base = if pinned {
            "stream/pinned".to_string()
        } else {
            "stream/not_in_diff".to_string()
        };
        let hover = c.control_hover;
        let chevron = div()
            .id(ElementId::NamedInteger(
                "section-chevron".into(),
                u64::from(pinned),
            ))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(theme.sizes.control_xsmall_height)
            .rounded(theme.radii.chip)
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .child(icon(
                if collapsed {
                    DvIcon::ChevronRight
                } else {
                    DvIcon::ChevronDown
                },
                px(14.),
                c.fg2,
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_open(SECTION, collapsed, cx);
            }))
            .automation_target(format!("{base}/chevron"));
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .w_full()
            .h(px(heights::HEADER_BAR))
            .pl(theme.spacing.md)
            .pr(px(10.))
            .bg(opaque(c.code_head, pinned))
            .border_b_1()
            .border_color(c.line)
            .child(chevron)
            .child(icon(DvIcon::FileDiff, px(15.), c.fg3))
            .child(
                div()
                    .flex_none()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.fg)
                    .child("Not in the diff"),
            )
            .child(count_badge(theme).child(count.to_string()))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg3)
                    .child(self.section_subtitle(cx)),
            )
            .automation_target(if pinned {
                base
            } else {
                format!("{base}/header")
            })
    }

    /// The row under the section's cards while no file is listed: the empty state's heading
    /// and line, quietly, and for an empty interdiff its Show all changes (`I`).
    pub(in super::super) fn render_section_empty(
        &self,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let Some(empty) = self
            .session
            .as_ref()
            .and_then(|session| EmptyState::of_session(session.read(cx)))
        else {
            return div().h(px(heights::NOTICE)).into_any_element();
        };
        let show_all = matches!(empty, EmptyState::NoChangesSince(_)).then(|| {
            let session = self.session.clone();
            small_button("section-show-all", "Show all changes", theme)
                .child(kbd(keymap::label("i"), theme))
                .on_click(move |_, _, cx| {
                    if let Some(session) = &session {
                        session.update(cx, |session, cx| {
                            session.set_comparison(Comparison::All, cx)
                        });
                    }
                })
                .automation_target("stream/not_in_diff/empty/show_all")
        });
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(heights::NOTICE))
            .px(theme.spacing.xl)
            .border_t_1()
            .border_color(c.line)
            .text_spec(&theme.text.body)
            .child(icon(DvIcon::FileDiff, px(15.), c.fg3))
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(c.fg2)
                    .child(empty.heading()),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg3)
                    .child(empty.note()),
            )
            .children(show_all)
            .automation_target("stream/not_in_diff/empty")
    }
}
