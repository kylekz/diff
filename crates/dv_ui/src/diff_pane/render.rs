//! The stream's layout (DESIGN §4.1 "Diff", the board's `GlassReview` main column): the list of
//! rows over the `code` surface, the pinned header above it, the file menu and the find bar.
//! Code rows are `code`'s; this file draws the rest of the rows (gaps, notices, the footer).

pub(super) mod code;
mod find_bar;
mod header;
mod section;
mod thread;

use dv_core::diff::{EXPAND_STEP, Expand, FileEntry};
use dv_core::review::AnchorSide;
use gpui::{
    AnyElement, Context, DispatchPhase, FontWeight, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Render,
    ScrollEvent, StatefulInteractiveElement as _, Styled as _, Window, canvas, div,
    prelude::FluentBuilder as _, px,
};
use std::rc::Rc;

use super::DiffPane;
use super::rows::{Gap, InsertItem, Notice, Row, heights};
use crate::automation::AutomationTargetExt as _;
use crate::banner::{Banner, BannerAction};
use crate::components::{icon, skeleton, small_button};
use crate::icons::DvIcon;
use crate::keymap;
use crate::services::ViewMode;
use crate::session::FileState;
use crate::theme::{ActiveTheme as _, DvTheme, StyledText as _};
use code::{MARKER_WIDTH, NUMBER_WIDTH, TEXT_INSET};

impl Render for DiffPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Rc::new(cx.dv_theme().clone());
        if self.char_width.is_none() {
            let text = cx.text_system();
            let font = text.resolve_font(&gpui::font(theme.fonts.mono.clone()));
            self.char_width = text
                .ch_advance(font, theme.text.code.size)
                .ok()
                .map(f32::from);
        }
        self.code_columns = self.columns_in_view();
        self.view_bounds = self.list.viewport_bounds();
        self.frame_marks = self.frame_marks(window);
        // The lines this frame draws put their geometry back as they are drawn.
        self.geometry.borrow_mut().clear();
        if self.activation.is_none() {
            self.activation = Some(cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.end_text_drag(cx);
                }
            }));
        }
        self.release_orphaned_focus(window, cx);
        self.hover_before_frame(window, cx);
        self.focus_waiting_composer(window, cx);
        self.reveal_laid_out_composer(window, cx);
        self.open_waiting_reply(window, cx);
        // What the viewport shows is worked out again after this frame's layout, when it moved.
        if self.viewport_key != Some(self.current_viewport_key()) && !self.viewport_pending {
            self.viewport_pending = true;
            cx.defer_in(window, |this, _, cx| {
                this.viewport_pending = false;
                this.viewport_changed(cx);
            });
        }
        let view = cx.entity();
        let rows_theme = theme.clone();
        let list = gpui::list(self.list.clone(), move |item, window, cx| {
            let theme = rows_theme.clone();
            view.update(cx, |this, cx| this.render_item(item, &theme, window, cx))
        })
        .size_full();
        let pinned = self.pinned().map(|(block, shift)| {
            div()
                .absolute()
                .top(px(shift))
                .left_0()
                .right_0()
                // Clicks stay with the header; the wheel still scrolls the rows under it.
                .block_mouse_except_scroll()
                .child(self.render_header(block, true, &theme, cx))
        });
        let menu = self.render_menu(&theme, window, cx);
        let hover_card = self.render_hover(&theme, cx);
        let find_bar = self.render_find_bar(&theme, cx);
        // While text is being selected the pointer is followed wherever it goes, in the
        // window or out of it, until the button comes up (`select`).
        let text_drag = self.text_drag.is_some().then(|| {
            let view = cx.entity();
            canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    let moved = view.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                        if phase != DispatchPhase::Bubble {
                            return;
                        }
                        moved.update(cx, |this, cx| {
                            if event.pressed_button == Some(MouseButton::Left) {
                                this.drag_text_to(event.position, cx);
                            } else {
                                this.end_text_drag(cx);
                            }
                        });
                    });
                    let released = view.clone();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                            released.update(cx, |this, cx| this.end_text_drag(cx));
                        }
                    });
                },
            )
            .absolute()
            .size_full()
        });
        div()
            .id("diff-pane")
            .relative()
            .size_full()
            .overflow_hidden()
            .text_color(theme.colors.fg)
            // The pointer leaving the pane (onto the panel, the status bar) leaves the card's
            // token: it closes after its grace unless the pointer is over the card (`intel`).
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    this.pointer_left_pane(cx);
                }
            }))
            .on_scroll(cx.listener(|this, event: &ScrollEvent, _, cx| {
                let delta = event.delta.pixel_delta(px(heights::CODE));
                if delta.x != px(0.) {
                    this.scroll_sideways(-f32::from(delta.x), cx);
                }
                // The card hangs where its token was: a scroll takes it away.
                this.close_hover(cx);
            }))
            // The pointer resting on an identifier asks for its hover (`intel`).
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if event.pressed_button.is_none() {
                    this.hover_pointer_moved(event.position, cx);
                }
            }))
            // A press anywhere in the stream clears the selected text and the caret before
            // what is under it hears it; a press in code then starts them again (`select`).
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                if event.button == MouseButton::Left {
                    this.press_outside_text(event.modifiers.shift, cx);
                }
            }))
            // A line drag (a press on a comment `+`) ends wherever the button comes up: over
            // the stream, before anything in it hears the release, or outside it.
            .capture_any_mouse_up(cx.listener(|this, event: &MouseUpEvent, window, cx| {
                if event.button == MouseButton::Left {
                    this.finish_line_drag(window, cx);
                    this.end_text_drag(cx);
                }
            }))
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.finish_line_drag(window, cx);
                    this.end_text_drag(cx);
                }),
            )
            .child(list)
            .children(pinned)
            .children(find_bar)
            .children(text_drag)
            .children(menu)
            .children(hover_card)
            .automation_target(if self.is_file_view() {
                "file_view"
            } else {
                "stream"
            })
    }
}

impl DiffPane {
    /// How many columns of code a cell shows at the list's width.
    fn columns_in_view(&self) -> usize {
        let width = f32::from(self.list.viewport_bounds().size.width);
        let width = if width > 0. { width } else { 1160. };
        let cell = match self.mode {
            ViewMode::Unified => {
                width - self.number_columns() * NUMBER_WIDTH - MARKER_WIDTH - TEXT_INSET
            }
            ViewMode::Split => width / 2. - NUMBER_WIDTH - MARKER_WIDTH - TEXT_INSET - 1.,
        };
        let char_width = self.char_width.unwrap_or(7.2).max(1.);
        (cell.max(0.) / char_width).ceil() as usize + 1
    }

    /// List item `item`. Runs while the list lays out, so it never touches the list's state.
    fn render_item(
        &mut self,
        item: usize,
        theme: &DvTheme,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((block, row)) = self.locate(item) else {
            return div().w_full().h(px(self.spacer_height)).into_any_element();
        };
        let row = self.blocks[block].rows[row].clone();
        let element = match &row {
            Row::Header { .. } => self.render_header(block, false, theme, cx),
            Row::Line(_) | Row::Pair { .. } => self.render_code(block, &row, theme, cx),
            Row::Gap(gap) => self.render_gap(block, gap, theme, cx),
            Row::Notice(notice) => self.render_notice(block, notice, theme, cx),
            Row::Footer => self.render_footer(block, theme, cx),
            Row::Insert(insert) => match &insert.item {
                InsertItem::Thread(thread) => {
                    self.render_thread(block, thread, Some(insert), theme, cx)
                }
                InsertItem::Composer => self.render_composer(block, Some(insert), theme, cx),
            },
            Row::Heading(InsertItem::Thread(thread)) => {
                self.render_thread(block, thread, None, theme, cx)
            }
            Row::Heading(InsertItem::Composer) => self.render_composer(block, None, theme, cx),
            Row::Empty => self.render_section_empty(theme, cx),
        };
        div().w_full().child(element).into_any_element()
    }

    /// Block `block`'s session entry and loaded state.
    fn entry_and_state(
        &self,
        block: usize,
        cx: &gpui::App,
    ) -> Option<(FileEntry, Option<FileState>)> {
        let session = self.session.as_ref()?.read(cx);
        let file = block.checked_sub(self.first_file_block())?;
        let entry = session.files().get(file)?.clone();
        let state = session.file_state(&entry.path).cloned();
        Some((entry, state))
    }

    /// A gap row (DESIGN §4.1): the next hunk's `@@ -a,b +c,d @@`, how many lines it hides, and
    /// "Expand 20 · Expand all", on the `hunk` tint.
    fn render_gap(
        &self,
        block: usize,
        gap: &Gap,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let path = self.blocks[block].path.clone();
        let name = format!("stream/gap/{path}/{}", gap.ordinal);
        let hidden = gap.lines.len();
        let ordinal = gap.ordinal;
        let step = if gap.below {
            Expand::FromBelow
        } else {
            Expand::FromAbove
        };
        let button = |key: &'static str, label: String, expand: Expand| {
            let path = path.clone();
            ghost_small(
                gpui::ElementId::NamedInteger(key.into(), (block * 10_000 + ordinal) as u64),
                theme,
            )
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.expand(&path, ordinal, expand, cx);
            }))
            .automation_target(format!("{name}/{key}"))
        };
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .h(px(heights::GAP))
            .pl(theme.spacing.xl)
            .pr(theme.spacing.xl)
            .bg(c.hunk)
            .border_t_1()
            .border_b_1()
            .border_color(c.line)
            .text_spec(&theme.text.secondary)
            .text_color(c.fg3)
            .child(icon(DvIcon::ChevronsUpDown, px(13.), c.fg3))
            .children(gap.hunk.map(|hunk| {
                div()
                    .font_family(theme.fonts.mono.clone())
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(c.blue)
                    .child(hunk.to_string())
            }))
            .child(format!(
                "··· {hidden} unchanged {}",
                if hidden == 1 { "line" } else { "lines" }
            ))
            .when(hidden > EXPAND_STEP, |this| {
                this.child(button("expand", format!("Expand {EXPAND_STEP}"), step))
                    .child("·")
            })
            .child(button("expand_all", "Expand all".to_string(), Expand::All))
            .automation_target(name)
    }

    /// What a file shows in place of its lines.
    fn render_notice(
        &self,
        block: usize,
        notice: &Notice,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let path = self.blocks[block].path.clone();
        let name = format!("stream/notice/{path}");
        if self.is_file_view() {
            return self.render_file_view_notice(notice, theme, cx);
        }
        let Some((entry, state)) = self.entry_and_state(block, cx) else {
            return div().into_any_element();
        };
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
                let failure = match state {
                    Some(FileState::Failed(failure)) => failure,
                    _ => crate::Failure::new("The file's changes didn't load"),
                };
                let open = self.files.get(&path).is_some_and(|ui| ui.banner_open);
                let this = cx.weak_entity();
                let toggle_path = path.clone();
                let retry_path = path.clone();
                let session = self.session.clone();
                Banner::new(name, failure)
                    .disclosure(open, move |_, cx| {
                        this.update(cx, |this, cx| {
                            if let Some(ui) = this.files.get_mut(&toggle_path) {
                                ui.banner_open = !ui.banner_open;
                            }
                            if let Some(&index) = this.index.get(&toggle_path) {
                                let item = this.starts[index] + 1;
                                this.remeasure(item);
                            }
                            cx.notify();
                        })
                        .ok();
                    })
                    .action(BannerAction::new("retry", "Retry", move |_, cx| {
                        if let Some(session) = &session {
                            session.update(cx, |session, cx| session.reload_file(&retry_path, cx));
                        }
                    }))
                    .into_any_element()
            }
            notice => {
                let (glyph, title, detail) = notice_text(notice, Some(&entry));
                let load_anyway = matches!(notice, Notice::TooLarge { .. }).then(|| {
                    let path = path.clone();
                    let session = self.session.clone();
                    small_button(
                        gpui::ElementId::NamedInteger("load-anyway".into(), block as u64),
                        "Load anyway",
                        theme,
                    )
                    .on_click(move |_, _, cx| {
                        if let Some(session) = &session {
                            session.update(cx, |session, cx| session.load_anyway(&path, cx));
                        }
                    })
                    .automation_target(format!("{name}/load_anyway"))
                });
                let dirty = matches!(notice, Notice::Submodule { dirty: true, .. }).then(|| {
                    crate::components::chip(c.yellow, c.yellow_soft, theme).child("dirty")
                });
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
                    .children(dirty)
                    .children(load_anyway)
                    .automation_target(name)
            }
        }
    }

    /// The next-file footer: "Next: `path`" and "Mark viewed and go next `V` `J`".
    fn render_footer(&self, block: usize, theme: &DvTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let path = &self.blocks[block].path;
        let name = format!("stream/footer/{path}");
        let next = self.blocks.get(block + 1).map(|next| next.path.clone());
        let viewed = self.is_viewed(path, cx);
        let read_only = self
            .session
            .as_ref()
            .is_some_and(|session| session.read(cx).is_read_only());
        let (label, keys): (Option<&str>, &[&str]) = match (&next, viewed || read_only) {
            (Some(_), false) => (Some("Mark viewed and go next"), &["v", "j"]),
            (Some(_), true) => (Some("Go to next file"), &["j"]),
            (None, false) => (Some("Mark viewed"), &["v"]),
            (None, true) => (None, &[]),
        };
        // The next file's path truncates at its start, so its name stays (DESIGN §4.1).
        let note = match next {
            Some(next) => div()
                .flex()
                .gap(px(4.))
                .min_w_0()
                .child(div().flex_none().child("Next:"))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis_start()
                        .child(next),
                ),
            None => div().child("Last file"),
        };
        let button = label.map(|label| {
            let keys: Vec<_> = keys
                .iter()
                .map(|key| crate::components::kbd(keymap::label(key), theme))
                .collect();
            crate::components::button(
                gpui::ElementId::NamedInteger("footer-next".into(), block as u64),
                theme,
            )
            .child(label)
            .children(keys)
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.blocks.get(block + 1).is_some() {
                    this.mark_viewed_and_next(block, cx);
                } else if let Some(path) = this.blocks.get(block).map(|b| b.path.clone()) {
                    this.toggle_viewed(&path, cx);
                }
            }))
            .automation_target(format!("{name}/next"))
        });
        div()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(10.))
            .h(px(heights::FOOTER))
            .px(theme.spacing.xl)
            .child(
                note.min_w_0()
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg3),
            )
            .children(button)
            .automation_target(name)
    }
}

/// The board's `ghostSm` button: 22 high, no fill until hovered, 12/500 in `fg2`.
fn ghost_small(id: gpui::ElementId, theme: &DvTheme) -> gpui::Stateful<gpui::Div> {
    let c = &theme.colors;
    let hover = c.control_hover;
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(5.))
        .h(theme.sizes.control_xsmall_height)
        .px(theme.spacing.md)
        .rounded(theme.radii.control)
        .text_spec(&theme.text.secondary)
        .font_weight(FontWeight::MEDIUM)
        .text_color(c.fg2)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
}

/// A notice's glyph, title and detail; without `entry` (the file view), a text that didn't
/// change is an empty file.
pub(super) fn notice_text(
    notice: &Notice,
    entry: Option<&FileEntry>,
) -> (DvIcon, &'static str, String) {
    match notice {
        Notice::Binary { old_len, new_len } => {
            (DvIcon::Binary, "Binary file", sizes(*old_len, *new_len))
        }
        Notice::TooLarge { old_len, new_len } => (
            DvIcon::FileDiff,
            "Too large to show",
            format!("{} — over 2 MB or 20,000 lines", sizes(*old_len, *new_len)),
        ),
        Notice::Submodule {
            old_oid, new_oid, ..
        } => {
            let short = |oid: &Option<dv_core::Oid>| {
                oid.as_ref()
                    .map_or("none".to_string(), |oid| oid.as_str()[..7].to_string())
            };
            (
                DvIcon::Package,
                "Submodule",
                format!("{} → {}", short(old_oid), short(new_oid)),
            )
        }
        Notice::Unmerged => (
            DvIcon::TriangleAlert,
            "Unresolved conflict",
            "the index holds no single version of this file".to_string(),
        ),
        Notice::NoContentChange => match entry {
            None => (DvIcon::File, "Empty file", "nothing to show".to_string()),
            Some(entry) => (
                DvIcon::FileDiff,
                "No content change",
                match (entry.old_mode, entry.new_mode, &entry.old_path) {
                    (_, _, Some(old)) => format!("renamed from {old}"),
                    (Some(old), Some(new), _) if old != new => {
                        format!("mode {} → {}", mode_text(old), mode_text(new))
                    }
                    _ => "an empty file".to_string(),
                },
            ),
        },
        Notice::Loading | Notice::Failed => (DvIcon::FileDiff, "", String::new()),
    }
}

fn mode_text(mode: dv_core::diff::FileMode) -> &'static str {
    use dv_core::diff::FileMode;
    match mode {
        FileMode::Regular => "100644",
        FileMode::Executable => "100755",
        FileMode::Symlink => "120000",
        FileMode::Submodule => "160000",
    }
}

/// "12.4 KB → 13.1 KB", "added · 3 KB", "deleted · 3 KB".
fn sizes(old: Option<u64>, new: Option<u64>) -> String {
    match (old, new) {
        (Some(old), Some(new)) => format!("{} → {}", byte_size(old), byte_size(new)),
        (None, Some(new)) => format!("added · {}", byte_size(new)),
        (Some(old), None) => format!("deleted · {}", byte_size(old)),
        (None, None) => "size unknown".to_string(),
    }
}

/// A size as people read it: bytes, KB or MB (1024s), one decimal above bytes.
pub fn byte_size(bytes: u64) -> String {
    const KB: f64 = 1024.;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes} B")
    } else if value < KB * KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{:.1} MB", value / (KB * KB))
    }
}

/// A side as automation targets name it.
fn side_key(side: AnchorSide) -> &'static str {
    match side {
        AnchorSide::Old => "old",
        AnchorSide::New => "new",
    }
}
