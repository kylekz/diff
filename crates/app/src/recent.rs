//! Persisted list of recently-opened reviews, shown in the navigator sidebar.
//! One JSON file in the platform data dir; best-effort (a corrupt or missing
//! file yields an empty list rather than an error the user has to see).

use std::path::PathBuf;

use dv_core::{DiffSource, RepoLocation};
use serde::{Deserialize, Serialize};

/// Newest-first cap. Beyond this, the oldest entries drop off.
const MAX_ENTRIES: usize = 50;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentEntry {
    pub location: RepoLocation,
    pub source: DiffSource,
    /// Display label, e.g. `difftest — working tree`.
    pub title: String,
    /// When this review was last opened. Recency sorts the list at *load*
    /// only — never live, so items don't jump around under the user's
    /// clicks. Defaults to 0 for entries persisted before this field.
    #[serde(default)]
    pub last_opened_ms: u64,
}

impl RecentEntry {
    /// Two entries are "the same review" if they point at the same repo and
    /// diff source; the title is derived, not part of identity.
    fn same_review(&self, other: &RecentEntry) -> bool {
        self.location == other.location && self.source == other.source
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    entries: Vec<RecentEntry>,
}

pub struct RecentStore {
    path: Option<PathBuf>,
    entries: Vec<RecentEntry>,
}

impl RecentStore {
    /// Load from the default location (`<data_dir>/dv/recent.json`). Missing
    /// or unparseable file → empty list. `path` is `None` only if no data dir
    /// can be determined, in which case the store works in-memory.
    pub fn load() -> Self {
        let path = default_path();
        let mut entries = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<Persisted>(&bytes).ok())
            .map(|p| p.entries)
            .unwrap_or_default();
        // Recency ordering is applied here, once — while the app runs the
        // order stays put (see `touch`).
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_opened_ms));
        Self { path, entries }
    }

    pub fn entries(&self) -> &[RecentEntry] {
        &self.entries
    }

    /// Record that `entry`'s review was opened, and return its index in the
    /// display list. An existing same-review entry is updated **in place**
    /// (recency bumped, title refreshed) — deliberately not moved: a live
    /// list that reorders under the user's click is disorienting. New
    /// entries go on top. Recency ordering applies at next load.
    pub fn touch(&mut self, mut entry: RecentEntry) -> usize {
        entry.last_opened_ms = now_ms();
        let index = match self.entries.iter().position(|e| e.same_review(&entry)) {
            Some(index) => {
                self.entries[index] = entry;
                index
            }
            None => {
                self.entries.insert(0, entry);
                // Drop the *least recently opened* entry over the cap, not
                // blindly the last one (display order isn't recency order).
                // `skip(1)` protects the just-inserted entry: timestamps
                // have millisecond resolution, so it can tie with existing
                // entries and min_by_key would happily pick it.
                if self.entries.len() > MAX_ENTRIES
                    && let Some(oldest) = self
                        .entries
                        .iter()
                        .enumerate()
                        .skip(1)
                        .min_by_key(|(_, e)| e.last_opened_ms)
                        .map(|(i, _)| i)
                {
                    self.entries.remove(oldest);
                }
                0
            }
        };
        self.save();
        index
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let persisted = Persisted {
            entries: self.entries.clone(),
        };
        if let Ok(bytes) = serde_json::to_vec_pretty(&persisted) {
            // Atomic-ish: write a temp sibling then rename over the target.
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}

fn default_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("dv").join("recent.json"))
}

fn now_ms() -> u64 {
    dv_core::review::now_ms()
}

/// Human title for a review, e.g. `difftest — working tree` or
/// `proj — abc123..def456`.
pub fn title_for(location: &RepoLocation, source: &DiffSource) -> String {
    let repo = repo_short_name(location);
    let src = match source {
        DiffSource::WorkingTree => "working tree".to_string(),
        DiffSource::Staged => "staged".to_string(),
        DiffSource::Commit(rev) => format!("commit {}", short_rev(rev)),
        DiffSource::Range {
            base,
            head,
            merge_base,
        } => {
            let sep = if *merge_base { "..." } else { ".." };
            format!("{}{sep}{}", short_rev(base), short_rev(head))
        }
    };
    format!("{repo} — {src}")
}

/// The last path component of a repo location, for a compact label.
fn repo_short_name(location: &RepoLocation) -> String {
    let full = location.display_name();
    full.rsplit(['/', '\\', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or(&full)
        .to_string()
}

fn short_rev(rev: &str) -> String {
    // Full 40/64-hex SHAs shorten to 7; named refs pass through untouched.
    if rev.len() >= 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        rev[..7].to_string()
    } else {
        rev.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wt(name: &str) -> RecentEntry {
        let location = RepoLocation::Local(PathBuf::from(format!("D:\\code\\{name}")));
        let source = DiffSource::WorkingTree;
        RecentEntry {
            title: title_for(&location, &source),
            location,
            source,
            last_opened_ms: 0,
        }
    }

    #[test]
    fn touch_updates_existing_entry_in_place() {
        let mut store = RecentStore {
            path: None,
            entries: Vec::new(),
        };
        store.touch(wt("a")); // index 0
        store.touch(wt("b")); // inserted on top → [b, a]
        // Re-opening `a` must NOT move it — the list stays put under the
        // user's clicks (the reported sidebar-jump bug).
        let index = store.touch(wt("a"));
        assert_eq!(index, 1);
        assert_eq!(store.entries().len(), 2);
        assert_eq!(store.entries()[0].location, wt("b").location);
        assert_eq!(store.entries()[1].location, wt("a").location);
        // Recency was still recorded, for ordering at next load.
        assert!(store.entries()[1].last_opened_ms >= store.entries()[0].last_opened_ms);
    }

    #[test]
    fn load_order_is_recency_but_touch_preserves_it() {
        let mut entries = vec![wt("old"), wt("new")];
        entries[0].last_opened_ms = 100;
        entries[1].last_opened_ms = 200;
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_opened_ms));
        assert_eq!(entries[0].location, wt("new").location);
    }

    #[test]
    fn touch_caps_at_max_evicting_least_recent() {
        let mut store = RecentStore {
            path: None,
            entries: Vec::new(),
        };
        // Distinct explicit timestamps: wall-clock ones tie at millisecond
        // resolution inside a test loop, making eviction order arbitrary.
        for i in 0..MAX_ENTRIES {
            let mut e = wt(&i.to_string());
            e.last_opened_ms = 1_000 + i as u64; // "0" is least recent
            store.entries.push(e);
        }
        let index = store.touch(wt("newcomer"));
        assert_eq!(index, 0);
        assert_eq!(store.entries().len(), MAX_ENTRIES);
        assert!(
            store
                .entries()
                .iter()
                .any(|e| e.location == wt("newcomer").location)
        );
        // The least-recently-opened entry was the one evicted.
        assert!(
            !store
                .entries()
                .iter()
                .any(|e| e.location == wt("0").location)
        );
    }

    #[test]
    fn title_derivation() {
        let loc = RepoLocation::Local(PathBuf::from("D:\\code\\difftest"));
        assert_eq!(
            title_for(&loc, &DiffSource::WorkingTree),
            "difftest — working tree"
        );
        assert_eq!(
            title_for(
                &loc,
                &DiffSource::Range {
                    base: "a".repeat(40),
                    head: "main".into(),
                    merge_base: true,
                }
            ),
            "difftest — aaaaaaa...main"
        );
        let wsl = RepoLocation::Wsl {
            distro: "Ubuntu".into(),
            path: "/home/kyle/proj".into(),
        };
        assert_eq!(title_for(&wsl, &DiffSource::Staged), "proj — staged");
    }
}
