//! The thread card and the composer as elements (DESIGN §4.1 "Thread card", the board's inline
//! card: `thread` surface, a 32px header with the state chip, the anchor and the actions, then
//! the comments with 22px avatars and a reply field).

use std::rc::Rc;

use dv_core::review::{Author, AuthorKind, Thread, ThreadState};
use gpui::{
    AnyElement, App, Div, ElementId, Entity, FontWeight, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, RenderOnce, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Textarea, TextareaState};

use super::{
    first_line, initial, relative_time, role_tag, role_word, shown_anchor_label, state_label,
    state_note, trail_label,
};
use crate::actions::composer::{Cancel, Post};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{chip, icon, kbd, kbd_on_accent, primary_button, small_button};
use crate::failure::Failure;
use crate::icons::DvIcon;
use crate::keymap;
use crate::theme::{ActiveTheme as _, DvTheme, StyledText as _};

type Handler = Rc<dyn Fn(&mut Window, &mut App)>;
type StateHandler = Rc<dyn Fn(ThreadState, &mut Window, &mut App)>;

/// A thread's card (see the module docs). The stream places it; the card fills its width.
#[derive(IntoElement)]
pub struct ThreadCard {
    thread: Thread,
    now: u64,
    outdated: bool,
    /// The path its anchor names, before the anchor: a card away from its file's rows (the
    /// stream's "Not in the diff" section).
    path: Option<String>,
    /// The lines it shows at now, when they aren't its anchor's.
    lines: Option<(u32, u32)>,
    /// Outdated, drawn where its code went since this round (PLAN §8): the anchor says so.
    trail: Option<u32>,
    /// On a file the review doesn't change, found there (PLAN §8), away from it (the "Not in
    /// the diff" section): an "Unchanged file" chip and View file, which runs this.
    unlisted: Option<Handler>,
    moved: bool,
    focused: bool,
    collapsed: bool,
    read_only: bool,
    reply: Option<ComposerBox>,
    error: Option<Failure>,
    on_state: Option<StateHandler>,
    on_toggle: Option<Handler>,
    on_reply: Option<Handler>,
    on_focus: Option<Handler>,
}

impl ThreadCard {
    /// A card for `thread`, its times relative to `now` (ms since the epoch).
    pub fn new(thread: Thread, now: u64) -> Self {
        Self {
            thread,
            now,
            outdated: false,
            path: None,
            lines: None,
            trail: None,
            unlisted: None,
            moved: false,
            focused: false,
            collapsed: false,
            read_only: false,
            reply: None,
            error: None,
            on_state: None,
            on_toggle: None,
            on_reply: None,
            on_focus: None,
        }
    }

    /// Its anchored text can't be found (PLAN §8): the yellow chip and its lines as they were.
    pub fn outdated(mut self, outdated: bool) -> Self {
        self.outdated = outdated;
        self
    }

    /// Drawn where its code went since round `round` (an outdated thread with a trail, PLAN
    /// §8): the anchor reads "code changed since round 1" (DESIGN §4.1).
    pub fn trail(mut self, round: Option<u32>) -> Self {
        self.trail = round;
        self
    }

    /// On a file the review doesn't change, shown away from it (DESIGN §4.1 "Not in the
    /// diff"): the "Unchanged file" chip, and **View file**, which runs `view` (the file view
    /// at its line).
    pub fn unlisted(mut self, view: Option<impl Fn(&mut Window, &mut App) + 'static>) -> Self {
        self.unlisted = view.map(|view| Rc::new(view) as Handler);
        self
    }

    /// Names `path` before the anchor ("src/hooks/use-can.ts · new · line 12"), for a card
    /// that isn't under its file's header (DESIGN §4.1, "Not in the diff").
    pub fn path(mut self, path: Option<String>) -> Self {
        self.path = path;
        self
    }

    /// Where it shows now (`None`: its anchor's lines) and whether those are lines it moved
    /// to: the anchor then reads "new · lines 34–36 · moved" (DESIGN §4.1).
    pub fn shown_at(mut self, lines: Option<(u32, u32)>, moved: bool) -> Self {
        self.lines = lines;
        self.moved = moved;
        self
    }

    /// The keyboard's thread (`]`/`[`): the accent ring, and `R` beside the reply field.
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// One line instead of the whole thread (a resolved thread until it is opened).
    pub fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }

    /// A finished review's card: the actions and the reply field show, disabled.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// The reply composer, when it is open on this thread.
    pub fn reply(mut self, reply: Option<ComposerBox>) -> Self {
        self.reply = reply;
        self
    }

    /// Why the reviewer's last Resolve or Reopen failed.
    pub fn error(mut self, error: Option<Failure>) -> Self {
        self.error = error;
        self
    }

    /// Resolve and Reopen: the state the reviewer asked for.
    pub fn on_state(mut self, f: impl Fn(ThreadState, &mut Window, &mut App) + 'static) -> Self {
        self.on_state = Some(Rc::new(f));
        self
    }

    /// The chevron of a resolved thread: collapse or open it.
    pub fn on_toggle(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_toggle = Some(Rc::new(f));
        self
    }

    /// A click on the reply field.
    pub fn on_reply(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_reply = Some(Rc::new(f));
        self
    }

    /// A click anywhere on the card: it becomes the focused thread.
    pub fn on_focus(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_focus = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for ThreadCard {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        if self.thread.state == ThreadState::Annotation {
            return self.render_annotation(cx);
        }
        let theme = cx.dv_theme().clone();
        let c = &theme.colors;
        let name = format!("thread/{}", self.thread.id);
        let anchor = &self.thread.anchor;
        let anchor_text = match (&self.path, self.trail) {
            (_, Some(round)) => trail_label(round),
            (Some(path), None) => format!(
                "{path} · {}",
                shown_anchor_label(anchor, self.lines, self.moved)
            ),
            (None, None) => shown_anchor_label(anchor, self.lines, self.moved),
        };
        let resolved = self.thread.state == ThreadState::Resolved;

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
            .child(state_chip(&self.thread, &theme))
            .when(self.outdated, |this| {
                this.child(
                    chip(c.yellow, c.yellow_soft, &theme)
                        .child(icon(DvIcon::TriangleAlert, px(12.), c.yellow))
                        .child("Outdated"),
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
            // The anchor (and a collapsed thread's first line) take only the room the chips and
            // the actions leave, so in a narrow card they give way first, then the chip's agent
            // name; the actions never do.
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
                            // A path before the anchor gives way at its start, so its name
                            // and the lines stay (as paths do everywhere, DESIGN §4.1).
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
                                .child(first_line(&self.thread.body)),
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
            .children(self.actions(&name, &theme))
            .when(resolved, |this| {
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
                    .child(div().flex_1().min_w_0().child(error.message.clone()))
            }));
        if self.collapsed {
            return card.automation_target(name);
        }

        let note = (self.thread.state == ThreadState::Addressed)
            .then(|| state_note(&self.thread))
            .flatten()
            .map(|note| {
                div()
                    .flex()
                    .gap(theme.spacing.md)
                    .px(theme.spacing.lg)
                    .py(px(7.))
                    .bg(c.blue_soft)
                    .border_b_1()
                    .border_color(c.line)
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg2)
                    .child(
                        div().pt(px(1.)).child(agents::glyph(
                            self.thread
                                .last_change
                                .as_ref()
                                .map(|change| &change.author),
                            px(13.),
                            &theme,
                        )),
                    )
                    .child(div().flex_1().min_w_0().child(note.to_string()))
            });
        // A comment on the whole file has no lines to show as they were; a card away from its
        // unchanged file shows the lines it is on.
        let snippet = ((self.outdated || self.unlisted.is_some())
            && !self.thread.anchor.text.is_empty())
        .then(|| {
            div()
                .mx(theme.spacing.lg)
                .mt(px(10.))
                .px(px(10.))
                .py(theme.spacing.sm)
                .rounded(theme.radii.control)
                .bg(c.recess)
                .font_family(theme.fonts.mono.clone())
                .text_size(px(11.5))
                .line_height(px(17.))
                .text_color(c.fg2)
                .children(anchor.text.iter().map(|line| {
                    div()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(line.replace('\t', "    "))
                }))
        });
        let mut comments = vec![comment(
            ElementId::Name(format!("{name}/body").into()),
            &self.thread.author,
            &self.thread.body,
            self.thread.created_at,
            self.now,
            true,
            self.thread.annotation,
            &theme,
            cx,
        )];
        for reply in self
            .thread
            .replies
            .iter()
            .filter(|reply| !reply.is_deleted())
        {
            comments.push(comment(
                ElementId::Name(format!("{name}/{}", reply.id).into()),
                &reply.author,
                &reply.body,
                reply.created_at,
                self.now,
                false,
                false,
                &theme,
                cx,
            ));
        }
        let reply_row = div()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .pt(theme.spacing.sm)
            .pr(theme.spacing.lg)
            .pb(px(10.))
            .pl(px(44.));
        let reply_row = match self.reply {
            Some(composer) => reply_row.child(div().flex_1().min_w_0().child(composer)),
            None => reply_row.child(self.reply_field(&name, &theme)),
        };
        card.children(note)
            .children(snippet)
            .children(comments)
            .child(reply_row)
            .automation_target(name)
    }
}

impl ThreadCard {
    /// An annotation's card (DESIGN §4.1 "Annotation card", PLAN §19.3): quieter than a
    /// thread's, since nothing waits on it — the diff's own surface, a `line` outline, no
    /// shadow and a blue rule down its left edge; a header with the agent, "Annotation", the
    /// anchor and **Ask**; the explanation and the agent's additions to it; the reply composer
    /// once the reviewer asks.
    fn render_annotation(self, cx: &mut App) -> AnyElement {
        let theme = cx.dv_theme().clone();
        let c = &theme.colors;
        let name = format!("thread/{}", self.thread.id);
        let anchor_text = match &self.path {
            Some(path) => format!(
                "{path} · {}",
                shown_anchor_label(&self.thread.anchor, self.lines, self.moved)
            ),
            None => shown_anchor_label(&self.thread.anchor, self.lines, self.moved),
        };
        let on_reply = self.on_reply.clone().filter(|_| !self.read_only);
        let ask = small_button(ElementId::Name(format!("{name}/ask").into()), "Ask", &theme)
            .when(self.focused && !self.read_only, |this| {
                this.child(kbd(keymap::label("r"), &theme))
            })
            .map(|this| match on_reply {
                Some(on_reply) => this.on_click(move |_, window, cx| on_reply(window, cx)),
                None => this.opacity(0.5).cursor_default(),
            })
            .automation_target(format!("{name}/ask"));
        let header = div()
            .flex()
            .items_center()
            .gap(theme.spacing.sm)
            .h(px(28.))
            .child(
                agents::mark(&self.thread.author, px(16.), px(10.), &theme)
                    .rounded(theme.radii.small),
            )
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.fg2)
                    .child(SharedString::from(
                        agents::shown_name(&self.thread.author).to_string(),
                    )),
            )
            .child(
                div()
                    .flex_none()
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg3)
                    .child("Annotation"),
            )
            .child(
                div()
                    .flex_1()
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
            .child(ask);
        let body = |id: String, text: &str| {
            crate::markdown::view(ElementId::Name(id.into()), text, cx)
                .text_size(theme.text.body.size)
                .line_height(px(20.))
                .text_color(c.fg2)
        };
        // The agent's own additions to its explanation, each under its name and time.
        let additions = self
            .thread
            .replies
            .iter()
            .filter(|reply| !reply.is_deleted())
            .map(|reply| {
                div()
                    .mt(theme.spacing.sm)
                    .pt(theme.spacing.sm)
                    .border_t_1()
                    .border_color(c.line)
                    .child(
                        div()
                            .text_spec(&theme.text.secondary)
                            .text_color(c.fg3)
                            .child(format!(
                                "{} · {}",
                                agents::shown_name(&reply.author),
                                relative_time(reply.created_at, self.now)
                            )),
                    )
                    .child(body(format!("{name}/{}", reply.id), &reply.body))
            })
            .collect::<Vec<_>>();
        let content = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .pl(theme.spacing.md)
            .pr(theme.spacing.sm)
            .pb(px(10.))
            .child(header)
            .child(body(format!("{name}/body"), &self.thread.body))
            .children(additions)
            .children(
                self.reply
                    .map(|composer| div().mt(theme.spacing.md).child(composer)),
            );
        div()
            .id(ElementId::Name(format!("{name}/card").into()))
            .w_full()
            .flex()
            .rounded(theme.radii.card)
            .border_1()
            .border_color(if self.focused { c.accent_ring } else { c.line })
            .bg(c.code)
            .overflow_hidden()
            .text_spec(&theme.text.body)
            .text_color(c.fg)
            .when_some(self.on_focus.clone(), |this, focus| {
                this.on_mouse_down(MouseButton::Left, move |_, window, cx| focus(window, cx))
            })
            .child(div().flex_none().w(px(2.)).bg(c.blue))
            .child(content)
            .automation_target(name)
    }

    /// Resolve and Reopen as the state allows (PLAN §2.5), disabled on a finished review.
    fn actions(&self, name: &str, theme: &DvTheme) -> Vec<AnyElement> {
        let wanted: &[(&str, &str, ThreadState)] = match self.thread.state {
            ThreadState::Open => &[("resolve", "Resolve", ThreadState::Resolved)],
            ThreadState::Addressed => &[
                ("resolve", "Resolve", ThreadState::Resolved),
                ("reopen", "Reopen", ThreadState::Open),
            ],
            ThreadState::Resolved => &[("reopen", "Reopen", ThreadState::Open)],
            // Its own card (`render_annotation`) offers Ask instead.
            ThreadState::Annotation => &[],
        };
        wanted
            .iter()
            .map(|&(key, label, state)| {
                let button = small_button(
                    ElementId::Name(format!("{name}/{key}").into()),
                    label,
                    theme,
                );
                let button = match (&self.on_state, self.read_only) {
                    (Some(on_state), false) => {
                        let on_state = on_state.clone();
                        button.on_click(move |_, window, cx| on_state(state, window, cx))
                    }
                    _ => button.opacity(0.5).cursor_default(),
                };
                button
                    .automation_target(format!("{name}/{key}"))
                    .into_any_element()
            })
            .collect()
    }

    /// The reply field before it is opened: a quiet "Reply…" (with `R` on the focused thread)
    /// that opens the reply composer.
    fn reply_field(&self, name: &str, theme: &DvTheme) -> AnyElement {
        let c = &theme.colors;
        let on_reply = self.on_reply.clone().filter(|_| !self.read_only);
        div()
            .id(ElementId::Name(format!("{name}/reply").into()))
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .h(theme.sizes.control_height)
            .px(px(10.))
            .rounded(theme.radii.control)
            .border_1()
            .border_color(c.ctrl_line)
            .bg(c.input_bg)
            .text_color(c.fg3)
            .child(div().flex_1().child(if self.read_only {
                "Replies are closed on a finished review"
            } else {
                "Reply…"
            }))
            .when(self.focused && !self.read_only, |this| {
                this.child(kbd(keymap::label("r"), theme))
            })
            .map(|this| match on_reply {
                Some(on_reply) => this
                    .cursor_text()
                    .on_click(move |_, window, cx| on_reply(window, cx)),
                None => this.opacity(0.6),
            })
            .automation_target(format!("{name}/reply"))
            .into_any_element()
    }
}

/// The chip that says where a thread stands: `open` with the accent ring dot, `addressed` in
/// blue, `resolved` with a green check (DESIGN §4.1).
fn state_chip(thread: &Thread, theme: &DvTheme) -> AnyElement {
    let c = &theme.colors;
    // "Addressed by claude-code" truncates once the anchor beside it has none left (the card's
    // header).
    let label = div().min_w_0().truncate().child(state_label(thread));
    match thread.state {
        ThreadState::Open => chip(c.accent_text, c.accent_soft, theme)
            .child(
                div()
                    .size(px(7.))
                    .rounded_full()
                    .border(px(1.5))
                    .border_color(c.accent),
            )
            .child(label),
        ThreadState::Addressed => chip(c.blue, c.blue_soft, theme)
            .child(icon(DvIcon::Check, px(11.), c.blue))
            .child(label),
        ThreadState::Resolved => chip(c.green, c.green_soft, theme)
            .child(icon(DvIcon::CircleCheck, px(12.), c.green))
            .child(label),
        ThreadState::Annotation => chip(c.blue, c.blue_soft, theme)
            .child(icon(DvIcon::Bot, px(12.), c.blue))
            .child(label),
    }
    .min_w_0()
    .flex_shrink_1()
    .font_weight(FontWeight::SEMIBOLD)
    .into_any_element()
}

/// One comment: the avatar, the author's name and role, the time, and the body as Markdown.
/// `annotation`: the opening comment of a thread that began as the agent's annotation, which
/// says so in a quiet tag (DESIGN §4.1).
#[allow(clippy::too_many_arguments)]
fn comment(
    id: ElementId,
    author: &Author,
    body: &str,
    at: u64,
    now: u64,
    first: bool,
    annotation: bool,
    theme: &DvTheme,
    cx: &App,
) -> Div {
    let c = &theme.colors;
    let role = match author.kind {
        AuthorKind::Reviewer => div()
            .text_spec(&theme.text.secondary)
            .text_color(c.fg3)
            .child(role_word(author)),
        AuthorKind::Agent | AuthorKind::Github => tag(role_tag(author).to_string(), theme),
    };
    div()
        .flex()
        .gap(px(10.))
        .pt(if first { px(10.) } else { theme.spacing.md })
        .px(theme.spacing.lg)
        .pb(theme.spacing.xs)
        .child(avatar(author, theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.sm)
                        .h(px(18.))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(SharedString::from(agents::shown_name(author).to_string())),
                        )
                        .child(role)
                        .when(annotation, |this| this.child(tag("annotation", theme)))
                        .child(
                            div()
                                .text_spec(&theme.text.secondary)
                                .text_color(c.fg3)
                                .child(relative_time(at, now)),
                        ),
                )
                .child(
                    crate::markdown::view(id, body, cx)
                        .text_size(theme.text.body.size)
                        .line_height(px(20.))
                        .text_color(c.fg),
                ),
        )
}

/// A 22px avatar: a round initial for people (the reviewer, GitHub), a square with its model
/// family's mark, else the bot glyph, for an agent (the board's `avatarKyle` / `avatarAgent`,
/// [`agents::mark`]).
fn avatar(author: &Author, theme: &DvTheme) -> AnyElement {
    let c = &theme.colors;
    let base = div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(22.));
    match author.kind {
        AuthorKind::Agent => agents::mark(author, px(22.), px(13.), theme)
            .rounded(theme.radii.control)
            .into_any_element(),
        AuthorKind::Reviewer | AuthorKind::Github => base
            .rounded_full()
            .bg(if author.kind == AuthorKind::Reviewer {
                c.accent_soft
            } else {
                c.layer_strong
            })
            .text_color(if author.kind == AuthorKind::Reviewer {
                c.accent_text
            } else {
                c.fg2
            })
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .child(initial(&author.name))
            .into_any_element(),
    }
}

/// The board's `agentTag`: 16 high, a `line2` outline, 11/500 in `fg2`.
fn tag(text: impl Into<SharedString>, theme: &DvTheme) -> Div {
    let c = &theme.colors;
    div()
        .flex_none()
        .flex()
        .items_center()
        .h(px(16.))
        .px(px(5.))
        .rounded(theme.radii.small)
        .border_1()
        .border_color(c.line2)
        .text_color(c.fg2)
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .child(text.into())
}

/// The composer (see the module docs): its field, why the last post failed, and Cancel /
/// Comment. Standalone (a new thread) it is a card with its heading; `compact` (a reply) it is
/// the field and the buttons alone.
#[derive(IntoElement)]
pub struct ComposerBox {
    input: Entity<TextareaState>,
    title: String,
    action: &'static str,
    posting: bool,
    error: Option<Failure>,
    compact: bool,
    /// In place of "Markdown" in the footer: what happens to the text ("claude-code reads it
    /// with the round", asking about an annotation), which matters more in a narrow card.
    hint: Option<String>,
    on_post: Handler,
    on_cancel: Handler,
}

impl ComposerBox {
    /// A composer over `input`, headed `title`, its primary button `action` ("Comment",
    /// "Reply").
    pub fn new(
        input: Entity<TextareaState>,
        title: String,
        action: &'static str,
        on_post: impl Fn(&mut Window, &mut App) + 'static,
        on_cancel: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            input,
            title,
            action,
            posting: false,
            error: None,
            compact: false,
            hint: None,
            on_post: Rc::new(on_post),
            on_cancel: Rc::new(on_cancel),
        }
    }

    /// A post is on its way: the button says so and takes no clicks.
    pub fn posting(mut self, posting: bool) -> Self {
        self.posting = posting;
        self
    }

    pub fn error(mut self, error: Option<Failure>) -> Self {
        self.error = error;
        self
    }

    /// Inside a thread card: no heading, no card of its own.
    pub fn compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }

    /// Words in place of "Markdown" in the footer.
    pub fn hint(mut self, hint: Option<String>) -> Self {
        self.hint = hint;
        self
    }
}

impl RenderOnce for ComposerBox {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.dv_theme().clone();
        let c = &theme.colors;
        let (on_post, on_cancel) = (self.on_post.clone(), self.on_cancel.clone());
        // The field pads its own text (gpui-component's multi-line paddings).
        let field = div()
            .w_full()
            .rounded(theme.radii.control)
            .border_1()
            .border_color(c.ctrl_line)
            .bg(c.input_bg)
            .child(
                Textarea::new(&self.input)
                    .appearance(false)
                    .aria_label(if self.compact { "Reply" } else { "Comment" })
                    .font_family(theme.fonts.ui.clone())
                    .text_size(theme.text.body.size)
                    .line_height(px(20.))
                    .text_color(c.fg),
            )
            .automation_target("composer/input");
        let error = self.error.map(|error| {
            div()
                .mt(theme.spacing.sm)
                .flex()
                .items_center()
                .gap(theme.spacing.sm)
                .text_spec(&theme.text.secondary)
                .text_color(c.red)
                .child(icon(DvIcon::TriangleAlert, px(13.), c.red))
                .child(div().flex_1().min_w_0().child(error.message))
        });
        let post_handler = on_post.clone();
        let footer = div()
            .mt(theme.spacing.md)
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg3)
                    .child(self.hint.unwrap_or_else(|| "Markdown".to_string())),
            )
            .child(
                small_button("composer-cancel", "Cancel", &theme)
                    .child(kbd(keymap::label("escape"), &theme))
                    .on_click({
                        let on_cancel = on_cancel.clone();
                        move |_, window, cx| on_cancel(window, cx)
                    })
                    .automation_target("composer/cancel"),
            )
            .child(
                primary_button("composer-post", !self.posting, &theme)
                    .h(theme.sizes.control_small_height)
                    .px(theme.spacing.md)
                    .text_spec(&theme.text.secondary)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if self.posting {
                        "Posting…"
                    } else {
                        self.action
                    })
                    .child(kbd_on_accent(keymap::label("secondary-enter"), &theme))
                    .when(!self.posting, |this| {
                        this.on_click(move |_, window, cx| post_handler(window, cx))
                    })
                    .automation_target("composer/post"),
            );
        let root = div()
            .key_context(keymap::COMPOSER)
            .on_action(move |_: &Post, window, cx| on_post(window, cx))
            .on_action(move |_: &Cancel, window, cx| on_cancel(window, cx))
            .w_full()
            .flex()
            .flex_col()
            .text_spec(&theme.text.body)
            .text_color(c.fg);
        let root = if self.compact {
            root.child(field).children(error).child(footer)
        } else {
            root.rounded(theme.radii.card)
                .border_1()
                .border_color(c.accent_ring)
                .bg(c.thread)
                .shadow(theme.shadows.card.clone())
                .overflow_hidden()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.md)
                        .h(px(32.))
                        .px(theme.spacing.lg)
                        .border_b_1()
                        .border_color(c.line)
                        .text_spec(&theme.text.secondary)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(c.fg2)
                        .child(icon(DvIcon::MessageSquarePlus, px(13.), c.fg3))
                        .child(self.title),
                )
                .child(
                    div()
                        .px(theme.spacing.lg)
                        .pt(px(10.))
                        .pb(px(10.))
                        .child(field)
                        .children(error)
                        .child(footer),
                )
        };
        root.automation_target("composer")
    }
}
