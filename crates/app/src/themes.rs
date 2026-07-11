//! Bundled theme registry: four `gpui-component` `ThemeConfig`s embedded at
//! compile time, switched live via the theme picker (`ctrl-shift-t`, see
//! `shell.rs`) and persisted through `settings.rs`. Replaces the old
//! `main.rs::apply_aura_theme` — same load-and-apply mechanics, just table-
//! driven over more than one theme.

use std::rc::Rc;

use gpui::{App, Window};
use gpui_component::{Theme, ThemeConfig, ThemeMode};

/// Overrides every bundled theme's `mono_font_family` uniformly (font
/// deliverable) — set here in code rather than per-theme JSON so all four
/// themes get JetBrains Mono without relying on each theme author
/// remembering to set `mono_font.family` for themselves. Re-asserted after
/// every [`apply_theme`] call.
pub const MONO_FONT_FAMILY: &str = "JetBrains Mono";

/// Falls back here on first launch (no settings.json yet) and on any
/// unrecognized/corrupt persisted theme name.
pub const DEFAULT_THEME: &str = "Aura Dark";

struct Entry {
    name: &'static str,
    json: &'static str,
    mode: ThemeMode,
}

/// The bundled registry, in picker display order. `aura-dark.json` is kept
/// byte-identical to the pre-existing reference theme; the other three are
/// new siblings covering the exact same key set.
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
    THEMES
        .iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| {
            THEMES
                .iter()
                .find(|t| t.name == DEFAULT_THEME)
                .expect("DEFAULT_THEME must name a registry entry")
        })
}

/// Parse and apply the bundled theme called `name` (falling back to
/// [`DEFAULT_THEME`] if unrecognized), then re-assert the JetBrains Mono
/// override so it sticks regardless of which theme JSON is active.
///
/// `window` is `None` at startup (no window exists yet when the App-level
/// theme is first applied in `main.rs`); `Some` from the live picker, whose
/// `window.refresh()` (via `Theme::change`) is what makes the swap repaint
/// immediately instead of waiting for the next unrelated re-render.
pub fn apply_theme(name: &str, window: Option<&mut Window>, cx: &mut App) {
    let entry = find(name);
    let config: ThemeConfig = serde_json::from_str(entry.json).unwrap_or_else(|err| {
        panic!(
            "bundled theme {:?} must parse as a gpui-component ThemeConfig: {err}",
            entry.name
        )
    });
    Theme::change(entry.mode, window, cx);
    Theme::global_mut(cx).apply_config(&Rc::new(config));
    Theme::global_mut(cx).mono_font_family = MONO_FONT_FAMILY.into();
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
}
