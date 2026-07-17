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
    /// [`dv_core::provision::ConsistencyReport::needs_human_fingerprint`]
    /// (equivalently, `OnboardingPage::needs_human_fingerprint`) of every
    /// DISTINCT onboarding page the user has explicitly dismissed with a
    /// needs-human row showing, most-recent last, capped at
    /// [`MAX_DISMISSED_DRIFT_FINGERPRINTS`] (phase-8 capstone review, P3).
    /// Without this, dismissal was session-only (`shell.rs`'s
    /// `drift_page_dismissed`), so a permanent-by-choice state — no `gh`
    /// installed, `gh` deliberately left unauthenticated, a declined vtsls
    /// consent — re-popped the "Setup status" modal on every single launch
    /// (or first WSL repo open) of every session, forever.
    ///
    /// A SET rather than a single slot (capstone integration review, P3,
    /// on top of the first cut of this fix): a machine with more than one
    /// live WSL distro can have MULTIPLE simultaneously-valid dismissed
    /// shapes — e.g. Ubuntu and Debian both missing vtsls, checked (and
    /// dismissed) one repo-open at a time, so the page's row set differs
    /// launch to launch depending on which distro's repo was opened first.
    /// A single remembered fingerprint made every launch that didn't
    /// happen to match the last one dismissed re-pop the modal, clobbering
    /// the other shape's dismissal on close — an unbounded back-and-forth
    /// across sessions. Checked via [`Self::is_drift_dismissed`]: any
    /// member match stays silent, anything else — including a fresh,
    /// different drift — still surfaces normally.
    pub dismissed_drift_fingerprints: Vec<String>,
}

/// Cap on [`SetupState::dismissed_drift_fingerprints`] — generous enough to
/// cover every shape a realistic multi-distro machine's drift can take
/// (each distro's own missing-component combination), small enough that a
/// pathological flip-flopper can't grow `setup.json` without bound. Oldest
/// entry is evicted first once this is exceeded.
const MAX_DISMISSED_DRIFT_FINGERPRINTS: usize = 16;

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

    /// Record `fingerprint` (from `OnboardingPage::needs_human_fingerprint`)
    /// as one of the needs-human drifts the user has explicitly seen and
    /// dismissed — same read-modify-write pattern as `Self::mark_complete`,
    /// and the same best-effort, silent-failure posture as `Self::save`.
    /// Deduplicates (re-dismissing an already-known shape just moves it to
    /// most-recent) and evicts the OLDEST entry once
    /// [`MAX_DISMISSED_DRIFT_FINGERPRINTS`] is exceeded, so a machine that
    /// keeps producing genuinely new shapes can't grow this file forever.
    pub fn mark_drift_dismissed(fingerprint: String) {
        let mut state = Self::load();
        record_dismissal(&mut state.dismissed_drift_fingerprints, fingerprint);
        state.save();
    }

    /// `true` when `fingerprint` matches ANY previously-dismissed shape —
    /// the multi-shape counterpart to the old single-slot equality check
    /// (phase-8 capstone integration review, P3). Consulted by
    /// `shell.rs`'s `apply_consistency_report` before auto-surfacing.
    pub fn is_drift_dismissed(&self, fingerprint: &str) -> bool {
        self.dismissed_drift_fingerprints
            .iter()
            .any(|f| f == fingerprint)
    }

    fn save(&self) {
        let Some(path) = default_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            // Atomic-ish: write a pid-suffixed temp sibling then rename over
            // the target (same pattern + two-process rationale as
            // `Settings::save`).
            let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

fn default_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("dv").join("setup.json"))
}

/// Pure dedup-then-cap logic behind [`SetupState::mark_drift_dismissed`],
/// split out so it's testable without touching the real data dir: move
/// `fingerprint` to the end (most-recent) if already present, otherwise
/// append it, then evict from the front until `list.len()` is back within
/// [`MAX_DISMISSED_DRIFT_FINGERPRINTS`].
fn record_dismissal(list: &mut Vec<String>, fingerprint: String) {
    list.retain(|f| *f != fingerprint);
    list.push(fingerprint);
    let len = list.len();
    if len > MAX_DISMISSED_DRIFT_FINGERPRINTS {
        list.drain(..len - MAX_DISMISSED_DRIFT_FINGERPRINTS);
    }
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
            ..Default::default()
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
            dismissed_drift_fingerprints: vec!["deadbeef".to_string(), "cafef00d".to_string()],
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

    #[test]
    fn missing_dismissed_fingerprints_falls_back_to_empty() {
        // An old setup.json written before this field existed must still
        // parse (`#[serde(default)]` on the struct) rather than reject the
        // whole file and fall back to first-run.
        let parsed: SetupState = serde_json::from_str(r#"{"completed_version":"0.1.0"}"#).unwrap();
        assert_eq!(parsed.dismissed_drift_fingerprints, Vec::<String>::new());
        assert!(!parsed.is_first_run());
    }

    #[test]
    fn old_single_slot_field_is_ignored_not_rejected() {
        // The very first cut of this fix (capstone review) shipped a
        // single `dismissed_drift_fingerprint` string field before being
        // upgraded to a set (this same P3, addressed more thoroughly) — a
        // setup.json written by that build must still parse rather than
        // reject the whole file, same never-fail-hard posture as every
        // other unknown/stale field here.
        let parsed: SetupState = serde_json::from_str(
            r#"{"completed_version":"0.1.0","dismissed_drift_fingerprint":"deadbeef"}"#,
        )
        .unwrap();
        assert_eq!(parsed.dismissed_drift_fingerprints, Vec::<String>::new());
    }

    #[test]
    fn is_drift_dismissed_checks_membership_not_last_write() {
        let state = SetupState {
            dismissed_drift_fingerprints: vec![
                "ubuntu-shape".to_string(),
                "debian-shape".to_string(),
            ],
            ..Default::default()
        };
        // Both shapes stay recognized regardless of dismissal order — the
        // whole point vs. the single-slot version this replaces (P3).
        assert!(state.is_drift_dismissed("ubuntu-shape"));
        assert!(state.is_drift_dismissed("debian-shape"));
        assert!(!state.is_drift_dismissed("something-new"));
    }

    #[test]
    fn record_dismissal_dedups_and_moves_to_most_recent() {
        let mut list = vec!["a".to_string(), "b".to_string()];
        record_dismissal(&mut list, "a".to_string());
        assert_eq!(list, vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn record_dismissal_evicts_oldest_past_the_cap() {
        let mut list: Vec<String> = Vec::new();
        for i in 0..MAX_DISMISSED_DRIFT_FINGERPRINTS + 3 {
            record_dismissal(&mut list, format!("fp-{i}"));
        }
        assert_eq!(list.len(), MAX_DISMISSED_DRIFT_FINGERPRINTS);
        // The earliest entries were evicted; the most recent ones remain.
        assert!(!list.contains(&"fp-0".to_string()));
        assert!(list.contains(&format!("fp-{}", MAX_DISMISSED_DRIFT_FINGERPRINTS + 2)));
    }
}
