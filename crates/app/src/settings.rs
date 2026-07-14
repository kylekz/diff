//! Persisted app-wide settings: theme (+ follow-OS light/dark pair), fonts,
//! diff display defaults. Same file-layout convention as `recent.rs`: one
//! small JSON file next to `recent.json` in the platform data dir, atomic
//! tmp+rename write, and a missing or corrupt file falls back to defaults
//! rather than erroring — a broken settings.json must never be a startup
//! crash.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::themes::DEFAULT_THEME;

/// Bundled JetBrains Mono is still the default mono family — see
/// `themes.rs`'s (former) `MONO_FONT_FAMILY` — but it's now a user setting
/// rather than a hardcoded override, so a settings.json can name any font
/// family installed on the machine (or bundled). An unresolvable name just
/// falls back to a proportional sans (observed — not a monospace fallback,
/// despite what you'd expect from the font *system*) — there's no
/// validation here, matching the settings panel's mono-font field (see
/// shell.rs).
pub const DEFAULT_MONO_FONT: &str = "JetBrains Mono";

/// Today's effective diff/code text size, in px, kept as the default so a
/// settings.json that never sets `mono_font_size` renders byte-identical to
/// before this setting existed: diff rows render with `.text_sm()`
/// (`rems(0.875)`), and gpui-component's `Theme::font_size` — which drives
/// `window.rem_size()` (see `gpui_component::Root::render`) — defaults to
/// 16px and isn't overridden by any of the four bundled theme JSONs. So
/// `0.875 * 16 = 14`.
pub const DEFAULT_MONO_FONT_SIZE: f32 = 14.0;

/// Default follow-OS light/dark pairing. `DEFAULT_DARK_THEME` intentionally
/// matches `themes::DEFAULT_THEME` (today's plain default), so turning on
/// follow-OS for the first time in dark mode doesn't visibly change anything.
pub const DEFAULT_LIGHT_THEME: &str = "Claude Light";

/// Default number of context lines shown around each hunk — mirrors
/// `dv_core::DiffOptions::default().context_lines` (git's own default).
pub const DEFAULT_CONTEXT_LINES: u32 = 3;

/// Clamp for `mono_font_size` (the settings panel's "Font size" stepper,
/// `set_setting`, and [`Settings::load`] all share this one range — see
/// each's call to [`clamp`](f32::clamp) with these bounds).
pub const MONO_FONT_SIZE_MIN: f32 = 8.;
pub const MONO_FONT_SIZE_MAX: f32 = 24.;
/// Clamp for `context_lines`. `dv_core`'s `DiffOptions` itself has no upper
/// bound, but an unbounded value is a footgun — 20 is generously past any
/// reasonable review setting. Shared the same way as the font-size clamp
/// above.
pub const CONTEXT_LINES_MIN: u32 = 0;
pub const CONTEXT_LINES_MAX: u32 = 20;

/// Today's hardcoded sidebar width (`shell.rs`'s recent-review navigator) —
/// kept as the default so a settings.json that never sets `sidebar_width`
/// renders byte-identical to before drag-to-resize existed (Phase 4
/// deliverable 5).
pub const DEFAULT_SIDEBAR_WIDTH: f32 = 280.;
/// Today's hardcoded review-summary panel width (`workspace.rs`'s
/// `render_summary`) — same "byte-identical until touched" reasoning as
/// `DEFAULT_SIDEBAR_WIDTH`.
pub const DEFAULT_SUMMARY_WIDTH: f32 = 320.;
/// Clamp for `sidebar_width`, shared by the drag handle
/// (`shell.rs`'s `render_sidebar_resize_handle`), `set_setting`, and
/// [`Settings::load`] — same pattern as the font-size/context-lines clamps
/// above.
pub const SIDEBAR_WIDTH_MIN: f32 = 180.;
pub const SIDEBAR_WIDTH_MAX: f32 = 480.;
/// Clamp for `summary_width` (`workspace.rs`'s `render_summary_resize_handle`).
pub const SUMMARY_WIDTH_MIN: f32 = 240.;
pub const SUMMARY_WIDTH_MAX: f32 = 560.;

/// Which diff layout a freshly opened review starts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewModeSetting {
    #[default]
    Unified,
    Split,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Name of the bundled theme to apply when `follow_os_appearance` is
    /// off (see `themes.rs`'s registry) — the explicit, manually-picked
    /// theme. Left untouched while follow-OS is on, so turning follow-OS
    /// back off restores whatever was picked last.
    pub theme: String,
    /// When on, the active theme tracks the OS's light/dark appearance
    /// (`light_theme`/`dark_theme` below) instead of `theme`. Picking a
    /// theme explicitly (picker or settings panel) always turns this back
    /// off — explicit beats automatic (see `shell.rs`'s `choose_theme`).
    pub follow_os_appearance: bool,
    /// Theme applied when `follow_os_appearance` is on and the OS is in
    /// light mode.
    pub light_theme: String,
    /// Theme applied when `follow_os_appearance` is on and the OS is in
    /// dark mode.
    pub dark_theme: String,
    /// Mono font family for diff/code text — free text, not validated
    /// against installed fonts (gpui falls back silently for an unresolved
    /// family; see the settings panel's note).
    pub mono_font: String,
    /// Mono font size, in px, for diff/code text. Also drives row height
    /// (see `workspace.rs`'s `row_height`). Clamped to
    /// `MONO_FONT_SIZE_MIN..=MONO_FONT_SIZE_MAX` by the settings panel and
    /// `set_setting`, and again by [`Settings::load`] — so a hand-edited
    /// settings.json outside that range gets pulled back in range at load
    /// time rather than rendering at whatever out-of-bounds size was saved.
    pub mono_font_size: f32,
    /// Context lines shown around each diff hunk (`dv_core::DiffOptions`'s
    /// existing knob — see `workspace.rs`'s `compute_diff`). Clamped to
    /// `CONTEXT_LINES_MIN..=CONTEXT_LINES_MAX` the same way as
    /// `mono_font_size` above.
    pub context_lines: u32,
    /// View mode (unified/split) a freshly opened review starts in.
    pub view_mode_default: ViewModeSetting,
    /// Sidebar (review navigator) width, in px — `shell.rs`'s drag handle on
    /// its inner edge (docs/phase-4-settings-and-theming.md deliverable 5).
    /// Clamped to `SIDEBAR_WIDTH_MIN..=SIDEBAR_WIDTH_MAX`, same re-clamp
    /// posture as `mono_font_size`/`context_lines` (see [`Settings::load`]).
    pub sidebar_width: f32,
    /// Review-summary panel width, in px — `workspace.rs`'s drag handle on
    /// its inner (left) edge. Clamped to
    /// `SUMMARY_WIDTH_MIN..=SUMMARY_WIDTH_MAX`.
    pub summary_width: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME.to_string(),
            follow_os_appearance: false,
            light_theme: DEFAULT_LIGHT_THEME.to_string(),
            dark_theme: DEFAULT_THEME.to_string(),
            mono_font: DEFAULT_MONO_FONT.to_string(),
            mono_font_size: DEFAULT_MONO_FONT_SIZE,
            context_lines: DEFAULT_CONTEXT_LINES,
            view_mode_default: ViewModeSetting::Unified,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            summary_width: DEFAULT_SUMMARY_WIDTH,
        }
    }
}

impl Settings {
    /// Load from the default location (`<data_dir>/dv/settings.json`).
    /// Missing file, unreadable file, or unparseable JSON all fall back to
    /// [`Settings::default`] silently — settings are a nicety, not
    /// something worth surfacing an error dialog over. Numeric fields are
    /// re-clamped after parsing (same bounds the panel's own steppers /
    /// drag handles enforce) so a hand-edited settings.json can't sneak an
    /// out-of-range value past both — see [`Self::clamp_numeric_fields`].
    pub fn load() -> Self {
        let mut settings = default_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok())
            .unwrap_or_default();
        settings.clamp_numeric_fields();
        settings
    }

    /// Pull every numeric field back into its documented range. Shared by
    /// [`Self::load`] (a hand-edited or stale settings.json) and the drag
    /// handles / `set_setting`'s own clamps, and exercised directly by
    /// tests below without touching the real data dir.
    pub(crate) fn clamp_numeric_fields(&mut self) {
        self.mono_font_size = self
            .mono_font_size
            .clamp(MONO_FONT_SIZE_MIN, MONO_FONT_SIZE_MAX);
        self.context_lines = self
            .context_lines
            .clamp(CONTEXT_LINES_MIN, CONTEXT_LINES_MAX);
        self.sidebar_width = self
            .sidebar_width
            .clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX);
        self.summary_width = self
            .summary_width
            .clamp(SUMMARY_WIDTH_MIN, SUMMARY_WIDTH_MAX);
    }

    /// Best-effort save; silently does nothing if the data dir can't be
    /// determined or the write fails (same posture as
    /// `dv_core::ReviewIndex::save`).
    pub fn save(&self) {
        let Some(path) = default_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            // Atomic-ish: write a temp sibling then rename over the target.
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    /// Which bundled theme should be active right now. When
    /// `follow_os_appearance` is on, `os_is_dark` (the live OS/window
    /// appearance) picks between `light_theme`/`dark_theme`; otherwise the
    /// explicitly-chosen `theme` wins. Doesn't validate the resolved name
    /// against the registry — `themes::apply_theme` already falls back to
    /// `DEFAULT_THEME` for an unknown/stale one.
    pub fn effective_theme(&self, os_is_dark: bool) -> &str {
        if self.follow_os_appearance {
            if os_is_dark {
                &self.dark_theme
            } else {
                &self.light_theme
            }
        } else {
            &self.theme
        }
    }
}

fn default_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("dv").join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_todays_behavior() {
        let s = Settings::default();
        assert_eq!(s.theme, "Aura Dark");
        assert!(!s.follow_os_appearance);
        assert_eq!(s.light_theme, "Claude Light");
        assert_eq!(s.dark_theme, "Aura Dark");
        assert_eq!(s.mono_font, "JetBrains Mono");
        assert_eq!(s.mono_font_size, 14.0);
        assert_eq!(s.context_lines, 3);
        assert_eq!(s.view_mode_default, ViewModeSetting::Unified);
        assert_eq!(s.sidebar_width, 280.0);
        assert_eq!(s.summary_width, 320.0);
    }

    #[test]
    fn round_trips_through_json() {
        let settings = Settings {
            theme: "Claude Dark".to_string(),
            follow_os_appearance: true,
            light_theme: "Claude Light".to_string(),
            dark_theme: "Dracula".to_string(),
            mono_font: "Fira Code".to_string(),
            mono_font_size: 18.0,
            context_lines: 5,
            view_mode_default: ViewModeSetting::Split,
            sidebar_width: 340.0,
            summary_width: 400.0,
        };
        let json = serde_json::to_string(&settings).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, settings);
    }

    #[test]
    fn missing_fields_fall_back_to_default() {
        // An empty object (e.g. a pre-phase-4 settings.json with only
        // `theme`, or truly empty) must still parse, picking up every
        // default.
        let parsed: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, Settings::default());
    }

    #[test]
    fn old_settings_json_with_only_theme_still_loads() {
        // Exactly what phase-1/2/3's settings.json looked like on disk —
        // every new field must fill in with defaults rather than fail to
        // parse. Assert every one of them, not just a couple — a field
        // added later without its default correctly wired up should fail
        // this test, not just the fields someone happened to remember to
        // check.
        let parsed: Settings = serde_json::from_str(r#"{"theme":"Dracula"}"#).unwrap();
        assert_eq!(parsed.theme, "Dracula");
        assert_eq!(
            parsed.follow_os_appearance,
            Settings::default().follow_os_appearance
        );
        assert_eq!(parsed.light_theme, Settings::default().light_theme);
        assert_eq!(parsed.dark_theme, Settings::default().dark_theme);
        assert_eq!(parsed.mono_font, Settings::default().mono_font);
        assert_eq!(parsed.mono_font_size, DEFAULT_MONO_FONT_SIZE);
        assert_eq!(parsed.context_lines, Settings::default().context_lines);
        assert_eq!(parsed.view_mode_default, ViewModeSetting::Unified);
        assert_eq!(parsed.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
        assert_eq!(parsed.summary_width, DEFAULT_SUMMARY_WIDTH);
    }

    // --- clamp_numeric_fields (Phase 4 deliverable 5: sidebar/summary drag
    // handles share this clamp with `mono_font_size`/`context_lines`) -------

    #[test]
    fn clamp_numeric_fields_pulls_high_values_down_to_max() {
        let mut s = Settings {
            mono_font_size: 999.,
            context_lines: 999,
            sidebar_width: 999.,
            summary_width: 9999.,
            ..Settings::default()
        };
        s.clamp_numeric_fields();
        assert_eq!(s.mono_font_size, MONO_FONT_SIZE_MAX);
        assert_eq!(s.context_lines, CONTEXT_LINES_MAX);
        assert_eq!(s.sidebar_width, SIDEBAR_WIDTH_MAX);
        assert_eq!(s.summary_width, SUMMARY_WIDTH_MAX);
    }

    #[test]
    fn clamp_numeric_fields_pulls_low_values_up_to_min() {
        let mut s = Settings {
            mono_font_size: -5.,
            context_lines: 0, // already at CONTEXT_LINES_MIN — exercised for symmetry
            sidebar_width: 0.,
            summary_width: -100.,
            ..Settings::default()
        };
        s.clamp_numeric_fields();
        assert_eq!(s.mono_font_size, MONO_FONT_SIZE_MIN);
        assert_eq!(s.context_lines, CONTEXT_LINES_MIN);
        assert_eq!(s.sidebar_width, SIDEBAR_WIDTH_MIN);
        assert_eq!(s.summary_width, SUMMARY_WIDTH_MIN);
    }

    #[test]
    fn clamp_numeric_fields_leaves_in_range_values_untouched() {
        let mut s = Settings {
            sidebar_width: 300.,
            summary_width: 350.,
            ..Settings::default()
        };
        s.clamp_numeric_fields();
        assert_eq!(s.sidebar_width, 300.);
        assert_eq!(s.summary_width, 350.);
    }

    #[test]
    fn corrupt_json_is_rejected_by_from_slice() {
        // `Settings::load()` chains this through `.ok()` into
        // `unwrap_or_default()` — exercised here at the parse-result level,
        // since `load()` itself depends on the real data dir.
        let result = serde_json::from_slice::<Settings>(b"not json");
        assert!(result.is_err());
    }

    #[test]
    fn truncated_json_is_rejected_by_from_slice() {
        // A write torn mid-flush (crash/power-loss between the tmp write
        // and rename shouldn't happen, but a partially-written file some
        // other way might) must not panic `load()`.
        let result = serde_json::from_slice::<Settings>(br#"{"theme":"Dra"#);
        assert!(result.is_err());
    }

    // --- effective_theme --------------------------------------------------

    #[test]
    fn effective_theme_is_explicit_when_not_following_os() {
        let s = Settings {
            theme: "Dracula".to_string(),
            follow_os_appearance: false,
            ..Settings::default()
        };
        assert_eq!(s.effective_theme(true), "Dracula");
        assert_eq!(s.effective_theme(false), "Dracula");
    }

    #[test]
    fn effective_theme_follows_os_when_enabled() {
        let s = Settings {
            follow_os_appearance: true,
            light_theme: "Claude Light".to_string(),
            dark_theme: "Claude Dark".to_string(),
            ..Settings::default()
        };
        assert_eq!(s.effective_theme(true), "Claude Dark");
        assert_eq!(s.effective_theme(false), "Claude Light");
    }
}
