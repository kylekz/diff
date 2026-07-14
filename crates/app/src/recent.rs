//! Persisted seed list of once-recently-opened reviews, read once at
//! startup to migrate a pre-Phase-6 sidebar into the cross-repo review index
//! (`dv_core::ReviewIndex` — docs/phase-6-review-navigator.md's S6b/S6c
//! index split, cross-cutting risk G: a user with only a `recent.json` must
//! not lose their sidebar on upgrade). One JSON file in the platform data
//! dir; best-effort (a corrupt or missing file yields an empty list rather
//! than an error the user has to see).
//!
//! **Read-only as of S6c.** The sidebar itself (`AppShell::render_review_card`)
//! and review selection (`AppShell::open_review`/`open_review_row`) are
//! driven entirely by the index now — nothing writes `recent.json` anymore
//! (that used to be `RecentStore::touch`, called from every `open_review`;
//! the index's own `ReviewIndex::upsert` is the equivalent write path
//! today). This file only still exists to seed `AppShell::hydrate_index`'s
//! startup walk with locations the index hasn't hydrated yet.

use std::path::PathBuf;

use dv_core::{DiffSource, RepoLocation};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentEntry {
    pub location: RepoLocation,
    pub source: DiffSource,
    /// Display label, e.g. `difftest — working tree`. Pre-S6c UI text; kept
    /// on the struct only because it's part of the persisted schema — no
    /// code reads it anymore (the index derives its own title via
    /// `dv_core::entry_title`).
    pub title: String,
    /// When this review was last opened. Recency sorts the list at *load*
    /// only. Defaults to 0 for entries persisted before this field.
    #[serde(default)]
    pub last_opened_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    entries: Vec<RecentEntry>,
}

pub struct RecentStore {
    entries: Vec<RecentEntry>,
}

impl RecentStore {
    /// Load from the default location (`<data_dir>/dv/recent.json`). Missing
    /// or unparseable file → empty list.
    pub fn load() -> Self {
        let mut entries = default_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<Persisted>(&bytes).ok())
            .map(|p| p.entries)
            .unwrap_or_default();
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_opened_ms));
        Self { entries }
    }

    pub fn entries(&self) -> &[RecentEntry] {
        &self.entries
    }
}

fn default_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("dv").join("recent.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wt(name: &str) -> RecentEntry {
        RecentEntry {
            location: RepoLocation::Local(PathBuf::from(format!("D:\\code\\{name}"))),
            source: DiffSource::WorkingTree,
            title: format!("{name} — working tree"),
            last_opened_ms: 0,
        }
    }

    #[test]
    fn load_sorts_by_recency_descending() {
        let mut entries = [wt("old"), wt("new")];
        entries[0].last_opened_ms = 100;
        entries[1].last_opened_ms = 200;
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_opened_ms));
        assert_eq!(entries[0].location, wt("new").location);
        assert_eq!(entries[1].location, wt("old").location);
    }

    #[test]
    fn recent_entry_round_trips_through_json() {
        let entry = wt("difftest");
        let json = serde_json::to_string(&entry).unwrap();
        let round_tripped: RecentEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, entry);
    }

    #[test]
    fn old_recent_json_missing_last_opened_ms_loads() {
        // A file written before that field existed must still parse
        // (forward compat, same posture the index and settings take).
        let mut value = serde_json::to_value(wt("difftest")).unwrap();
        value.as_object_mut().unwrap().remove("last_opened_ms");
        let parsed: RecentEntry = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.last_opened_ms, 0);
    }
}
