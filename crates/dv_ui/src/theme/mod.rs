//! The Glass theme (DESIGN §1): Aura's colours on the window material, or one of the
//! opaque-only themes (DESIGN §1.3).
//!
//! [`DvTheme`] is a gpui global holding every DESIGN token resolved for one [`Palette`]
//! (Aura/Claude/Dracula), one [`Appearance`] (dark/light) and one [`Material`]
//! (Mica/Acrylic/Vibrancy/Opaque; both `gpui_appearance`'s). Views read it through
//! [`ActiveTheme::dv_theme`] and never hard-code colours. [`apply`] installs a first theme at
//! startup and [`install`] switches it (`crate::appearance` follows the settings and the OS);
//! both map the tokens onto gpui-component's [`Theme`] so stock components match, including
//! `Root`'s background, which is the window background of DESIGN §3: transparent over a material
//! so the OS material shows, `chrome` solid over Opaque.

pub mod tokens;

#[cfg(test)]
mod tests;

use gpui::{
    App, BoxShadow, FontWeight, Global, Hsla, Pixels, Rgba, SharedString, Styled, point, px,
};
use gpui_component::highlighter::HighlightTheme;
use gpui_component::{Theme, ThemeMode, ThemeTokens};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::Arc;

use crate::assets::fonts;
use tokens::{Surface, TokenSet};

/// Dark or light (DESIGN §1.1, §1.2), and the window material (PLAN D8, DESIGN §3): the
/// toolbox's, whose keys (`"dark"`, `"mica"`, …) settings and automation state spell.
pub use gpui_appearance::{Appearance, Material};

/// A theme's family (DESIGN §1.3): Aura (dark and light, on every material), Claude (dark and
/// light, opaque) or Dracula (dark, opaque).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Palette {
    #[default]
    Aura,
    Claude,
    Dracula,
}

impl Palette {
    pub const ALL: [Palette; 3] = [Self::Aura, Self::Claude, Self::Dracula];

    /// `"aura"`, `"claude"` or `"dracula"`, as settings and automation state spell it.
    pub fn key(self) -> &'static str {
        match self {
            Self::Aura => "aura",
            Self::Claude => "claude",
            Self::Dracula => "dracula",
        }
    }

    /// The name the UI shows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Aura => "Aura",
            Self::Claude => "Claude",
            Self::Dracula => "Dracula",
        }
    }

    /// The palette a [`key`](Self::key) names.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|palette| palette.key() == key)
    }

    /// Whether it has a light palette; one that hasn't shows dark whatever the mode.
    pub fn has_light(self) -> bool {
        self != Self::Dracula
    }

    /// Whether it is drawn over the window material; the others are opaque only.
    pub fn is_glass(self) -> bool {
        self == Self::Aura
    }

    /// The appearance it shows when `asked` is wanted.
    pub fn appearance(self, asked: Appearance) -> Appearance {
        if self.has_light() {
            asked
        } else {
            Appearance::Dark
        }
    }

    /// The material it shows when `asked` is what the platform can show.
    pub fn material(self, asked: Material) -> Material {
        if self.is_glass() {
            asked
        } else {
            Material::Opaque
        }
    }

    fn tokens(self, appearance: Appearance) -> &'static TokenSet {
        match (self, self.appearance(appearance)) {
            (Self::Aura, Appearance::Dark) => &tokens::DARK,
            (Self::Aura, Appearance::Light) => &tokens::LIGHT,
            (Self::Claude, Appearance::Dark) => &tokens::CLAUDE_DARK,
            (Self::Claude, Appearance::Light) => &tokens::CLAUDE_LIGHT,
            (Self::Dracula, _) => &tokens::DRACULA,
        }
    }
}

/// Syntax colours by token kind (PLAN §6 step 4 resolves kinds to these at paint time).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyntaxColors {
    pub keyword: Hsla,
    pub function: Hsla,
    pub string: Hsla,
    pub number: Hsla,
    pub ty: Hsla,
    pub constant: Hsla,
    pub property: Hsla,
    pub punctuation: Hsla,
    pub comment: Hsla,
    pub plain: Hsla,
}

/// Every colour token for one appearance and material (DESIGN §1.1, §1.2), plus the few
/// component colours DESIGN leaves to the platform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DvColors {
    // Surfaces, resolved for the material.
    pub chrome: Hsla,
    pub panel: Hsla,
    pub code: Hsla,
    pub code_head: Hsla,
    pub recess: Hsla,
    pub layer: Hsla,
    pub layer_strong: Hsla,
    pub thread: Hsla,
    pub overlay: Hsla,
    /// A popover over content with no scrim under it (the hover card, menus, the find bar, a
    /// server's details): `overlay`'s Opaque value whatever the material, so the code under it
    /// never shows through (DESIGN §3).
    pub popover: Hsla,
    pub ctrl: Hsla,
    pub input_bg: Hsla,
    // Solid tokens.
    pub fg: Hsla,
    pub fg2: Hsla,
    pub fg3: Hsla,
    pub accent: Hsla,
    pub accent_fg: Hsla,
    pub accent_text: Hsla,
    pub accent_soft: Hsla,
    pub accent_edge: Hsla,
    pub accent_ring: Hsla,
    pub green: Hsla,
    pub red: Hsla,
    pub yellow: Hsla,
    pub blue: Hsla,
    pub green_soft: Hsla,
    pub red_soft: Hsla,
    pub yellow_soft: Hsla,
    pub blue_soft: Hsla,
    pub warn_line: Hsla,
    pub line: Hsla,
    pub line2: Hsla,
    pub ctrl_line: Hsla,
    pub overlay_line: Hsla,
    pub scrim: Hsla,
    pub seg_on: Hsla,
    pub seg_track: Hsla,
    pub kbd_bg: Hsla,
    pub sel: Hsla,
    pub text_sel: Hsla,
    pub find_match: Hsla,
    pub find_current: Hsla,
    pub find_line: Hsla,
    pub track: Hsla,
    pub icode_bg: Hsla,
    // Diff.
    pub hunk: Hsla,
    pub add_bg: Hsla,
    pub add_gut: Hsla,
    pub add_word: Hsla,
    pub del_bg: Hsla,
    pub del_gut: Hsla,
    pub del_word: Hsla,
    pub syntax: SyntaxColors,
    // Component colours.
    /// Behind everything (DESIGN §3): transparent over Mica and Acrylic, so the OS material
    /// shows; `chrome` solid over Opaque.
    pub window_background: Hsla,
    /// A ghost control or caption button under the pointer: the `line` tint.
    pub control_hover: Hsla,
    /// The Close caption button under the pointer, and its glyph: Windows' own red and white.
    pub caption_close_hover: Hsla,
    pub caption_close_fg: Hsla,
}

impl DvColors {
    pub fn new(palette: Palette, appearance: Appearance, material: Material) -> Self {
        let TokenSet {
            surfaces: s,
            solid: t,
            syntax: x,
        } = palette.tokens(appearance);
        let m = |surface: &Surface| -> Hsla { surface.on(material).into() };
        let c = |value: Rgba| -> Hsla { value.into() };
        let chrome = m(&s.chrome);
        Self {
            chrome,
            panel: m(&s.panel),
            code: m(&s.code),
            code_head: m(&s.code_head),
            recess: m(&s.recess),
            layer: m(&s.layer),
            layer_strong: m(&s.layer_strong),
            thread: m(&s.thread),
            overlay: m(&s.overlay),
            popover: s.overlay.on(Material::Opaque).into(),
            ctrl: m(&s.ctrl),
            input_bg: m(&s.input_bg),
            fg: c(t.fg),
            fg2: c(t.fg2),
            fg3: c(t.fg3),
            accent: c(t.accent),
            accent_fg: c(t.accent_fg),
            accent_text: c(t.accent_text),
            accent_soft: c(t.accent_soft),
            accent_edge: c(t.accent_edge),
            accent_ring: c(t.accent_ring),
            green: c(t.green),
            red: c(t.red),
            yellow: c(t.yellow),
            blue: c(t.blue),
            green_soft: c(t.green_soft),
            red_soft: c(t.red_soft),
            yellow_soft: c(t.yellow_soft),
            blue_soft: c(t.blue_soft),
            warn_line: c(t.warn_line),
            line: c(t.line),
            line2: c(t.line2),
            ctrl_line: c(t.ctrl_line),
            overlay_line: c(t.overlay_line),
            scrim: c(t.scrim),
            seg_on: c(t.seg_on),
            seg_track: c(t.seg_track),
            kbd_bg: c(t.kbd_bg),
            sel: c(t.sel),
            text_sel: c(t.text_sel),
            find_match: c(t.find_match),
            find_current: c(t.find_current),
            find_line: c(t.find_line),
            track: c(t.track),
            icode_bg: c(t.icode_bg),
            hunk: c(t.hunk),
            add_bg: c(t.add_bg),
            add_gut: c(t.add_gut),
            add_word: c(t.add_word),
            del_bg: c(t.del_bg),
            del_gut: c(t.del_gut),
            del_word: c(t.del_word),
            syntax: SyntaxColors {
                keyword: c(x.keyword),
                function: c(x.function),
                string: c(x.string),
                number: c(x.number),
                ty: c(x.ty),
                constant: c(x.constant),
                property: c(x.property),
                punctuation: c(x.punctuation),
                comment: c(x.comment),
                plain: c(x.plain),
            },
            window_background: if material.is_translucent() {
                gpui::transparent_black()
            } else {
                chrome
            },
            control_hover: c(t.line),
            caption_close_hover: c(tokens::hex(0xc42b1c)),
            caption_close_fg: gpui::white(),
        }
    }
}

/// The bundled families (DESIGN §1.4).
#[derive(Clone, Debug, PartialEq)]
pub struct Fonts {
    /// Geist: all UI text.
    pub ui: SharedString,
    /// JetBrains Mono: code, keycaps, line numbers.
    pub mono: SharedString,
}

impl Default for Fonts {
    fn default() -> Self {
        Self {
            ui: fonts::GEIST.into(),
            mono: fonts::JETBRAINS_MONO.into(),
        }
    }
}

/// One step of the type scale.
#[derive(Clone, Debug, PartialEq)]
pub struct TextSpec {
    pub family: SharedString,
    pub size: Pixels,
    pub line_height: Pixels,
    pub weight: FontWeight,
}

impl TextSpec {
    fn new(family: &SharedString, size: f32, line_height: f32, weight: FontWeight) -> Self {
        Self {
            family: family.clone(),
            size: px(size),
            line_height: px(line_height),
            weight,
        }
    }
}

/// The type scale (DESIGN §1.4): 13/18 body, 12 secondary, 11 keycaps; 600 for titles, 500 for
/// controls, 400 for text. Geist ships 400/500/600 here, so the canvas's 700 wordmark is drawn
/// at 600.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeScale {
    /// 13/18 body text.
    pub body: TextSpec,
    /// 13/18 controls (buttons, the switcher).
    pub control: TextSpec,
    /// 13/18 titles ("Changes").
    pub title: TextSpec,
    /// 12/16 secondary text (the status bar, hints).
    pub secondary: TextSpec,
    /// 14 the dv wordmark.
    pub wordmark: TextSpec,
    /// 15/20 sheet titles and the main empty state.
    pub heading: TextSpec,
    /// 20/26 the onboarding title.
    pub display: TextSpec,
    /// 11 mono keycaps and badges.
    pub kbd: TextSpec,
    /// 11.5 mono inline code.
    pub inline_code: TextSpec,
    /// 12/20 mono code rows.
    pub code: TextSpec,
}

impl TypeScale {
    pub fn new(fonts: &Fonts) -> Self {
        let (regular, medium, semibold) =
            (FontWeight::NORMAL, FontWeight::MEDIUM, FontWeight::SEMIBOLD);
        Self {
            body: TextSpec::new(&fonts.ui, 13., 18., regular),
            control: TextSpec::new(&fonts.ui, 13., 18., medium),
            title: TextSpec::new(&fonts.ui, 13., 18., semibold),
            secondary: TextSpec::new(&fonts.ui, 12., 16., regular),
            wordmark: TextSpec::new(&fonts.ui, 14., 18., semibold),
            heading: TextSpec::new(&fonts.ui, 15., 20., semibold),
            display: TextSpec::new(&fonts.ui, 20., 26., semibold),
            kbd: TextSpec::new(&fonts.mono, 11., 11., regular),
            inline_code: TextSpec::new(&fonts.mono, 11.5, 16., regular),
            code: TextSpec::new(&fonts.mono, 12., 20., regular),
        }
    }
}

/// Applies a [`TextSpec`] to any styled element.
pub trait StyledText: Styled + Sized {
    fn text_spec(self, spec: &TextSpec) -> Self {
        self.font_family(spec.family.clone())
            .text_size(spec.size)
            .line_height(spec.line_height)
            .font_weight(spec.weight)
    }
}

impl<E: Styled> StyledText for E {}

/// Fixed sizes (DESIGN §1.5, §2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sizes {
    pub title_bar_height: Pixels,
    pub status_bar_height: Pixels,
    pub panel_width: Pixels,
    pub panel_min_width: Pixels,
    pub panel_max_width: Pixels,
    pub control_height: Pixels,
    pub control_small_height: Pixels,
    pub control_xsmall_height: Pixels,
    pub kbd_height: Pixels,
    pub file_row_height: Pixels,
    pub code_row_height: Pixels,
    /// The title bar's centred command field (canvas `GlassReview`).
    pub command_field_width: Pixels,
    /// A Windows caption button: 46 wide, the title bar's height.
    pub caption_button_width: Pixels,
    /// Room for macOS's traffic lights at the title bar's left edge: the three lights from 12 to
    /// 72, then the spacing scale's 12 (DESIGN §2).
    pub traffic_lights_inset: Pixels,
    pub window_width: Pixels,
    pub window_height: Pixels,
    pub window_min_width: Pixels,
    pub window_min_height: Pixels,
}

impl Default for Sizes {
    fn default() -> Self {
        Self {
            title_bar_height: px(40.),
            status_bar_height: px(28.),
            panel_width: px(280.),
            panel_min_width: px(220.),
            panel_max_width: px(420.),
            control_height: px(28.),
            control_small_height: px(24.),
            control_xsmall_height: px(22.),
            kbd_height: px(18.),
            file_row_height: px(28.),
            code_row_height: px(20.),
            command_field_width: px(300.),
            caption_button_width: px(46.),
            traffic_lights_inset: px(84.),
            window_width: px(1440.),
            window_height: px(900.),
            window_min_width: px(1100.),
            window_min_height: px(700.),
        }
    }
}

/// Corner radii (DESIGN §1.5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Radii {
    /// Keycaps, inline code.
    pub small: Pixels,
    /// Chips.
    pub chip: Pixels,
    /// Buttons, inputs.
    pub control: Pixels,
    /// Segmented controls.
    pub segmented: Pixels,
    /// Thread cards.
    pub card: Pixels,
    /// Sheets.
    pub sheet: Pixels,
}

impl Default for Radii {
    fn default() -> Self {
        Self {
            small: px(4.),
            chip: px(5.),
            control: px(6.),
            segmented: px(7.),
            card: px(10.),
            sheet: px(12.),
        }
    }
}

/// The spacing scale 4 / 6 / 8 / 12 / 16 / 24 (DESIGN §1.5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spacing {
    pub xs: Pixels,
    pub sm: Pixels,
    pub md: Pixels,
    pub lg: Pixels,
    pub xl: Pixels,
    pub xxl: Pixels,
}

impl Default for Spacing {
    fn default() -> Self {
        Self {
            xs: px(4.),
            sm: px(6.),
            md: px(8.),
            lg: px(12.),
            xl: px(16.),
            xxl: px(24.),
        }
    }
}

/// Elevation (DESIGN §1.5, the canvas's `cardShadow`, `overlayShadow`, `ctrlShadow`).
#[derive(Clone, Debug, PartialEq)]
pub struct Shadows {
    pub card: Vec<BoxShadow>,
    /// Overlays: two drop shadows and, on dark, a 1px inner top highlight.
    pub overlay: Vec<BoxShadow>,
    /// Buttons: an inner highlight on dark, a hairline drop on light.
    pub control: Vec<BoxShadow>,
}

impl Shadows {
    pub fn new(appearance: Appearance) -> Self {
        let black = |alpha| tokens::rgba(0, 0, 0, alpha);
        let ink = |alpha| tokens::rgba(30, 22, 70, alpha);
        match appearance {
            Appearance::Dark => Self {
                card: vec![shadow(black(0.28), 6., 20., false)],
                overlay: vec![
                    shadow(black(0.55), 28., 80., false),
                    shadow(black(0.35), 2., 10., false),
                    shadow(tokens::rgba(255, 255, 255, 0.06), 1., 0., true),
                ],
                control: vec![shadow(tokens::rgba(255, 255, 255, 0.04), 1., 0., true)],
            },
            Appearance::Light => Self {
                card: vec![shadow(ink(0.08), 4., 14., false)],
                // Softer than dark's: a popover over light code with no scrim under it (the
                // find bar, the hover card) would otherwise sit in a grey haze (DESIGN §1.5).
                overlay: vec![
                    shadow(ink(0.14), 20., 56., false),
                    shadow(ink(0.10), 2., 10., false),
                ],
                control: vec![shadow(tokens::rgba(28, 24, 56, 0.05), 1., 1., false)],
            },
        }
    }
}

fn shadow(color: Rgba, y: f32, blur: f32, inset: bool) -> BoxShadow {
    BoxShadow {
        color: color.into(),
        offset: point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(0.),
        inset,
    }
}

/// Every DESIGN token for one palette, appearance and material.
#[derive(Clone, Debug)]
pub struct DvTheme {
    pub palette: Palette,
    pub appearance: Appearance,
    pub material: Material,
    pub colors: DvColors,
    pub fonts: Fonts,
    pub text: TypeScale,
    pub sizes: Sizes,
    pub radii: Radii,
    pub spacing: Spacing,
    pub shadows: Shadows,
}

impl Global for DvTheme {}

impl DvTheme {
    /// The theme for `palette`, showing `appearance` over `material` as far as the palette
    /// has them ([`Palette::appearance`], [`Palette::material`]).
    pub fn new(palette: Palette, appearance: Appearance, material: Material) -> Self {
        let appearance = palette.appearance(appearance);
        let material = palette.material(material);
        let fonts = Fonts::default();
        Self {
            palette,
            appearance,
            material,
            colors: DvColors::new(palette, appearance, material),
            text: TypeScale::new(&fonts),
            fonts,
            sizes: Sizes::default(),
            radii: Radii::default(),
            spacing: Spacing::default(),
            shadows: Shadows::new(appearance),
        }
    }

    /// `"<palette>-<appearance>"`: what a cache of anything drawn in its colours is keyed by.
    pub fn key(&self) -> String {
        format!("{}-{}", self.palette.key(), self.appearance.key())
    }
}

/// Access to the installed [`DvTheme`].
pub trait ActiveTheme {
    /// The theme in use. Panics if [`apply`] hasn't run, which is a startup bug.
    fn dv_theme(&self) -> &DvTheme;
}

impl ActiveTheme for App {
    fn dv_theme(&self) -> &DvTheme {
        self.global::<DvTheme>()
    }
}

/// Installs the dark, opaque theme: the one in use until `crate::appearance` resolves the
/// settings. Call after `gpui_component::init`.
pub fn apply(cx: &mut App) {
    set(
        DvTheme::new(Palette::Aura, Appearance::Dark, Material::Opaque),
        cx,
    );
}

/// Switches to the theme for `palette`, showing `appearance` over `material` as far as it has
/// them, and redraws every window. Nothing happens when it is already in use.
pub fn install(palette: Palette, appearance: Appearance, material: Material, cx: &mut App) {
    let theme = DvTheme::new(palette, appearance, material);
    if cx.try_global::<DvTheme>().is_some_and(|current| {
        (current.palette, current.appearance, current.material)
            == (theme.palette, theme.appearance, theme.material)
    }) {
        return;
    }
    set(theme, cx);
    cx.refresh_windows();
}

fn set(dv: DvTheme, cx: &mut App) {
    // The stock palette of the mode first, so whatever the mapping leaves alone matches it.
    Theme::change(component_mode(dv.appearance), None, cx);
    map_component_theme(&dv, Theme::global_mut(cx));
    cx.set_global(dv);
    Theme::sync_base(cx);
}

fn component_mode(appearance: Appearance) -> ThemeMode {
    match appearance {
        Appearance::Dark => ThemeMode::Dark,
        Appearance::Light => ThemeMode::Light,
    }
}

/// Writes the palette, fonts and radii of `dv` into a gpui-component [`Theme`].
pub fn map_component_theme(dv: &DvTheme, theme: &mut Theme) {
    let c = &dv.colors;
    theme.highlight_theme = Arc::new(highlight_theme(dv));
    theme.mode = component_mode(dv.appearance);
    theme.font_family = dv.fonts.ui.clone();
    // gpui-component sizes its controls in rems from this; dv's views set their own text sizes.
    theme.font_size = px(16.);
    theme.mono_font_family = dv.fonts.mono.clone();
    theme.mono_font_size = dv.text.code.size;
    theme.radius = dv.radii.control;
    theme.radius_lg = dv.radii.sheet;
    theme.shadow = true;

    let colors = &mut theme.colors;
    // `Root` paints this behind every window (DESIGN §3).
    colors.background = c.window_background;
    colors.foreground = c.fg;
    colors.border = c.ctrl_line;
    colors.input = c.ctrl_line;
    colors.ring = c.accent_ring;
    colors.caret = c.accent;
    colors.selection = c.sel;
    colors.overlay = c.scrim;
    colors.window_border = c.line;

    colors.accent = c.control_hover;
    colors.accent_foreground = c.fg;
    colors.muted = c.layer;
    colors.muted_foreground = c.fg3;
    colors.popover = c.popover;
    colors.popover_foreground = c.fg;

    colors.primary = c.accent;
    colors.primary_hover = c.accent_edge;
    colors.primary_active = c.accent.opacity(0.85);
    colors.primary_foreground = c.accent_fg;
    colors.secondary = c.ctrl;
    colors.secondary_hover = c.layer_strong;
    colors.secondary_active = c.layer_strong;
    colors.secondary_foreground = c.fg;

    colors.button = c.ctrl;
    colors.button_hover = c.layer_strong;
    colors.button_active = c.layer_strong;
    colors.button_foreground = c.fg;
    colors.button_primary = c.accent;
    colors.button_primary_hover = c.accent_edge;
    colors.button_primary_active = c.accent.opacity(0.85);
    colors.button_primary_foreground = c.accent_fg;
    colors.button_secondary = c.ctrl;
    colors.button_secondary_hover = c.layer_strong;
    colors.button_secondary_active = c.layer_strong;
    colors.button_secondary_foreground = c.fg;

    colors.danger = c.red;
    colors.danger_hover = c.red.opacity(0.9);
    colors.danger_active = c.red.opacity(0.8);
    colors.danger_foreground = c.accent_fg;
    colors.warning = c.yellow;
    colors.warning_hover = c.yellow.opacity(0.9);
    colors.warning_active = c.yellow.opacity(0.8);
    colors.warning_foreground = c.accent_fg;
    colors.success = c.green;
    colors.success_hover = c.green.opacity(0.9);
    colors.success_active = c.green.opacity(0.8);
    colors.success_foreground = c.accent_fg;
    colors.info = c.blue;
    colors.info_hover = c.blue.opacity(0.9);
    colors.info_active = c.blue.opacity(0.8);
    colors.info_foreground = c.accent_fg;

    // Filled semantic buttons are the hue's soft fill with the hue as text, so they read on
    // every surface in both modes.
    colors.button_danger = c.red_soft;
    colors.button_danger_hover = deepen(c.red_soft, 1.4);
    colors.button_danger_active = deepen(c.red_soft, 1.8);
    colors.button_danger_foreground = c.red;
    colors.button_warning = c.yellow_soft;
    colors.button_warning_hover = deepen(c.yellow_soft, 1.4);
    colors.button_warning_active = deepen(c.yellow_soft, 1.8);
    colors.button_warning_foreground = c.yellow;
    colors.button_success = c.green_soft;
    colors.button_success_hover = deepen(c.green_soft, 1.4);
    colors.button_success_active = deepen(c.green_soft, 1.8);
    colors.button_success_foreground = c.green;
    colors.button_info = c.blue_soft;
    colors.button_info_hover = deepen(c.blue_soft, 1.4);
    colors.button_info_active = deepen(c.blue_soft, 1.8);
    colors.button_info_foreground = c.blue;

    colors.link = c.accent_text;
    colors.link_hover = c.accent_text;
    colors.link_active = c.accent_text;

    colors.list = gpui::transparent_black();
    colors.list_even = gpui::transparent_black();
    colors.list_head = c.panel;
    colors.list_hover = c.control_hover;
    colors.list_active = c.sel;
    colors.list_active_border = c.accent_ring;

    colors.sidebar = c.panel;
    colors.sidebar_foreground = c.fg;
    colors.sidebar_border = c.line;
    colors.sidebar_accent = c.control_hover;
    colors.sidebar_accent_foreground = c.fg;
    colors.sidebar_primary = c.accent;
    colors.sidebar_primary_foreground = c.accent_fg;

    colors.tab = gpui::transparent_black();
    colors.tab_active = c.seg_on;
    colors.tab_active_foreground = c.fg;
    colors.tab_bar = c.chrome;
    colors.tab_bar_segmented = c.ctrl;
    colors.tab_foreground = c.fg2;

    colors.title_bar = c.chrome;
    colors.title_bar_border = c.line;
    colors.progress_bar = c.accent;
    colors.slider_bar = c.accent;
    colors.scrollbar = gpui::transparent_black();
    colors.scrollbar_thumb = c.line2;
    colors.scrollbar_thumb_hover = c.fg3;
    colors.skeleton = c.layer;
    colors.drag_border = c.accent;
    colors.drop_target = c.accent_soft;
    colors.accordion = c.layer;
    colors.group_box = c.layer;
    colors.group_box_foreground = c.fg;
    colors.table = gpui::transparent_black();
    colors.table_even = gpui::transparent_black();
    colors.table_head = c.code_head;
    colors.table_head_foreground = c.fg3;
    colors.table_hover = c.control_hover;
    colors.table_active = c.sel;
    colors.table_active_border = c.accent_ring;
    colors.table_row_border = c.line;

    theme.tokens = ThemeTokens::from(&theme.colors);
}

/// `color` with its alpha scaled by `factor`, up to opaque: a soft fill hovered or pressed.
fn deepen(color: Hsla, factor: f32) -> Hsla {
    color.alpha((color.a * factor).min(1.))
}

/// WCAG relative luminance of an opaque colour.
pub fn relative_luminance(color: Hsla) -> f32 {
    let rgba = color.to_rgb();
    let channel = |c: f32| {
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgba.r) + 0.7152 * channel(rgba.g) + 0.0722 * channel(rgba.b)
}

/// WCAG contrast ratio between two opaque colours.
pub fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// gpui-component's highlight theme in dv's syntax colours, for the code it highlights itself:
/// a fenced block in Markdown (a hover card's signature, a comment's code). Every capture it
/// knows takes the colour of its token kind as the stream paints it (`crate::syntax`), plain
/// text when it has none (DESIGN §1.1).
fn highlight_theme(dv: &DvTheme) -> HighlightTheme {
    let syntax = &dv.colors.syntax;
    let hex = |color: Hsla| serde_json::to_value(color).unwrap_or(Value::Null);
    let names = match serde_json::to_value(gpui_component::highlighter::SyntaxColors::default()) {
        Ok(Value::Object(names)) => names,
        _ => Map::new(),
    };
    let captures: Map<String, Value> = names
        .into_iter()
        .map(|(name, _)| {
            let color = crate::syntax::kind_for_capture(&name)
                .map_or(syntax.plain, |kind| kind.color(syntax));
            (name, json!({"color": hex(color)}))
        })
        .collect();
    let theme = json!({
        "name": "dv",
        "appearance": dv.appearance.key(),
        "style": {
            "editor.background": hex(dv.colors.code),
            "editor.foreground": hex(syntax.plain),
            "syntax": captures,
        },
    });
    serde_json::from_value(theme).unwrap_or_else(|error| {
        log::warn!("dv's highlight theme didn't build ({error}); using gpui-component's");
        let fallback = if dv.appearance.is_dark() {
            HighlightTheme::default_dark()
        } else {
            HighlightTheme::default_light()
        };
        (*fallback).clone()
    })
}
