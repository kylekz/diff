//! Thread cards and the composer in the stream (DESIGN §4.1): placed under the line they follow
//! on its side — in split view in that side's half, over the line's tint, the other half
//! `recess` (the board's inline card) — or across the file for an outdated thread, a comment on
//! the whole file and its composer, and across the stream in the "Not in the diff" section,
//! naming the path it was written on.

use dv_core::diff::LineKind;
use dv_core::review::{AnchorSide, ThreadId};
use gpui::{AnyElement, Context, IntoElement, ParentElement as _, Styled as _, div, px};

use super::super::DiffPane;
use super::super::rows::Insert;
use super::{MARKER_WIDTH, NUMBER_WIDTH};
use crate::services::ViewMode;
use crate::theme::DvTheme;
use crate::thread::{ComposerBox, ThreadCard};

/// Room to the card's right, and above and below it (the board's `8px 14px 10px`).
const CARD_RIGHT: f32 = 14.;
const CARD_TOP: f32 = 8.;
const CARD_BOTTOM: f32 = 10.;

impl DiffPane {
    /// `thread`'s card; `at` is where it sits, `None` for a card heading its file: an outdated
    /// thread, a comment on the whole file, or one placed at lines of a file whose rows aren't
    /// shown (too large to show).
    pub(in super::super) fn render_thread(
        &self,
        block: usize,
        thread: &ThreadId,
        at: Option<&Insert>,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(session) = self.session.clone() else {
            return div().into_any_element();
        };
        let (found, placement, read_only) = {
            let session = session.read(cx);
            let found = session.review().thread(thread).cloned();
            let placement = found.as_ref().map(|found| session.placement(found));
            (found, placement, session.is_read_only())
        };
        let (Some(found), Some(placement)) = (found, placement) else {
            return div().into_any_element();
        };
        let lines = placement.lines(&found.anchor);
        let moved = placement.shows_moved(&found);
        let collapsed = !self.is_thread_open(thread, found.state);
        let reply = self
            .composer
            .as_ref()
            .filter(|composer| composer.target.reply_to.as_ref() == Some(thread))
            .map(|composer| self.composer_box(composer, true, cx));
        let error = self.thread_error(thread).cloned();
        let pane = cx.weak_entity();
        // In the section, away from its file's rows, the card names the path it was written on;
        // one found in a file the review doesn't change offers that file's view at its lines.
        let in_section = placement.is_not_in_diff() && !self.is_file_view();
        let path = in_section.then(|| found.anchor.path.clone());
        let view = placement
            .placement()
            .unlisted_path()
            .filter(|_| in_section)
            .map(|path| {
                let (pane, path) = (pane.clone(), path.to_string());
                let line = lines.map(|(start, _)| start);
                move |_: &mut gpui::Window, cx: &mut gpui::App| {
                    pane.update(cx, |_, cx| {
                        cx.emit(crate::diff_pane::DiffPaneEvent::ViewFile {
                            path: path.clone(),
                            line,
                        })
                    })
                    .ok();
                }
            });
        let trail = placement.trail().map(|trail| trail.round);
        let card = ThreadCard::new(found, dv_core::review::event::now_ms())
            .outdated(placement.is_outdated())
            .trail(trail)
            .path(path)
            .unlisted(view)
            .shown_at(lines, moved)
            .focused(self.focused_thread.as_ref() == Some(thread))
            .collapsed(collapsed)
            .read_only(read_only)
            .reply(reply)
            .error(error)
            .on_state({
                let (pane, thread) = (pane.clone(), thread.clone());
                move |state, _, cx| {
                    pane.update(cx, |this, cx| this.set_thread_state(&thread, state, cx))
                        .ok();
                }
            })
            .on_toggle({
                let (pane, thread) = (pane.clone(), thread.clone());
                move |_, cx| {
                    pane.update(cx, |this, cx| this.toggle_thread(&thread, cx))
                        .ok();
                }
            })
            .on_reply({
                let (pane, thread) = (pane.clone(), thread.clone());
                move |window, cx| {
                    pane.update(cx, |this, cx| this.start_reply(&thread, window, cx))
                        .ok();
                }
            })
            .on_focus({
                let thread = thread.clone();
                move |_, cx| {
                    pane.update(cx, |this, cx| this.focus_thread(&thread, cx))
                        .ok();
                }
            });
        self.place(block, at, card.into_any_element(), theme)
    }

    /// The new thread's composer, under the line it follows (`at`), or heading its file for a
    /// comment on the whole file.
    pub(in super::super) fn render_composer(
        &self,
        block: usize,
        at: Option<&Insert>,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(composer) = self
            .composer
            .as_ref()
            .filter(|composer| composer.target.reply_to.is_none())
        else {
            return div().into_any_element();
        };
        let element = self.composer_box(composer, false, cx).into_any_element();
        self.place(block, at, element, theme)
    }

    fn composer_box(
        &self,
        composer: &crate::thread::Composer,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> ComposerBox {
        // A reply to an annotation asks the agent about it (PLAN §19.3), who reads it once the
        // round is finished.
        let asking = self.asking_agent(composer, cx);
        let pane = cx.weak_entity();
        let cancel = pane.clone();
        ComposerBox::new(
            composer.input.clone(),
            composer.target.title(),
            match (compact, &asking) {
                (true, Some(_)) => "Ask",
                (true, None) => "Reply",
                (false, _) => "Comment",
            },
            move |window, cx| {
                pane.update(cx, |this, cx| this.post_composer(window, cx))
                    .ok();
            },
            move |window, cx| {
                cancel
                    .update(cx, |this, cx| this.cancel_composer(window, cx))
                    .ok();
            },
        )
        .posting(composer.posting)
        .error(composer.error.clone())
        .compact(compact)
        .hint(asking.map(|agent| format!("{agent} reads it with the round")))
    }

    /// The agent `composer` asks, when it replies to an annotation (PLAN §19.3).
    pub(in super::super) fn asking_agent(
        &self,
        composer: &crate::thread::Composer,
        cx: &gpui::App,
    ) -> Option<String> {
        let thread = composer.target.reply_to.as_ref()?;
        let session = self.session.as_ref()?.read(cx);
        session
            .review()
            .thread(thread)
            .filter(|thread| thread.state == dv_core::review::ThreadState::Annotation)
            .map(|thread| crate::agents::shown_name(&thread.author).to_string())
    }

    /// Lays `element` out under the line `at` names: in split view in its side's half, over
    /// the line's tint, beside an empty `recess` half; in unified view across the code, past
    /// the line numbers. Without `at`, across the file.
    fn place(
        &self,
        block: usize,
        at: Option<&Insert>,
        element: AnyElement,
        theme: &DvTheme,
    ) -> AnyElement {
        let c = &theme.colors;
        let padded = |left: f32, element: AnyElement| {
            div()
                .pt(px(CARD_TOP))
                .pr(px(CARD_RIGHT))
                .pb(px(CARD_BOTTOM))
                .pl(px(left))
                .child(element)
        };
        let Some(at) = at else {
            return div()
                .w_full()
                .child(padded(f32::from(theme.spacing.xl), element))
                .into_any_element();
        };
        let tint = self
            .lines(&self.blocks[block].path)
            .and_then(|lines| lines.get(at.line))
            .map_or(gpui::transparent_black(), |line| match line.kind {
                LineKind::Added => c.add_bg,
                LineKind::Removed => c.del_bg,
                LineKind::Context => gpui::transparent_black(),
            });
        match self.mode {
            ViewMode::Unified => div()
                .w_full()
                .bg(tint)
                .child(padded(
                    self.number_columns() * NUMBER_WIDTH + MARKER_WIDTH + 2.,
                    element,
                ))
                .into_any_element(),
            ViewMode::Split => {
                let half = |content: Option<AnyElement>| {
                    let cell = div().w_1_2().min_w_0();
                    match content {
                        Some(content) => cell
                            .bg(tint)
                            .child(padded(NUMBER_WIDTH + MARKER_WIDTH + 2., content)),
                        None => cell.bg(c.recess),
                    }
                };
                let (old, new) = match at.side {
                    AnchorSide::Old => (half(Some(element)), half(None)),
                    AnchorSide::New => (half(None), half(Some(element))),
                };
                div()
                    .w_full()
                    .flex()
                    .child(old.border_r_1().border_color(c.line))
                    .child(new)
                    .into_any_element()
            }
        }
    }
}
