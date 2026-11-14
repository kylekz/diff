//! The switcher's panel (DESIGN §3: `overlay` over the scrim, radius 12, the overlay shadow):
//! the filter field, the repositories and their rows, and a footer with "Open folder…" and the
//! keys.

use gpui::{
    AnyElement, Context, ElementId, FontWeight, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, anchored, deferred, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::Input;

use super::branches::Mode;
use super::items::{Entry, Item, ItemKind, RepoHeader, RepoSummary, TargetKind};
use super::{MenuTarget, SWITCHER_WIDTH, Switcher, SwitcherMenu};

/// The menus' layer: above the switcher, itself at the shell's menu priority (4).
const MENU_PRIORITY: usize = 5;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    count_badge, icon, kbd, location_chip, menu_item, menu_panel, skeleton, status_chip, totals,
};
use crate::icons::DvIcon;
use crate::keymap;
use crate::theme::{ActiveTheme as _, DvTheme, StyledText as _};

impl Render for Switcher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.dv_theme().clone();
        let c = &theme.colors;
        let viewport = window.viewport_size();
        let max_height = (viewport.height - theme.sizes.title_bar_height - px(56.)).max(px(200.));
        div()
            .key_context(keymap::SWITCHER)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::remove_selected))
            .on_action(cx.listener(Self::ask_close))
            .w(SWITCHER_WIDTH)
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
            .child(self.render_footer(&theme, cx))
            .children(
                self.menu
                    .clone()
                    .map(|menu| self.render_menu(menu, &theme, cx)),
            )
            .automation_target("switcher")
    }
}

impl Switcher {
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
                        .aria_label("Filter repositories and reviews")
                        .px_0()
                        .text_size(px(14.)),
                ),
            )
            .automation_target("switcher/filter")
    }

    fn render_list(&self, theme: &DvTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let list = div()
            .id("switcher-list")
            .track_scroll(&self.scroll)
            .flex_shrink(1.)
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .p(theme.spacing.sm);
        let branch_rows = self.listing.items.is_empty() && !self.listing.entries.is_empty();
        if branch_rows && self.mode != Mode::Repos {
            let title = if self.query.trim().is_empty() {
                "No branches to compare".to_string()
            } else {
                format!("No branches match “{}”", self.query.trim())
            };
            return list
                .child(repo_header(&self.listing.repos[0], true, None, theme))
                .child(
                    div()
                        .flex()
                        .justify_center()
                        .py(theme.spacing.xl)
                        .text_color(c.fg2)
                        .child(title),
                )
                .automation_target("switcher/empty");
        }
        if self.listing.entries.is_empty() {
            let known = !self.reviews.read(cx).rows().is_empty();
            let (title, note) = if known {
                (
                    format!("No repositories or reviews match “{}”", self.query.trim()),
                    "Try part of a repository's name, a branch or a status.".to_string(),
                )
            } else {
                (
                    "No repositories yet".to_string(),
                    "Open a folder with a git repository to review its changes.".to_string(),
                )
            };
            return list
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(theme.spacing.xs)
                        .py(theme.spacing.xxl)
                        .child(div().text_color(c.fg2).child(title))
                        .child(
                            div()
                                .text_spec(&theme.text.secondary)
                                .text_color(c.fg3)
                                .child(note),
                        ),
                )
                .automation_target("switcher/empty");
        }
        let lines: Vec<AnyElement> = self
            .listing
            .entries
            .iter()
            .enumerate()
            .filter_map(|(line, entry)| match *entry {
                Entry::Repo(repo) => self
                    .listing
                    .repos
                    .get(repo)
                    .map(|header| self.repo_header(header, line == 0, theme, cx)),
                Entry::Item(ix) => self
                    .listing
                    .items
                    .get(ix)
                    .map(|item| self.render_item(ix, item, theme, cx)),
            })
            .collect();
        list.children(lines).into_any_element()
    }

    fn render_item(
        &self,
        ix: usize,
        item: &Item,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let selected = ix == self.selected;
        let glyph = match &item.kind {
            ItemKind::Target(offer) => match offer.kind {
                TargetKind::Working => DvIcon::FileDiff,
                // Not a check: the source picker's check marks the current target.
                TargetKind::Staged => DvIcon::Package,
                TargetKind::Branch => DvIcon::GitBranch,
            },
            ItemKind::Review { .. } => DvIcon::GitCommit,
            ItemKind::Start { .. } => DvIcon::Play,
            ItemKind::Retry { .. } => DvIcon::RotateCw,
            ItemKind::OpenFolder => DvIcon::FolderOpen,
            ItemKind::CompareBranches => DvIcon::ArrowLeftRight,
            ItemKind::Branch { .. } | ItemKind::Base { .. } => DvIcon::GitBranch,
        };
        // Asked to close its review (`Ctrl Shift W`): the row says so instead of its badge.
        let asking = self.closing() == Some(item.key.as_str());
        let right: Option<AnyElement> = match item.review() {
            Some(_) if asking => Some(
                div()
                    .flex_none()
                    .font_weight(FontWeight::MEDIUM)
                    .child("Close this review?")
                    .into_any_element(),
            ),
            Some(badge) => Some(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .child(status_chip(badge.status, theme))
                    .child(
                        div()
                            .text_spec(&theme.text.secondary)
                            .text_color(c.fg3)
                            .child(format!("round {}", badge.round)),
                    )
                    .when(badge.open_threads > 0, |this| {
                        this.child(
                            count_badge(theme)
                                .child(icon(DvIcon::MessageSquare, px(10.), c.accent_text))
                                .child(format!("{} open", badge.open_threads)),
                        )
                    })
                    .into_any_element(),
            ),
            None if matches!(item.kind, ItemKind::Target(_) | ItemKind::Base { .. })
                && selected =>
            {
                Some(
                    div()
                        .text_spec(&theme.text.secondary)
                        .text_color(c.fg3)
                        .child("New review")
                        .into_any_element(),
                )
            }
            None => None,
        };
        let footer = item.kind == ItemKind::OpenFolder;
        // A Start or Retry whose start runs is inert (DESIGN §4.1): dimmed, no hover, and a
        // click (like Enter) does nothing.
        let inert = item.is_starting();
        let hover = c.control_hover;
        let glyph_color = match (inert, selected) {
            (true, _) => c.fg3,
            (false, true) => c.fg,
            (false, false) => c.fg2,
        };
        // A row beside an open review has a menu (module docs): a right-click, or its `⋯`.
        let group = SharedString::from(format!("switcher-item-{}", item.key));
        let more = item.review().is_some().then(|| {
            let key = item.key.clone();
            more_button(
                ElementId::Name(format!("switcher-item-more-{}", item.key).into()),
                group.clone(),
                self.row_menu_is(&item.key),
                theme,
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    if this.row_menu_is(&key) {
                        this.close_menu(cx);
                    } else {
                        this.open_row_menu(&key, event.position, cx);
                    }
                }),
            )
            .automation_target(format!("switcher/{}/more", item.key))
        });
        div()
            .id(ElementId::NamedInteger("switcher-item".into(), ix as u64))
            .group(group)
            .when(item.review().is_some(), |this| {
                let key = item.key.clone();
                this.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.open_row_menu(&key, event.position, cx);
                    }),
                )
            })
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .h(px(30.))
            .pl(if footer { theme.spacing.md } else { px(30.) })
            .pr(theme.spacing.md)
            .rounded(theme.radii.control)
            .when(!inert, |this| this.cursor_pointer())
            .map(|this| {
                if selected {
                    this.bg(c.sel)
                } else if inert {
                    this
                } else {
                    this.hover(move |style| style.bg(hover))
                }
            })
            .on_click(cx.listener(move |this, _, window, cx| this.pick(ix, window, cx)))
            .child(icon(glyph, px(14.), glyph_color))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .when(footer, |this| this.font_weight(FontWeight::MEDIUM))
                    .when(inert, |this| this.text_color(c.fg3))
                    .child(SharedString::from(item.label().to_string())),
            )
            .children(item.detail().map(|detail| {
                div()
                    .flex_none()
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg3)
                    .child(SharedString::from(detail))
            }))
            .child(div().flex_1())
            .children(right)
            .children(more)
            .automation_target(format!("switcher/{}", item.key))
    }

    fn render_footer(&self, theme: &DvTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let open_folder = self
            .listing
            .items
            .iter()
            .position(|item| item.kind == ItemKind::OpenFolder)
            .and_then(|ix| {
                let item = self.listing.items.get(ix)?;
                Some(self.render_item(ix, item, theme, cx))
            });
        let hint = |keys: Vec<&'static str>, what: &'static str| {
            div()
                .flex()
                .items_center()
                .gap(px(4.))
                .children(keys.into_iter().map(|key| kbd(keymap::label(key), theme)))
                .child(what)
        };
        let hints = div()
            .flex()
            .items_center()
            .gap(theme.spacing.lg)
            .pr(theme.spacing.sm)
            .text_spec(&theme.text.secondary)
            .text_color(c.fg3);
        // A row asked to close its review: only its two answers.
        let hints = if self.closing().is_some() {
            hints
                .child(hint(vec!["enter"], "close review"))
                .child(hint(vec!["escape"], "keep"))
        } else {
            hints
                .when(self.closable(), |this| {
                    this.child(hint(vec!["secondary-shift-w"], "close review"))
                })
                .when(self.removable().is_some(), |this| {
                    this.child(hint(vec!["secondary-shift-backspace"], "remove"))
                })
                .child(hint(vec!["up", "down"], "move"))
                .child(hint(
                    vec!["enter"],
                    if matches!(self.mode, Mode::Branches { .. }) {
                        "next"
                    } else {
                        "open"
                    },
                ))
                .child(hint(
                    vec!["escape"],
                    match self.mode {
                        Mode::Repos => "close",
                        Mode::Branches { .. } if !self.back_to_repos => "close",
                        _ => "back",
                    },
                ))
        };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .h(px(44.))
            .px(theme.spacing.sm)
            .border_t_1()
            .border_color(c.line)
            .children(open_folder)
            .child(div().flex_1())
            // Why the last close failed stands in for the keys until the selection moves.
            .child(match self.close_failure() {
                Some(why) => div()
                    .min_w_0()
                    .pr(theme.spacing.sm)
                    .text_spec(&theme.text.secondary)
                    .child(problem(
                        &format!("Couldn't close the review: {why}"),
                        c.red,
                        theme,
                    ))
                    .automation_target("switcher/close_failure"),
                None => hints.into_any_element(),
            })
            .into_any_element()
    }

    /// The open menu at the pointer: a repository header's **Remove from list**, disabled for
    /// the open review's repository with the reason under it, or a review row's **Close
    /// review**.
    fn render_menu(
        &self,
        menu: SwitcherMenu,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let location = match menu.target {
            MenuTarget::Repo(location) => location,
            MenuTarget::Review { row } => {
                let close = menu_item(
                    "switcher-menu-close",
                    "Close review",
                    DvIcon::X,
                    true,
                    theme,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.close_review(&row, cx)))
                .automation_target("switcher/menu/close");
                let panel = menu_panel("switcher-menu", theme)
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu(cx)))
                    .child(close)
                    .automation_target("switcher/menu");
                return menu_layer(menu.position, panel);
            }
        };
        let removable = self.can_remove(&location);
        let remove = menu_item(
            "switcher-menu-remove",
            "Remove from list",
            DvIcon::X,
            removable,
            theme,
        )
        .when(removable, |this| {
            this.on_click(cx.listener(move |this, _, _, cx| this.remove(&location, cx)))
        })
        .automation_target("switcher/menu/remove");
        let panel = menu_panel("switcher-menu", theme)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_menu(cx)))
            .child(remove)
            .when(!removable, |this| {
                this.child(
                    div()
                        .px(theme.spacing.md)
                        .pb(theme.spacing.xs)
                        .text_spec(&theme.text.secondary)
                        .text_color(c.fg3)
                        .child("Its review is open"),
                )
            })
            .automation_target("switcher/menu");
        menu_layer(menu.position, panel)
    }

    /// A repository's header: its name, its path dimmed, its location chip, and a line on its
    /// uncommitted changes or why it can't be opened; in the repositories' list a right-click,
    /// or its `⋯` under the pointer, opens its menu (the branch picker's header has none).
    fn repo_header(
        &self,
        header: &RepoHeader,
        first: bool,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.mode != Mode::Repos {
            return repo_header(header, first, None, theme);
        }
        let group = SharedString::from(format!("switcher-repo-{}", header.key));
        let more = more_button(
            ElementId::Name(format!("switcher-more-{}", header.key).into()),
            group.clone(),
            self.repo_menu_is(&header.location),
            theme,
        )
        .on_mouse_down(MouseButton::Left, {
            let location = header.location.clone();
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                if this.repo_menu_is(&location) {
                    this.close_menu(cx);
                } else {
                    this.open_menu(location.clone(), event.position, cx);
                }
            })
        })
        .automation_target(format!("switcher/repo/{}/more", header.key));
        let location = header.location.clone();
        div()
            .id(ElementId::Name(group.clone()))
            .group(group)
            .child(repo_header(
                header,
                first,
                Some(more.into_any_element()),
                theme,
            ))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.open_menu(location.clone(), event.position, cx);
                }),
            )
            .into_any_element()
    }
}

/// A menu's `panel` at `position` (the pointer), kept inside the window, above the switcher.
fn menu_layer(position: gpui::Point<gpui::Pixels>, panel: impl IntoElement) -> AnyElement {
    deferred(
        anchored()
            .position(position)
            .snap_to_window_with_margin(px(8.))
            .child(panel),
    )
    .with_priority(MENU_PRIORITY)
    .into_any_element()
}

/// A `⋯` that opens a menu: a 22-square ghost button shown under the pointer of `group` (its
/// header or row) and while its menu is `open` (DESIGN §4.1).
fn more_button(
    id: ElementId,
    group: SharedString,
    open: bool,
    theme: &DvTheme,
) -> gpui::Stateful<gpui::Div> {
    let c = &theme.colors;
    let hover = c.control_hover;
    div()
        .id(id)
        .flex_none()
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme.radii.control)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .when(!open, |this| {
            this.opacity(0.)
                .group_hover(group, |style| style.opacity(1.))
        })
        .when(open, |this| this.bg(hover))
        .child(icon(DvIcon::Ellipsis, px(14.), c.fg2))
}

/// A repository's header: its name, its path dimmed, its location chip, `more` at the end of
/// its first line, and a line on its uncommitted changes or why it can't be opened.
fn repo_header(
    header: &RepoHeader,
    first: bool,
    more: Option<AnyElement>,
    theme: &DvTheme,
) -> AnyElement {
    let c = &theme.colors;
    let summary: AnyElement = match &header.summary {
        RepoSummary::Loading => skeleton(px(160.), px(10.), theme),
        RepoSummary::Unreachable(reason) => {
            let text = match &header.distro {
                Some(distro) => format!("{distro} · {reason}"),
                None => reason.clone(),
            };
            problem(&text, c.yellow, theme)
        }
        RepoSummary::Stopped(distro) => {
            problem(&format!("{distro} isn't running"), c.yellow, theme)
        }
        RepoSummary::Starting(line) => div()
            .text_color(c.fg3)
            .child(SharedString::from(line.clone()))
            .into_any_element(),
        RepoSummary::Error(message) => problem(message, c.red, theme),
        RepoSummary::Clean => div()
            .text_color(c.fg3)
            .child("Working tree clean")
            .into_any_element(),
        RepoSummary::Changes(working) => div()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .child(div().text_color(c.fg2).child(format!(
                "uncommitted changes · {}",
                if working.files == 1 {
                    "1 file".to_string()
                } else {
                    format!("{} files", working.files)
                }
            )))
            .child(totals(
                working.additions,
                working.deletions,
                px(11.5),
                theme,
            ))
            .into_any_element(),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .pt(if first { theme.spacing.xs } else { px(10.) })
        .pb(theme.spacing.xs)
        .px(theme.spacing.md)
        .child(
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .child(icon(DvIcon::Folder, px(14.), c.fg2))
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(SharedString::from(header.name.clone())),
                )
                .children(
                    header
                        .distro
                        .as_deref()
                        .map(|distro| location_chip(distro, theme)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis_start()
                        .text_spec(&theme.text.secondary)
                        .text_color(c.fg3)
                        .child(SharedString::from(header.shown_path.clone())),
                )
                .children(more),
        )
        .child(
            div()
                .pl(px(22.))
                .h(px(16.))
                .flex()
                .items_center()
                .text_spec(&theme.text.secondary)
                .child(summary),
        )
        .automation_target(format!("switcher/repo/{}", header.key))
}

/// A one-line problem with its glyph.
fn problem(text: &str, hue: gpui::Hsla, theme: &DvTheme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(theme.spacing.sm)
        .min_w_0()
        .child(icon(DvIcon::TriangleAlert, px(12.), hue))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(theme.colors.fg2)
                .child(SharedString::from(text.to_string())),
        )
        .into_any_element()
}
