//! First-run marker: `<data_dir>/dv/setup.json`, next to `settings.json`/
//! `recent.json` (same file-layout convention — see `settings.rs`'s module
//! doc). This is the ONLY thing that decides whether the onboarding page
//! (`onboarding.rs`) auto-opens on launch; the per-launch consistency check
//! itself (`shell.rs`'s `AppShell::spawn_consistency_check`) runs
//! regardless of this marker.
//!
//! Deleting this file simulates first run (per the orchestrator's
//! verification notes) — a missing or corrupt file both fall back to
//! "first run", same never-fail-hard posture as `Settings::load`/
//! `RecentStore::load`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Whether the onboarding page has ever been shown-and-dismissed, and
/// which app version that happened at (recorded for a future slice that
/// might want to re-show onboarding after a meaningful version bump — this
/// slice never reads `completed_version` for anything but presence).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SetupState {
    pub completed_version: Option<String>,
}

impl SetupState {
    /// Load from the default location. Missing/unreadable/corrupt all fall
    /// back to `SetupState::default()` (i.e. "first run") — a broken
    /// setup.json must never crash startup or wedge the onboarding decision.
    pub fn load() -> Self {
        default_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<SetupState>(&bytes).ok())
            .unwrap_or_default()
    }

    /// `true` until the onboarding page has been dismissed once.
    pub fn is_first_run(&self) -> bool {
        self.completed_version.is_none()
    }

    /// Record that onboarding has been seen-and-dismissed at `version` —
    /// re-reads and re-writes the whole file (not just this state's own
    /// copy) so a concurrent write from elsewhere in the process isn't
    /// clobbered; there's only ever one field here today, so this is
    /// mostly future-proofing against a second one showing up later.
    /// Best-effort, same silent-failure posture as `Settings::save`.
    pub fn mark_complete(version: &str) {
        let mut state = Self::load();
        state.completed_version = Some(version.to_string());
        state.save();
    }

    fn save(&self) {
        let Some(path) = default_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            // Atomic-ish: write a temp sibling then rename over the target
            // (same pattern as `Settings::save`/`RecentStore::save`).
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

fn default_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("dv").join("setup.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_first_run() {
        assert!(SetupState::default().is_first_run());
    }

    #[test]
    fn completed_state_is_not_first_run() {
        let state = SetupState {
            completed_version: Some("0.1.0".to_string()),
        };
        assert!(!state.is_first_run());
    }

    #[test]
    fn missing_fields_fall_back_to_default() {
        let parsed: SetupState = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, SetupState::default());
        assert!(parsed.is_first_run());
    }

    #[test]
    fn round_trips_through_json() {
        let state = SetupState {
            completed_version: Some("0.2.0".to_string()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: SetupState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn corrupt_json_is_rejected_by_from_slice() {
        // `SetupState::load()` chains this through `.ok()` into
        // `unwrap_or_default()` — exercised here at the parse-result level,
        // matching `settings.rs`'s equivalent test (`load()` itself depends
        // on the real data dir).
        let result = serde_json::from_slice::<SetupState>(b"not json");
        assert!(result.is_err());
    }
}
