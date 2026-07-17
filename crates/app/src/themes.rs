//! Bundled theme registry: four `gpui-component` `ThemeConfig`s embedded at
//! compile time, switched live via the theme picker (`ctrl-shift-t`, see
//! `shell.rs`) and persisted through `settings.rs`. Replaces the old
//! `main.rs::apply_aura_theme` — same load-and-apply mechanics, just table-
//! driven over more than one theme.
//!
//! Also owns [`DvTheme`] — the dv-side extension to `gpui_component::Theme`
//! for tokens the R1–R3 restyle needs that `ThemeConfig` has no field
//! for.

use std::rc::Rc;

use gpui::{App, Global, Hsla, SharedString, Window};
use gpui_component::{Colorize, Theme, ThemeConfig, ThemeMode};
use serde::Deserialize;

/// Falls back here on first launch (no settings.json yet) and on any
/// unrecognized/corrupt persisted theme name.
pub const DEFAULT_THEME: &str = "Aura Dark";

struct Entry {
    name: &'static str,
    json: &'static str,
    mode: ThemeMode,
}

/// The bundled registry, in picker display order. `aura-dark.json` keeps its
/// `ThemeConfig`-visible keys identical to the pre-existing reference theme;
/// the other three are new siblings covering the exact same key set. All
/// four now additionally carry a dv-owned `"dv"` override section (see
/// [`DvTheme`]) that the reference theme never had and `ThemeConfig` itself
/// doesn't parse.
const THEMES: [Entry; 4] = [
    Entry {
        name: "Aura Dark",
        json: include_str!("../../../themes/aura-dark.json"),
        mode: ThemeMode::Dark,
    },
    Entry {
        name: "Dracula",
        json: include_str!("../../../themes/dracula.json"),
        mode: ThemeMode::Dark,
    },
    Entry {
        name: "Claude Dark",
        json: include_str!("../../../themes/claude-dark.json"),
        mode: ThemeMode::Dark,
    },
    Entry {
        name: "Claude Light",
        json: include_str!("../../../themes/claude-light.json"),
        mode: ThemeMode::Light,
    },
];

/// Names in picker display order.
pub fn names() -> impl Iterator<Item = &'static str> {
    THEMES.iter().map(|t| t.name)
}

/// Look up a registry entry by name, falling back to [`DEFAULT_THEME`] for
/// an unknown name (a stale/corrupt settings.json must never wedge the app
/// on a theme that no longer exists).
fn find(name: &str) -> &'static Entry {
    THEMES.iter().find(|t| t.name == name).unwrap_or_else(|| {
        THEMES
            .iter()
            .find(|t| t.name == DEFAULT_THEME)
            .expect("DEFAULT_THEME must name a registry entry")
    })
}

/// Parse and apply the bundled theme called `name` (falling back to
/// [`DEFAULT_THEME`] if unrecognized), then re-assert `mono_font` (the
/// settings-panel-editable mono font family, see `settings.rs`'s
/// `DEFAULT_MONO_FONT`) so it sticks regardless of which theme JSON is
/// active — none of the four bundled themes set their own `mono_font.family`.
///
/// `window` is `None` at startup (no window exists yet when the App-level
/// theme is first applied in `main.rs`); `Some` from the live picker, whose
/// `window.refresh()` (via `Theme::change`) is what makes the swap repaint
/// immediately instead of waiting for the next unrelated re-render.
pub fn apply_theme(name: &str, mono_font: &str, window: Option<&mut Window>, cx: &mut App) {
    let entry = find(name);
    let config: ThemeConfig = serde_json::from_str(entry.json).unwrap_or_else(|err| {
        panic!(
            "bundled theme {:?} must parse as a gpui-component ThemeConfig: {err}",
            entry.name
        )
    });
    Theme::change(entry.mode, window, cx);
    Theme::global_mut(cx).apply_config(&Rc::new(config));
    Theme::global_mut(cx).mono_font_family = SharedString::from(mono_font.to_string());
    // Re-resolve on every apply (not just at startup) — follow-OS theme
    // switching (Phase 4) re-invokes `apply_theme` live, and `DvTheme` must
    // never go stale relative to the base theme it derives from.
    cx.set_global(DvTheme::resolve(Theme::global(cx), entry.json));
}

/// dv-owned theme extension: tokens the R1–R3 restyle needs that
/// `gpui_component::ThemeConfig`'s schema has no field for — a deepest-chrome
/// "recess" surface, a second dim-text tier, word-level intraline tints, the
/// absent-split-side/backdrop scrims, and a deliberate purple "link" hue.
/// A `gpui::Global`, populated by [`apply_theme`] right after `apply_config`
/// so it is always in lockstep with the base `gpui_component::Theme`
/// (including on every follow-OS re-application, not just at startup).
///
/// Every field has a derivation rule from the already-applied `Theme`'s
/// colors, so a third-party theme JSON with no `"dv"` section still gets sane
/// (if approximate) values — see [`DvTheme::resolve`]. The four bundled
/// themes instead carry an explicit `"dv"` override section in their JSON
/// (hand-picked per-theme
/// hex for `recess_bg`/`text_secondary`; no one generic formula reproduces
/// all four — their sidebar→recess darkening factors alone range ~0.05–0.35).
/// The alpha-composite tokens (word tints, `void_bg`, `backdrop`) are always
/// *computed*, never stored.
///
/// R1a landed the tokens and their derivation only; R1b is the first
/// consumer (`shell::state_pill`'s `accent_alt` use for the merged-PR /
/// renamed-file pill). The other fields (`recess_bg`, `text_secondary`,
/// the word tints, `void_bg`, `backdrop`) still have no render call site —
/// title bar, diff pane, and sidebar restyles (R1c-R1e) are where those get
/// consumers — hence the blanket `allow(dead_code)` staying put below.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct DvTheme {
    /// Deepest chrome surface: hunk headers, gap rows, split divider, and the
    /// base color `backdrop` composes over.
    pub recess_bg: Hsla,
    /// Second dim-text tier between `foreground` and `muted.foreground`, for
    /// row/card metadata (PR number, author, timestamps).
    pub text_secondary: Hsla,
    /// Word-level intraline tint for added text — `success` @ 0.28, a
    /// stronger pop than the existing ~0.14 whole-row tint.
    pub word_created_bg: Hsla,
    /// Word-level intraline tint for removed text — `danger` @ 0.28.
    pub word_deleted_bg: Hsla,
    /// Selected/hover fill for rows sitting ON `sidebar.background` (review
    /// cards, file-tree rows, footer keycap chips). Normally just
    /// `muted.background` — but Claude Light defines `muted.background` ==
    /// `sidebar.background` (#f0eee6), which made every muted-filled state
    /// on a sidebar surface literally invisible (R1e/R1f visual review, P2).
    /// When the two collide, falls back to `foreground` @ 0.10 — an
    /// alpha-composite that is visible over any surface by construction.
    /// Dark themes keep their exact pre-existing `muted.background` pixels.
    pub surface_active: Hsla,
    /// 1px seam border for modal panels sitting on `sidebar.background`
    /// (pickers, settings, onboarding — R2 item 6's shared modal chrome).
    /// Normally `muted.background` — the "soft lightened seam" the modal
    /// recipe calls for, never `border` by default (Aura Dark defines
    /// `border` as #000000) — but under the same Claude Light collision
    /// `surface_active` documents above, a `muted` border on a `sidebar`
    /// panel paints the panel's own color and the seam vanishes (R2 visual
    /// review, P3). Claude Light's `border` (#e3e1d7) is a perfectly good
    /// seam, so the collision case falls back to it.
    pub modal_border: Hsla,
    /// Absent side of a one-sided split-view row — `recess_bg` @ 0.60.
    pub void_bg: Hsla,
    /// Modal/palette dimming scrim — `recess_bg` @ 0.67 on dark themes,
    /// `foreground` @ 0.30 on light themes (a dark scrim over a light base
    /// reads as a dim, not a glow).
    pub backdrop: Hsla,
    /// The deliberate purple "link" hue — merged-PR badge, renamed-file
    /// badge. Defaults to `base.magenta`, which is present in all four
    /// bundled theme JSONs.
    pub accent_alt: Hsla,
}

impl Global for DvTheme {}

/// The optional `"dv": { ... }` override section, read from the same
/// `themes/*.json` text `apply_theme` already parses as a `ThemeConfig`.
/// `ThemeConfig` (and every struct it's built from) has no
/// `#[serde(deny_unknown_fields)]` anywhere in gpui-component's theme schema,
/// so an unrecognized top-level `"dv"` key is silently ignored by that parse
/// — verified by the `theme_config_tolerates_an_unknown_top_level_dv_key`
/// test below, this module's first order of business per the slice plan.
/// Only genuinely new hues get an override slot here; the computed
/// alpha-composite tokens never do (see [`DvTheme`]'s doc comment).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct DvThemeOverrides {
    recess_bg: Option<String>,
    text_secondary: Option<String>,
    accent_alt: Option<String>,
}

/// Just enough of a theme JSON's shape to pull out its `"dv"` section —
/// deliberately not `ThemeConfig` itself (which doesn't know this key), and
/// deliberately not tracking gpui-component's schema otherwise.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
struct DvThemeDocument {
    dv: DvThemeOverrides,
}

impl DvTheme {
    /// Resolve every token for `theme` (the just-applied `gpui_component`
    /// theme), honoring an optional `"dv"` override section parsed out of
    /// `raw_json` (the same JSON text applied to `theme`). A JSON parse
    /// failure or a missing `"dv"` section both fall back to the derivation
    /// rules — this must never panic on a theme that already parsed fine as
    /// a `ThemeConfig`.
    fn resolve(theme: &Theme, raw_json: &str) -> DvTheme {
        let overrides = serde_json::from_str::<DvThemeDocument>(raw_json)
            .map(|doc| doc.dv)
            .unwrap_or_default();

        let parse = |hex: &Option<String>| hex.as_deref().and_then(|hex| Hsla::parse_hex(hex).ok());

        // Derivation: darken(sidebar.background). No single factor matches
        // all four bundled themes' hand-tuned values (see this struct's doc
        // comment), so this is a best-effort fallback for third-party themes
        // only — the bundled four always hit their `"dv"` override instead.
        let recess_bg = parse(&overrides.recess_bg).unwrap_or_else(|| theme.sidebar.darken(0.15));

        // Derivation: an even mix of `foreground` and `muted.foreground`.
        let text_secondary = parse(&overrides.text_secondary)
            .unwrap_or_else(|| theme.foreground.mix(theme.muted_foreground, 0.5));

        // Derivation: `base.magenta`, present in every bundled theme JSON and
        // already an exact match for the spec table — no bundled theme needs
        // an override here.
        let accent_alt = parse(&overrides.accent_alt).unwrap_or(theme.magenta);

        let surface_active = if theme.muted == theme.sidebar {
            theme.foreground.opacity(0.10)
        } else {
            theme.muted
        };
        let modal_border = if theme.muted == theme.sidebar {
            theme.border
        } else {
            theme.muted
        };

        DvTheme {
            recess_bg,
            text_secondary,
            surface_active,
            modal_border,
            word_created_bg: theme.success.opacity(0.28),
            word_deleted_bg: theme.danger.opacity(0.28),
            void_bg: recess_bg.opacity(0.60),
            backdrop: if theme.is_dark() {
                recess_bg.opacity(0.67)
            } else {
                theme.foreground.opacity(0.30)
            },
            accent_alt,
        }
    }
}

/// The dv theme extension for the currently-applied theme. Panics if called
/// before the first [`apply_theme`] (mirrors `Theme::global`'s own contract —
/// there is always a theme applied before any window exists).
///
/// First read at R1b (`shell::state_pill`'s callers, for `accent_alt`).
pub fn dv_theme(cx: &App) -> &DvTheme {
    cx.global::<DvTheme>()
}

/// Re-assert just the mono font family, without touching the rest of the
/// theme (colors, mode, ...) — the settings panel's "Mono font" text field
/// calls this on every edit rather than re-parsing + re-applying the whole
/// `ThemeConfig` via [`apply_theme`]. `window.refresh()` makes the change
/// repaint immediately, matching [`apply_theme`]'s live-picker path.
pub fn set_mono_font(family: &str, window: &mut Window, cx: &mut App) {
    Theme::global_mut(cx).mono_font_family = SharedString::from(family.to_string());
    window.refresh();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_theme_json_parses_and_self_describes_correctly() {
        // The one gate that matters most here: a broken theme JSON is a
        // startup panic (see `apply_theme`'s `unwrap_or_else`), so every
        // bundled file must round-trip through `ThemeConfig` cleanly and
        // agree with the registry's own idea of its name/mode.
        for entry in &THEMES {
            let parsed = serde_json::from_str::<ThemeConfig>(entry.json);
            assert!(
                parsed.is_ok(),
                "{} failed to parse as ThemeConfig: {:?}",
                entry.name,
                parsed.err()
            );
            let config = parsed.unwrap();
            assert_eq!(config.name.as_ref(), entry.name);
            assert_eq!(config.mode, entry.mode, "{} has the wrong mode", entry.name);
        }
    }

    #[test]
    fn names_lists_all_four_in_picker_order() {
        let names: Vec<_> = names().collect();
        assert_eq!(
            names,
            vec!["Aura Dark", "Dracula", "Claude Dark", "Claude Light"]
        );
    }

    #[test]
    fn find_resolves_a_known_name() {
        assert_eq!(find("Dracula").name, "Dracula");
        assert_eq!(find("Claude Light").mode, ThemeMode::Light);
    }

    #[test]
    fn find_falls_back_to_default_for_an_unknown_name() {
        assert_eq!(find("Not A Real Theme").name, DEFAULT_THEME);
        assert_eq!(find("").name, DEFAULT_THEME);
    }

    #[test]
    fn default_theme_names_a_real_registry_entry() {
        assert!(THEMES.iter().any(|t| t.name == DEFAULT_THEME));
    }

    /// VERIFY FIRST (the slice plan's own flagged assumption): a `"dv"` key
    /// alongside `"colors"`/`"highlight"` at a theme JSON's top level must not
    /// break `ThemeConfig` parsing, since gpui-component has no schema field
    /// for it. This is the load-bearing fact the whole `DvTheme` mechanism
    /// (embedding overrides straight in `themes/*.json`, no sibling file)
    /// depends on.
    #[test]
    fn theme_config_tolerates_an_unknown_top_level_dv_key() {
        let raw = r##"{
            "name": "Has Dv Section",
            "mode": "dark",
            "colors": { "background": "#000000", "foreground": "#ffffff" },
            "dv": { "recess_bg": "#111111", "text_secondary": "#aaaaaa" }
        }"##;
        let config: ThemeConfig = serde_json::from_str(raw)
            .expect("an unrecognized top-level \"dv\" key must not fail ThemeConfig parsing");
        assert_eq!(config.name.as_ref(), "Has Dv Section");
        assert_eq!(config.mode, ThemeMode::Dark);
    }

    /// Parses `entry.json` as both a `ThemeConfig` (applied to a fresh
    /// `Theme`) and a `DvTheme`, mirroring what `apply_theme` does at
    /// runtime.
    fn resolved_theme_for(entry: &Entry) -> (Theme, DvTheme) {
        let config: ThemeConfig = serde_json::from_str(entry.json).unwrap();
        let mut theme = Theme::default();
        theme.apply_config(&Rc::new(config));
        let dv = DvTheme::resolve(&theme, entry.json);
        (theme, dv)
    }

    #[test]
    fn dv_theme_resolves_the_bundled_override_hues_exactly() {
        // The hand-picked per-theme hex — the values every bundled theme's
        // new `"dv"` JSON section was set to.
        struct Expected {
            theme: &'static str,
            recess_bg: &'static str,
            text_secondary: &'static str,
            accent_alt: &'static str,
        }
        const EXPECTED: [Expected; 4] = [
            Expected {
                theme: "Aura Dark",
                recess_bg: "#0b0a10",
                text_secondary: "#b0aec7",
                accent_alt: "#f694ff",
            },
            Expected {
                theme: "Dracula",
                recess_bg: "#191a21",
                text_secondary: "#adb3cb",
                accent_alt: "#ff79c6",
            },
            Expected {
                theme: "Claude Dark",
                recess_bg: "#191817",
                text_secondary: "#c5c3bc",
                accent_alt: "#a8859b",
            },
            Expected {
                theme: "Claude Light",
                recess_bg: "#e7e4d8",
                text_secondary: "#56544e",
                accent_alt: "#8c6b7e",
            },
        ];

        for expected in &EXPECTED {
            let (_, dv) = resolved_theme_for(find(expected.theme));
            assert_eq!(
                dv.recess_bg,
                Hsla::parse_hex(expected.recess_bg).unwrap(),
                "{}: recess_bg",
                expected.theme
            );
            assert_eq!(
                dv.text_secondary,
                Hsla::parse_hex(expected.text_secondary).unwrap(),
                "{}: text_secondary",
                expected.theme
            );
            assert_eq!(
                dv.accent_alt,
                Hsla::parse_hex(expected.accent_alt).unwrap(),
                "{}: accent_alt",
                expected.theme
            );
        }
    }

    #[test]
    fn dv_theme_computes_alpha_composite_tokens_from_the_resolved_bases() {
        // The four computed tokens are formulas over already-resolved fields
        // (never stored), for every bundled theme.
        for entry in &THEMES {
            let (theme, dv) = resolved_theme_for(entry);

            assert_eq!(
                dv.word_created_bg,
                theme.success.opacity(0.28),
                "{}: word_created_bg",
                entry.name
            );
            assert_eq!(
                dv.word_deleted_bg,
                theme.danger.opacity(0.28),
                "{}: word_deleted_bg",
                entry.name
            );
            assert_eq!(
                dv.void_bg,
                dv.recess_bg.opacity(0.60),
                "{}: void_bg",
                entry.name
            );
            let expected_backdrop = if theme.is_dark() {
                dv.recess_bg.opacity(0.67)
            } else {
                theme.foreground.opacity(0.30)
            };
            assert_eq!(dv.backdrop, expected_backdrop, "{}: backdrop", entry.name);

            // modal_border: `muted` normally; Claude Light's muted ==
            // sidebar collision falls back to `border` so the panel seam
            // never paints the panel's own color (R2 visual review, P3).
            let expected_seam = if theme.muted == theme.sidebar {
                theme.border
            } else {
                theme.muted
            };
            assert_eq!(
                dv.modal_border, expected_seam,
                "{}: modal_border",
                entry.name
            );
            assert_ne!(
                dv.modal_border, theme.sidebar,
                "{}: a modal panel's seam must never equal its own background",
                entry.name
            );
        }
    }

    #[test]
    fn dv_theme_word_tints_match_the_spec_table_color_and_alpha() {
        // Cross-check one theme's computed tokens against the literal
        // expected hex, split into its color
        // part (must match exactly) and alpha (checked as a float — the
        // expected hex alpha suffix is `to_hex`'s *truncating* rendering of
        // 0.28/0.60/0.67, not a bit-exact round-trip target).
        let (theme, dv) = resolved_theme_for(find("Aura Dark"));
        assert!(theme.is_dark());

        let created = Hsla::parse_hex("#61ffca").unwrap(); // highlight.created == success
        assert_eq!(dv.word_created_bg.h, created.h);
        assert_eq!(dv.word_created_bg.s, created.s);
        assert_eq!(dv.word_created_bg.l, created.l);
        assert!((dv.word_created_bg.a - 0.28).abs() < 1e-6);

        let deleted = Hsla::parse_hex("#ff6767").unwrap(); // highlight.deleted == danger
        assert_eq!(dv.word_deleted_bg.h, deleted.h);
        assert_eq!(dv.word_deleted_bg.s, deleted.s);
        assert_eq!(dv.word_deleted_bg.l, deleted.l);
        assert!((dv.word_deleted_bg.a - 0.28).abs() < 1e-6);

        let recess = Hsla::parse_hex("#0b0a10").unwrap();
        assert_eq!(dv.void_bg.h, recess.h);
        assert_eq!(dv.void_bg.s, recess.s);
        assert_eq!(dv.void_bg.l, recess.l);
        assert!((dv.void_bg.a - 0.60).abs() < 1e-6);
        assert!(
            (dv.backdrop.a - 0.67).abs() < 1e-6,
            "dark backdrop uses recess_bg @ 0.67"
        );

        let (light_theme, light_dv) = resolved_theme_for(find("Claude Light"));
        assert!(!light_theme.is_dark());
        assert_eq!(light_dv.backdrop.h, light_theme.foreground.h);
        assert_eq!(light_dv.backdrop.s, light_theme.foreground.s);
        assert_eq!(light_dv.backdrop.l, light_theme.foreground.l);
        assert!(
            (light_dv.backdrop.a - 0.30).abs() < 1e-6,
            "light backdrop uses foreground @ 0.30, not recess_bg"
        );
    }

    #[test]
    fn dv_theme_derivation_fallback_is_non_degenerate_without_a_dv_section() {
        // A synthetic theme with no `"dv"` section at all (a stand-in for a
        // third-party theme JSON) must still resolve to sane, non-degenerate
        // values via the derivation rules — for both a dark and a light mode.
        for mode in ["dark", "light"] {
            let raw = format!(
                r##"{{
                    "name": "Synthetic {mode}",
                    "mode": "{mode}",
                    "colors": {{
                        "background": "#202020",
                        "foreground": "#eeeeee",
                        "sidebar.background": "#151515",
                        "muted.foreground": "#888888",
                        "success.background": "#33cc55",
                        "danger.background": "#cc3333",
                        "base.magenta": "#cc55ff"
                    }}
                }}"##
            );
            let config: ThemeConfig =
                serde_json::from_str(&raw).expect("synthetic theme must parse");
            let mut theme = Theme::default();
            theme.apply_config(&Rc::new(config));
            let dv = DvTheme::resolve(&theme, &raw);

            assert_ne!(
                dv.recess_bg, theme.sidebar,
                "{mode}: recess_bg must differ from sidebar.background"
            );
            assert!(
                dv.recess_bg.a > 0.0,
                "{mode}: recess_bg must not be fully transparent"
            );
            assert!(
                dv.text_secondary.l > 0.0 && dv.text_secondary.l < 1.0,
                "{mode}: text_secondary lightness must be in range"
            );
            assert_eq!(
                dv.accent_alt, theme.magenta,
                "{mode}: accent_alt falls back to base.magenta"
            );
            assert!(
                dv.word_created_bg.a > 0.0 && dv.word_deleted_bg.a > 0.0,
                "{mode}: word tints must be visible, not fully transparent"
            );
            assert!(
                dv.void_bg.a > 0.0 && dv.void_bg.a < 1.0,
                "{mode}: void_bg must be a partial overlay, not opaque or invisible"
            );
            assert!(
                dv.backdrop.a > 0.0 && dv.backdrop.a < 1.0,
                "{mode}: backdrop must be a partial scrim, not opaque or invisible"
            );
        }
    }

    #[test]
    fn bundled_themes_share_the_reference_key_set_exactly() {
        // Backlog item (theme-slice review, P3): `Theme::apply_config` only
        // overwrites keys the JSON actually carries, so a bundled theme
        // missing a key silently inherits the PREVIOUSLY-ACTIVE theme's
        // value on a live swap — invisible in isolation, wrong the moment
        // someone flips between themes. Assert every bundled theme's
        // ThemeConfig-visible key-path set matches the reference theme's
        // exactly (the dv-owned `"dv"` override section is exempt: its
        // keys are Optional by design, with documented derivations).
        fn key_paths(
            prefix: &str,
            value: &serde_json::Value,
            out: &mut std::collections::BTreeSet<String>,
        ) {
            if let Some(obj) = value.as_object() {
                for (key, child) in obj {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    key_paths(&path, child, out);
                    out.insert(path);
                }
            }
        }
        fn theme_config_paths(json: &str) -> std::collections::BTreeSet<String> {
            let value: serde_json::Value = serde_json::from_str(json).expect("theme JSON parses");
            let mut paths = std::collections::BTreeSet::new();
            key_paths("", &value, &mut paths);
            paths.retain(|p| p != "dv" && !p.starts_with("dv."));
            paths
        }

        let reference = theme_config_paths(find(DEFAULT_THEME).json);
        assert!(!reference.is_empty());
        for entry in &THEMES {
            let got = theme_config_paths(entry.json);
            let missing: Vec<_> = reference.difference(&got).collect();
            let extra: Vec<_> = got.difference(&reference).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "{}: key set must match {DEFAULT_THEME}'s exactly \
                 (missing: {missing:?}, extra: {extra:?})",
                entry.name
            );
        }
    }
}
