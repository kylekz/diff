//! The custom title bar (DESIGN §2, §4.1): 40px of `chrome` holding dv's mark, the review
//! switcher button (`repo · working tree vs main`, a location chip for WSL, `Ctrl O`), the
//! source picker, the centred command field (`Ctrl K`), **PRs** (`Ctrl G`, `super::pr`), the
//! `⋯` that opens Settings (`Ctrl ,`) and, on Windows, the caption buttons.
//! The whole bar is a window drag region; its controls occlude it so they stay clickable.
//!
//! The switcher and the source picker hang under their buttons while open, and the file
//! finder (`Ctrl P`) under the command field
//! (`deferred(anchored())` in a zero-size box at the button's bottom-left corner, so they follow
//! the layout), above everything else, an open sheet included.
//!
//! **Room.** The command field is centred on the window unless the left group needs more than
//! the half that leaves it: then the field moves right as far as the group needs, short of the
//! caption buttons ([`field_left`], the group's width measured from its texts). The left group
//! never runs under the field: once the field can't move further, the switcher's label
//! truncates (the target, then the repository's name).
//!
//! The caption buttons are gpui `WindowControlArea`s, so Windows itself handles their clicks,
//! hover and Snap Layouts on Maximize (the pattern of Zed's `platform_title_bar`). macOS draws
//! its traffic lights over the left edge, which the bar leaves room for. On Linux the bar moves
//! the window itself and shows the caption buttons while dv draws the decorations
//! (`super::frame`).

use dv_core::Location;
use gpui::{
    Anchor, AnyElement, Context, FontWeight, HighlightStyle, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, RenderOnce, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, Window, WindowControlArea, anchored,
    deferred, div, point, prelude::FluentBuilder as _, px,
};

use super::Shell;
use super::render::MENU_PRIORITY;
use crate::actions::shell::ToggleSwitcher;
use crate::automation::AutomationTargetExt as _;
use crate::components::{icon, kbd, location_chip};
use crate::icons::DvIcon;
use crate::keymap;
use crate::theme::{DvTheme, StyledText as _};

/// What the switcher button names.
enum SwitcherLabel {
    Nothing,
    /// A review opening: its repository.
    Opening {
        name: String,
        distro: Option<String>,
    },
    /// The open review: its repository and its target ("working tree vs main").
    Review {
        name: String,
        target: String,
        distro: Option<String>,
    },
}

fn distro(location: &Location) -> Option<String> {
    match location {
        Location::Wsl { distro, .. } => Some(distro.clone()),
        Location::Local(_) => None,
    }
}

/// How many characters of a PR review's label (`#482 Title`) the title bar shows: about what
/// a long branch's label comes to ("feature/cache-the-config vs main"). A title is as long
/// as its author likes, and the label moves the command field right as far as it needs
/// (module docs, "Room"): capped, a title moves it no further than a branch's name does,
/// and **PRs** keeps its words in a window that has room for them. The strip under the
/// bar shows the title whole.
const PR_LABEL_CHARS: usize = 48;

/// `label`, a PR review's, at [`PR_LABEL_CHARS`] characters at most, then "…".
fn capped(label: &str) -> String {
    match label.char_indices().nth(PR_LABEL_CHARS) {
        Some((at, _)) => format!("{}…", label[..at].trim_end()),
        None => label.to_string(),
    }
}

/// The open review's target as the title bar's label names it: its label, a PR review's
/// capped ([`capped`]).
pub fn title_target(session: &crate::session::ReviewSession) -> String {
    let target = session.target_label();
    if session.review().is_pr_review() {
        capped(&target)
    } else {
        target
    }
}

impl Shell {
    /// The title bar of the main window.
    pub(super) fn render_title_bar(
        &self,
        window: &mut Window,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let left_inset = if cfg!(target_os = "macos") {
            theme.sizes.traffic_lights_inset
        } else {
            px(14.)
        };
        let label = match (&self.session, &self.opening) {
            (_, Some(opening)) => SwitcherLabel::Opening {
                name: opening.location.short_name(),
                distro: distro(&opening.location),
            },
            (Some(session), None) => {
                let session = session.read(cx);
                let location = &session.backend().repo().location;
                SwitcherLabel::Review {
                    name: location.short_name(),
                    target: title_target(session),
                    distro: distro(location),
                }
            }
            (None, None) => SwitcherLabel::Nothing,
        };
        let has_review = self.session.is_some();
        // The left group stops short of the command field, which moves right when the group
        // needs it to (module docs, "Room").
        let needed = left_group_width(&label, left_inset, has_review, window, theme);
        // The bar spans the window inside the frame Linux's client-side decorations draw.
        let frame = gpui_component::window_paddings(window);
        let width = window.viewport_size().width - frame.left - frame.right;
        let right = SETTINGS_BUTTON_ROOM
            + theme.sizes.caption_button_width * caption_buttons(window).len() as f32;
        // **PRs** gives up its label and keycap before the left group gives up anything.
        let prs_room = super::pr::prs_button_room(window, theme);
        let prs_compact = prs_is_compact(
            width,
            theme.sizes.command_field_width,
            needed,
            right + prs_room,
        );
        let field = field_left(
            width,
            theme.sizes.command_field_width,
            needed,
            right
                + if prs_compact {
                    super::pr::PRS_COMPACT_ROOM
                } else {
                    prs_room
                },
        );
        let room = field - FIELD_GAP;
        div()
            .id("titlebar")
            .relative()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(theme.sizes.title_bar_height)
            .bg(c.chrome)
            .border_b_1()
            .border_color(c.line)
            .window_control_area(WindowControlArea::Drag)
            .when(cfg!(target_os = "linux"), |bar| {
                super::frame::moves_window(bar, window, cx)
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .min_w_0()
                    .max_w(room)
                    .child(mark(left_inset, theme))
                    .child(div().flex_none().w(px(1.)).h(px(18.)).bg(c.line2))
                    .child(self.render_switcher_button(label, theme, cx))
                    .child(
                        div()
                            .flex_none()
                            .ml(px(2.))
                            .child(kbd(keymap::label("secondary-o"), theme)),
                    )
                    .when(has_review, |this| {
                        this.child(self.render_source_button(theme, cx))
                    }),
            )
            .child(div().flex_1())
            .child(self.render_prs_button(theme, prs_compact, cx))
            .child(
                div()
                    .flex_none()
                    .mr(px(6.))
                    .child(self.render_settings_button(theme, cx)),
            )
            .child(WindowControls::new(theme))
            // Centred on the window (or as far right as the left group needs), not between its
            // neighbours. The wrapper has no hitbox, so the drag region still reaches everywhere
            // but the field.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(field)
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .relative()
                            .child(command_field(theme, cx))
                            .children(self.file_finder.as_ref().map(|overlay| {
                                hang_centred(
                                    overlay.view.clone().into_any_element(),
                                    theme.sizes.command_field_width,
                                    crate::file_finder::FINDER_WIDTH,
                                )
                            }))
                            .children(self.palette.as_ref().map(|overlay| {
                                hang_centred(
                                    overlay.view.clone().into_any_element(),
                                    theme.sizes.command_field_width,
                                    crate::palette::PALETTE_WIDTH,
                                )
                            })),
                    ),
            )
            .automation_target("titlebar")
    }

    /// The switcher button (`Ctrl O`, PLAN §11 `switcher`), and the switcher under it while open.
    fn render_switcher_button(
        &self,
        label: SwitcherLabel,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let hover = c.control_hover;
        let text: AnyElement = match label {
            SwitcherLabel::Nothing => div()
                .text_color(c.fg2)
                .child("No review open")
                .into_any_element(),
            SwitcherLabel::Opening { name, distro } => div()
                .flex()
                .items_center()
                .gap(px(6.))
                .min_w_0()
                .overflow_hidden()
                .child(repo_name(name))
                .children(distro.map(|distro| location_chip(&distro, theme)))
                .child(
                    after_name()
                        .child(div().flex_none().text_color(c.fg3).child("·"))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(c.fg3)
                                .child("opening…"),
                        ),
                )
                .into_any_element(),
            SwitcherLabel::Review {
                name,
                target,
                distro,
            } => div()
                .flex()
                .items_center()
                .gap(px(6.))
                .min_w_0()
                .overflow_hidden()
                .child(repo_name(name))
                .children(distro.map(|distro| location_chip(&distro, theme)))
                .child(
                    after_name()
                        .child(div().flex_none().text_color(c.fg3).child("·"))
                        .child(target_label(&target, c.fg2, c.fg3)),
                )
                .into_any_element(),
        };
        let open = self.switcher.as_ref().map(|overlay| overlay.view.clone());
        div()
            .relative()
            .flex()
            .min_w_0()
            .ml(theme.spacing.sm)
            .child(
                div()
                    .id("titlebar-switcher")
                    .role(Role::Button)
                    .aria_label(format!(
                        "Switch repository or review ({})",
                        keymap::label("secondary-o")
                    ))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .min_w_0()
                    .overflow_hidden()
                    .h(theme.sizes.control_height)
                    .px(theme.spacing.md)
                    .rounded(theme.radii.control)
                    .text_spec(&theme.text.control)
                    .text_color(c.fg)
                    // A control inside the drag region.
                    .occlude()
                    .cursor_pointer()
                    .when(open.is_some(), |this| this.bg(c.control_hover))
                    .hover(move |style| style.bg(hover))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_switcher(&ToggleSwitcher, window, cx)
                    }))
                    .child(icon(DvIcon::Folder, px(15.), c.fg2))
                    .child(text)
                    .child(icon(DvIcon::ChevronDown, px(12.), c.fg3))
                    .automation_target("titlebar/switcher"),
            )
            .children(open.map(|view| hang(view.into_any_element())))
            .into_any_element()
    }

    /// The source picker's button (DESIGN §2 `[source ▾]`), and the menu under it while open.
    fn render_source_button(&self, theme: &DvTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let hover = c.control_hover;
        let open = self
            .source_picker
            .as_ref()
            .map(|overlay| overlay.view.clone());
        div()
            .relative()
            .flex_none()
            .ml(theme.spacing.sm)
            .child(
                div()
                    .id("titlebar-source")
                    .role(Role::Button)
                    .aria_label("Change what this review compares")
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(theme.sizes.control_height)
                    .px(theme.spacing.md)
                    .rounded(theme.radii.control)
                    .occlude()
                    .cursor_pointer()
                    .when(open.is_some(), |this| this.bg(c.control_hover))
                    .hover(move |style| style.bg(hover))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.source_picker.is_some() {
                            this.close_menu(window, cx);
                        } else {
                            this.open_source_picker(window, cx);
                        }
                    }))
                    .child(icon(DvIcon::ArrowLeftRight, px(15.), c.fg2))
                    .child(icon(DvIcon::ChevronDown, px(12.), c.fg3))
                    .automation_target("titlebar/source"),
            )
            .children(open.map(|view| hang(view.into_any_element())))
            .into_any_element()
    }
}

/// The room kept between the left group and the command field, and between the field and the
/// caption buttons.
const FIELD_GAP: Pixels = px(12.);

/// The room the `⋯` (Settings, `shell::setup`) takes at the bar's right, its margin included.
const SETTINGS_BUTTON_ROOM: Pixels = px(34.);

/// Where the command field's left edge goes (module docs, "Room"): centred in a window
/// `width` wide, or as far right as a left group `needed` wide needs, but never closer than
/// [`FIELD_GAP`] to the `right` reserved at the window's right edge (the caption buttons).
pub fn field_left(width: Pixels, field: Pixels, needed: Pixels, right: Pixels) -> Pixels {
    let centred = (width - field) / 2.;
    let furthest = width - right - FIELD_GAP - field;
    centred.max((needed + FIELD_GAP).min(furthest))
}

/// Whether **PRs** draws as its glyph alone: when a left group `needed` wide can't have the
/// command field where it needs it with `right` reserved at the window's right edge, the
/// whole button counted in ([`field_left`]'s `furthest`).
pub fn prs_is_compact(width: Pixels, field: Pixels, needed: Pixels, right: Pixels) -> bool {
    needed + FIELD_GAP > width - right - FIELD_GAP - field
}

/// How wide the left group draws when nothing truncates: the mark, the switcher button with
/// its label, its keycap and the source picker's button (their paddings, gaps and glyphs as
/// they are laid out, the texts measured).
fn left_group_width(
    label: &SwitcherLabel,
    left_inset: Pixels,
    has_review: bool,
    window: &Window,
    theme: &DvTheme,
) -> Pixels {
    use crate::components::text_width;
    let control = &theme.text.control;
    let semibold = crate::theme::TextSpec {
        weight: FontWeight::SEMIBOLD,
        ..control.clone()
    };
    let chip = |text: &str| {
        let medium = crate::theme::TextSpec {
            weight: FontWeight::MEDIUM,
            ..theme.text.secondary.clone()
        };
        theme.spacing.sm * 2. + px(12.) + theme.spacing.xs + text_width(window, text, &medium)
    };
    let (gap, dot) = (px(6.), text_width(window, "·", control));
    let text = match label {
        SwitcherLabel::Nothing => text_width(window, "No review open", control),
        SwitcherLabel::Opening { name, distro } => {
            text_width(window, name, &semibold)
                + distro
                    .as_deref()
                    .map_or(px(0.), |distro| gap + chip(distro))
                + gap * 2.
                + dot
                + text_width(window, "opening…", control)
        }
        SwitcherLabel::Review {
            name,
            target,
            distro,
        } => {
            text_width(window, name, &semibold)
                + distro
                    .as_deref()
                    .map_or(px(0.), |distro| gap + chip(distro))
                + gap * 2.
                + dot
                + text_width(window, target, control)
        }
    };
    let mark = left_inset
        + px(18.)
        + theme.spacing.md
        + text_width(window, "dv", &theme.text.wordmark)
        + theme.spacing.lg;
    let switcher =
        theme.spacing.sm + theme.spacing.md * 2. + px(15.) + px(7.) * 2. + px(12.) + text;
    let keycap = px(2.)
        + (theme.spacing.xs + px(1.)) * 2.
        + px(2.)
        + text_width(window, &keymap::label("secondary-o"), &theme.text.kbd);
    let source = if has_review {
        theme.spacing.sm + theme.spacing.md * 2. + px(15.) + px(5.) + px(12.)
    } else {
        px(0.)
    };
    // A little over, so rounding never truncates a label that fits.
    mark + px(1.) + switcher + keycap + source + px(4.)
}

/// Hangs an overlay `width` wide centred under a control `under` wide (the file finder under the
/// command field), 8px down, kept inside the window.
fn hang_centred(overlay: AnyElement, under: Pixels, width: Pixels) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .bottom_0()
        .child(
            deferred(
                anchored()
                    .anchor(Anchor::TopLeft)
                    .offset(point((under - width) / 2., px(8.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(overlay),
            )
            .with_priority(MENU_PRIORITY),
        )
        .into_any_element()
}

/// Hangs an overlay under its button: from a zero-size box at the button's bottom-left corner,
/// 8px down (just clear of the title bar), kept inside the window.
fn hang(overlay: AnyElement) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .bottom_0()
        .child(
            deferred(
                anchored()
                    .anchor(Anchor::TopLeft)
                    .offset(point(px(0.), px(8.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(overlay),
            )
            .with_priority(MENU_PRIORITY),
        )
        .into_any_element()
}

/// The repository's name in the switcher button: it truncates at its end once what follows it
/// ([`after_name`]) has given up all its room, so nothing after the button (the keycap, the
/// source picker) is ever drawn over it (DESIGN §4.1).
fn repo_name(name: String) -> AnyElement {
    div()
        .min_w_0()
        .truncate()
        .font_weight(FontWeight::SEMIBOLD)
        .child(SharedString::from(name))
        .into_any_element()
}

/// How much more readily what follows the name gives way than the name (whose factor is 1).
/// Flex shrinking shares a shortfall by factor times size, so at this ratio the name's share
/// rounds to nothing until the label has none left; a factor of 0 instead would leave the name
/// unable to shrink at all (CSS flexbox §9.7 scales the shortfall by factors that sum below 1).
const AFTER_NAME_SHRINK: f32 = 1e20;

/// What follows the name ("· working tree vs main"): it truncates first, and the name only once
/// it has no room left.
fn after_name() -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .min_w_0()
        .flex_shrink(AFTER_NAME_SHRINK)
        .overflow_hidden()
}

/// A target label with its "vs" dimmed (`working tree vs main`), truncated at its end when the
/// bar is narrow.
fn target_label(label: &str, fg: Hsla, dim: Hsla) -> AnyElement {
    let dimmed = label.find(" vs ").map(|at| at + 1..at + 3);
    let text = StyledText::new(SharedString::from(label.to_string())).with_highlights(dimmed.map(
        |range| {
            (
                range,
                HighlightStyle {
                    color: Some(dim),
                    ..Default::default()
                },
            )
        },
    ));
    div()
        .min_w_0()
        .truncate()
        .text_color(fg)
        .child(text)
        .into_any_element()
}

/// dv's mark in the accent and the wordmark.
fn mark(left_inset: Pixels, theme: &DvTheme) -> AnyElement {
    let c = &theme.colors;
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .pl(left_inset)
        .pr(theme.spacing.lg)
        .child(icon(DvIcon::Mark, px(18.), c.accent))
        .child(
            div()
                .text_spec(&theme.text.wordmark)
                .text_color(c.fg)
                .child("dv"),
        )
        .automation_target("titlebar/mark")
}

/// The command field (`Ctrl K`, PLAN §11 `palette`): a click opens the palette, or closes it.
fn command_field(theme: &DvTheme, cx: &mut Context<Shell>) -> AnyElement {
    let c = &theme.colors;
    div()
        .id("titlebar-command")
        .role(Role::Button)
        .aria_label(format!(
            "Go to file, review or command ({})",
            keymap::label("secondary-k")
        ))
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .w(theme.sizes.command_field_width)
        .h(theme.sizes.control_height)
        .pl(px(10.))
        .pr(px(5.))
        .rounded(theme.radii.control)
        .border_1()
        .border_color(c.ctrl_line)
        .bg(c.input_bg)
        .text_spec(&theme.text.body)
        .text_color(c.fg3)
        .occlude()
        .cursor_pointer()
        .hover(|style| style.border_color(c.line2))
        .on_click(cx.listener(|this, _, window, cx| {
            if this.palette.is_some() {
                this.close_menu(window, cx);
            } else {
                this.open_palette(window, cx);
            }
        }))
        .child(icon(DvIcon::Search, px(14.), c.fg3))
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .whitespace_nowrap()
                .child("Go to file, review or command"),
        )
        .child(kbd(keymap::label("secondary-k"), theme))
        .automation_target("titlebar/command")
}

/// Minimize, maximize/restore and close: 46 wide and the title bar's height, those
/// [`caption_buttons`] names.
#[derive(IntoElement)]
struct WindowControls {
    glyph: Hsla,
    hover: Hsla,
    close_hover: Hsla,
    close_glyph: Hsla,
    width: Pixels,
    height: Pixels,
}

impl WindowControls {
    fn new(theme: &DvTheme) -> Self {
        let c = &theme.colors;
        Self {
            glyph: c.fg2,
            hover: c.control_hover,
            close_hover: c.caption_close_hover,
            close_glyph: c.caption_close_fg,
            width: theme.sizes.caption_button_width,
            height: theme.sizes.title_bar_height,
        }
    }

    fn button(&self, button: CaptionButton) -> AnyElement {
        let (hover_bg, hover_glyph) = match button {
            CaptionButton::Close => (self.close_hover, self.close_glyph),
            _ => (self.hover, self.glyph),
        };
        let group = button.id();
        div()
            .id(button.id())
            .role(Role::Button)
            .aria_label(button.label())
            .group(group)
            .flex()
            .items_center()
            .justify_center()
            .w(self.width)
            .h(self.height)
            // Keep the drag region from claiming the hit test.
            .occlude()
            .window_control_area(button.control_area())
            // Windows handles the click through the control area; on Linux it is dv's.
            .when(cfg!(target_os = "linux"), |this| {
                this.on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    super::frame::caption_click(button.caption(), window);
                })
            })
            .hover(move |style| style.bg(hover_bg))
            .active(move |style| style.bg(hover_bg.opacity(0.8)))
            .child(
                gpui::svg()
                    .path(button.icon().path())
                    .size(px(10.))
                    .text_color(self.glyph)
                    .group_hover(group, move |style| style.text_color(hover_glyph)),
            )
            .automation_target(button.target())
    }
}

impl RenderOnce for WindowControls {
    fn render(self, window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        div()
            .id("window-controls")
            .flex_none()
            .flex()
            .flex_row()
            .children(
                caption_buttons(window)
                    .into_iter()
                    .map(|button| self.button(button)),
            )
    }
}

/// The caption buttons `window` shows ([`caption_buttons_for`] on this OS).
fn caption_buttons(window: &Window) -> Vec<CaptionButton> {
    caption_buttons_for(
        std::env::consts::OS,
        CaptionState {
            client_decorated: super::frame::client_decorated(window),
            controls: window.window_controls(),
            fullscreen: window.is_fullscreen(),
            maximized: window.is_maximized(),
        },
    )
}

/// What a window's caption buttons depend on.
#[derive(Clone, Copy, Debug)]
struct CaptionState {
    /// dv draws the window's decorations (`super::frame`; Linux).
    client_decorated: bool,
    /// The controls the window manager supports (Linux).
    controls: gpui::WindowControls,
    fullscreen: bool,
    maximized: bool,
}

/// The caption buttons the title bar shows on `os` (`std::env::consts::OS`), left to right: on
/// Windows all three; on Linux, while dv draws the window's decorations, those the window
/// manager supports, Close always; none in fullscreen, and none on macOS, whose traffic lights
/// are the system's.
fn caption_buttons_for(os: &str, state: CaptionState) -> Vec<CaptionButton> {
    let maximize = if state.maximized {
        CaptionButton::Restore
    } else {
        CaptionButton::Maximize
    };
    if state.fullscreen {
        Vec::new()
    } else if os == "windows" {
        vec![CaptionButton::Minimize, maximize, CaptionButton::Close]
    } else if os == "linux" && state.client_decorated {
        let supported = state.controls;
        let mut buttons = Vec::new();
        if supported.minimize {
            buttons.push(CaptionButton::Minimize);
        }
        if supported.maximize {
            buttons.push(maximize);
        }
        buttons.push(CaptionButton::Close);
        buttons
    } else {
        Vec::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptionButton {
    Minimize,
    Maximize,
    Restore,
    Close,
}

impl CaptionButton {
    fn id(self) -> &'static str {
        match self {
            Self::Minimize => "caption-minimize",
            Self::Maximize => "caption-maximize",
            Self::Restore => "caption-restore",
            Self::Close => "caption-close",
        }
    }

    fn icon(self) -> DvIcon {
        match self {
            Self::Minimize => DvIcon::WindowMinimize,
            Self::Maximize => DvIcon::WindowMaximize,
            Self::Restore => DvIcon::WindowRestore,
            Self::Close => DvIcon::WindowClose,
        }
    }

    /// The automation target. Maximize and Restore are one button in two states.
    fn target(self) -> &'static str {
        match self {
            Self::Minimize => "titlebar/min",
            Self::Maximize | Self::Restore => "titlebar/max",
            Self::Close => "titlebar/close",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Minimize => "Minimize",
            Self::Maximize => "Maximize",
            Self::Restore => "Restore",
            Self::Close => "Close",
        }
    }

    /// What a click does where the app handles it (Linux, `super::frame`).
    fn caption(self) -> super::frame::Caption {
        match self {
            Self::Minimize => super::frame::Caption::Minimize,
            Self::Maximize | Self::Restore => super::frame::Caption::Maximize,
            Self::Close => super::frame::Caption::Close,
        }
    }

    fn control_area(self) -> WindowControlArea {
        match self {
            Self::Minimize => WindowControlArea::Min,
            Self::Maximize | Self::Restore => WindowControlArea::Max,
            Self::Close => WindowControlArea::Close,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(client_decorated: bool, minimize: bool, maximize: bool) -> CaptionState {
        CaptionState {
            client_decorated,
            controls: gpui::WindowControls {
                fullscreen: true,
                maximize,
                minimize,
                window_menu: true,
            },
            fullscreen: false,
            maximized: false,
        }
    }

    #[test]
    fn a_pull_requests_label_is_capped_and_a_short_one_left_alone() {
        assert_eq!(capped("#7 Add the answer"), "#7 Add the answer");
        let long = "#12 Cache the parsed config between requests so a reload neither reads \
                    nor validates every file again";
        let label = capped(long);
        assert_eq!(label, "#12 Cache the parsed config between requests so…");
        assert_eq!(label.chars().count(), PR_LABEL_CHARS);
        // Characters, not bytes.
        let wide = "#3 ".to_string() + &"é".repeat(60);
        assert_eq!(capped(&wide).chars().count(), PR_LABEL_CHARS + 1);
    }

    #[test]
    fn caption_buttons_follow_the_platform_and_the_window_manager() {
        use CaptionButton::{Close, Maximize, Minimize, Restore};

        let every = state(false, true, true);
        assert_eq!(
            caption_buttons_for("windows", every),
            [Minimize, Maximize, Close]
        );
        let maximized = CaptionState {
            maximized: true,
            ..every
        };
        assert_eq!(
            caption_buttons_for("windows", maximized),
            [Minimize, Restore, Close]
        );
        let fullscreen = CaptionState {
            fullscreen: true,
            ..every
        };
        assert_eq!(caption_buttons_for("windows", fullscreen), []);
        // macOS: the traffic lights are the system's.
        assert_eq!(caption_buttons_for("macos", state(true, true, true)), []);

        // Linux: only while dv draws the decorations, and only what the window manager
        // supports, Close always.
        assert_eq!(caption_buttons_for("linux", every), [], "server-side");
        assert_eq!(
            caption_buttons_for("linux", state(true, true, true)),
            [Minimize, Maximize, Close]
        );
        assert_eq!(
            caption_buttons_for(
                "linux",
                CaptionState {
                    maximized: true,
                    ..state(true, true, true)
                }
            ),
            [Minimize, Restore, Close]
        );
        assert_eq!(
            caption_buttons_for("linux", state(true, false, true)),
            [Maximize, Close]
        );
        assert_eq!(
            caption_buttons_for("linux", state(true, false, false)),
            [Close],
            "a tiling compositor"
        );
        assert_eq!(
            caption_buttons_for(
                "linux",
                CaptionState {
                    fullscreen: true,
                    ..state(true, true, true)
                }
            ),
            []
        );
    }

    #[test]
    fn prs_keeps_its_label_while_the_left_group_has_room() {
        let (field, right) = (px(300.), px(34. + 120. + 138.));
        // 1440 wide: room for everything.
        assert!(!prs_is_compact(px(1440.), field, px(600.), right));
        // 1100 wide: a short label fits beside the whole button, a long one doesn't.
        assert!(!prs_is_compact(px(1100.), field, px(455.), right));
        assert!(prs_is_compact(px(1100.), field, px(600.), right));
        // Exactly enough is enough.
        assert!(!prs_is_compact(px(1100.), field, px(484.), right));
        assert!(prs_is_compact(px(1100.), field, px(485.), right));
    }

    #[test]
    fn the_command_field_moves_right_only_as_far_as_the_left_group_needs() {
        let (width, field, caption) = (px(1440.), px(300.), px(138.));
        // Room enough: centred.
        assert_eq!(field_left(width, field, px(400.), caption), px(570.));
        // A wider group: the field moves right just enough.
        assert_eq!(field_left(width, field, px(600.), caption), px(612.));
        // Never into the caption buttons: the label truncates instead.
        assert_eq!(field_left(width, field, px(1100.), caption), px(990.));
        // A narrow window whose half is too little already: centred.
        assert_eq!(field_left(px(500.), field, px(400.), caption), px(100.));
    }
}
