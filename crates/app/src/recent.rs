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
        let entries = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<Persisted>(&bytes).ok())
            .map(|p| p.entries)
            .unwrap_or_default();
        Self { path, entries }
    }

    pub fn entries(&self) -> &[RecentEntry] {
        &self.entries
    }

    /// Move `entry` to the front (deduping an existing same-review entry so a
    /// re-open just bumps recency and refreshes the title), then persist.
    pub fn touch(&mut self, entry: RecentEntry) {
        self.entries.retain(|e| !e.same_review(&entry));
        self.entries.insert(0, entry);
        self.entries.truncate(MAX_ENTRIES);
        self.save();
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
        }
    }

    #[test]
    fn touch_dedups_and_moves_to_front() {
        let mut store = RecentStore {
            path: None,
            entries: Vec::new(),
        };
        store.touch(wt("a"));
        store.touch(wt("b"));
        store.touch(wt("a")); // re-open a
        assert_eq!(store.entries().len(), 2);
        assert_eq!(store.entries()[0].location, wt("a").location);
        assert_eq!(store.entries()[1].location, wt("b").location);
    }

    #[test]
    fn touch_caps_at_max() {
        let mut store = RecentStore {
            path: None,
            entries: Vec::new(),
        };
        for i in 0..(MAX_ENTRIES + 10) {
            store.touch(wt(&i.to_string()));
        }
        assert_eq!(store.entries().len(), MAX_ENTRIES);
        // Most-recent (highest index) is first.
        assert_eq!(
            store.entries()[0].location,
            wt(&(MAX_ENTRIES + 9).to_string()).location
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
