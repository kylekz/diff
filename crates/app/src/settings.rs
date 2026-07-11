//! Persisted app-wide settings — currently just the active theme name.
//! Same file-layout convention as `recent.rs`: one small JSON file next to
//! `recent.json` in the platform data dir, atomic tmp+rename write, and a
//! missing or corrupt file falls back to defaults rather than erroring —
//! a broken settings.json must never be a startup crash.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::themes::DEFAULT_THEME;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Name of the bundled theme to apply at startup (see `themes.rs`'s
    /// registry). Room to grow: more fields can be added here, each with
    /// its own `#[serde(default)]`-friendly type, without breaking older
    /// settings.json files.
    pub theme: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME.to_string(),
        }
    }
}

impl Settings {
    /// Load from the default location (`<data_dir>/dv/settings.json`).
    /// Missing file, unreadable file, or unparseable JSON all fall back to
    /// [`Settings::default`] silently — settings are a nicety, not
    /// something worth surfacing an error dialog over.
    pub fn load() -> Self {
        default_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok())
            .unwrap_or_default()
    }

    /// Best-effort save; silently does nothing if the data dir can't be
    /// determined or the write fails (same posture as `RecentStore::save`).
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
}

fn default_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("dv").join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_is_aura_dark() {
        assert_eq!(Settings::default().theme, "Aura Dark");
    }

    #[test]
    fn round_trips_through_json() {
        let settings = Settings {
            theme: "Claude Dark".to_string(),
        };
        let json = serde_json::to_string(&settings).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, settings);
    }

    #[test]
    fn missing_fields_fall_back_to_default() {
        // An empty object (e.g. a future settings.json missing today's only
        // field) must still parse, picking up the default theme.
        let parsed: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, Settings::default());
    }

    #[test]
    fn corrupt_json_is_rejected_by_from_slice() {
        // `Settings::load()` chains this through `.ok()` into
        // `unwrap_or_default()` — exercised here at the parse-result level,
        // since `load()` itself depends on the real data dir.
        let result = serde_json::from_slice::<Settings>(b"not json");
        assert!(result.is_err());
    }
}
