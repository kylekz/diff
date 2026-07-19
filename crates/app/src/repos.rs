//! Persisted list of repositories the user has opened — the data behind
//! the sidebar's always-visible repo rows (the "open repos get added to
//! the sidebar, `+` on a row starts a review" flow). One JSON file in the
//! platform data dir (`<data_dir>/dv/repos.json`), best-effort on both
//! read and write (a corrupt or missing file yields an empty list; a
//! failed save is dropped silently — the list rebuilds itself from use).
//!
//! Unlike `recent.json` (a frozen pre-Phase-6 migration seed, see
//! [`crate::recent`]) this store is LIVE: every successful
//! `AppShell::open_review` records its location here, and the sidebar's
//! repo rows render from it — which is what makes a repo with zero
//! reviews still exist in the UI at all (the review index only learns
//! about a repo once it has a review). First launch with no `repos.json`
//! seeds it from every location the index/recent already knows, so an
//! upgrading user starts with the sidebar they already had.
//!
//! Entries keep insertion order (first-opened first) and are deduped by
//! [`same_location`] — case-insensitive with separators normalized for
//! local paths, matching how the sidebar's own `repo_group_key` folds
//! case, so a picker-opened `D:\code\X` and a CLI-opened `d:/code/x`
//! share one entry (and one "Remove from sidebar" actually removes it).
//! Callers pass absolutized locations (`AppShell::open_review` already
//! does), so `dv .` and `dv D:\x` can't create twins either.
//!
//! Concurrency: the file is read once at startup and each save rewrites
//! the whole list — two dv instances are last-writer-wins, with no lock
//! (unlike the review store). Accepted: the list is cheap, self-healing
//! (reopening a lost repo re-records it), and multi-instance dv is rare.

use std::path::PathBuf;

use dv_core::RepoLocation;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    repos: Vec<RepoLocation>,
}

pub struct RepoStore {
    entries: Vec<RepoLocation>,
    /// Whether a `repos.json` existed at load — the seed-once gate: an
    /// EXISTING-but-empty file is a user who removed every repo, not a
    /// fresh profile, and must not be re-seeded on next launch.
    existed: bool,
    /// `None` in tests (in-memory only) and when the platform data dir is
    /// unresolvable — every save silently no-ops then.
    path: Option<PathBuf>,
}

impl RepoStore {
    /// Load from the default location. Only a genuinely MISSING file
    /// reports `needs_seed()`: a file that exists but fails to parse
    /// (torn write, hand-edit gone wrong) loads as empty WITHOUT
    /// re-arming the seed — re-seeding would resurrect every repo the
    /// user ever removed, which is worse than an empty-but-rebuildable
    /// list (post-hoc review P2).
    pub fn load() -> Self {
        let path = default_path();
        let bytes = path.as_ref().and_then(|p| std::fs::read(p).ok());
        let existed = bytes.is_some();
        let entries = bytes
            .and_then(|bytes| serde_json::from_slice::<Persisted>(&bytes).ok())
            .map(|p| p.repos)
            .unwrap_or_default();
        Self {
            existed,
            entries,
            path,
        }
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        Self {
            entries: Vec::new(),
            existed: false,
            path: None,
        }
    }

    pub fn entries(&self) -> &[RepoLocation] {
        &self.entries
    }

    /// Whether this is a first load with no persisted file — the caller
    /// (`AppShell::new`) seeds from its already-known locations exactly
    /// once, then the file exists forever after.
    pub fn needs_seed(&self) -> bool {
        !self.existed
    }

    /// One-time migration fill (see [`Self::needs_seed`]); persists even
    /// when `locations` is empty so the seed never re-runs.
    pub fn seed(&mut self, locations: Vec<RepoLocation>) {
        for location in locations {
            if !self.contains(&location) {
                self.entries.push(location);
            }
        }
        self.existed = true;
        self.save();
    }

    /// Record an opened repo. Appends (and persists) only when new;
    /// returns whether the list changed.
    pub fn record(&mut self, location: &RepoLocation) -> bool {
        if self.contains(location) {
            return false;
        }
        self.entries.push(location.clone());
        self.save();
        true
    }

    /// Drop a repo row. Returns whether anything was removed.
    pub fn remove(&mut self, location: &RepoLocation) -> bool {
        let before = self.entries.len();
        self.entries.retain(|l| !same_location(l, location));
        let changed = self.entries.len() != before;
        if changed {
            self.save();
        }
        changed
    }

    fn contains(&self, location: &RepoLocation) -> bool {
        self.entries.iter().any(|l| same_location(l, location))
    }

    /// Temp-file + rename so a crash mid-save can't leave a torn
    /// `repos.json` (which `load` would read as an empty list — see its
    /// doc). Best-effort throughout; a failed rename leaves the old file
    /// intact.
    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let persisted = Persisted {
            repos: self.entries.clone(),
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&persisted) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// The store's dedup identity: local paths compare case-insensitively
/// with `/`/`\` folded together (the same normalization the sidebar's
/// `repo_group_key` applies, so one row ↔ one store entry); WSL locations
/// compare the distro ASCII-case-insensitively (matching `wsl -d`) with
/// the POSIX path exact.
fn same_location(a: &RepoLocation, b: &RepoLocation) -> bool {
    fn norm_local(path: &std::path::Path) -> String {
        path.to_string_lossy().replace('/', "\\").to_lowercase()
    }
    match (a, b) {
        (RepoLocation::Local(a), RepoLocation::Local(b)) => norm_local(a) == norm_local(b),
        (
            RepoLocation::Wsl {
                distro: da,
                path: pa,
            },
            RepoLocation::Wsl {
                distro: db,
                path: pb,
            },
        ) => da.eq_ignore_ascii_case(db) && pa == pb,
        _ => false,
    }
}

fn default_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("dv").join("repos.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(name: &str) -> RepoLocation {
        RepoLocation::Local(PathBuf::from(format!("D:\\code\\{name}")))
    }

    #[test]
    fn record_dedupes_and_keeps_insertion_order() {
        let mut store = RepoStore::in_memory();
        assert!(store.record(&loc("a")));
        assert!(store.record(&loc("b")));
        assert!(!store.record(&loc("a")));
        assert_eq!(store.entries(), &[loc("a"), loc("b")]);
    }

    #[test]
    fn record_and_remove_fold_case_and_separators_for_local_paths() {
        let mut store = RepoStore::in_memory();
        assert!(store.record(&RepoLocation::Local(PathBuf::from("D:\\code\\X"))));
        // The same repo spelled differently (CLI vs picker) is one entry…
        assert!(!store.record(&RepoLocation::Local(PathBuf::from("d:/code/x"))));
        assert_eq!(store.entries().len(), 1);
        // …and one remove kills it regardless of which spelling asks.
        assert!(store.remove(&RepoLocation::Local(PathBuf::from("d:\\CODE\\x"))));
        assert!(store.entries().is_empty());
    }

    #[test]
    fn wsl_locations_compare_distro_case_insensitively_but_path_exact() {
        let wsl = |distro: &str, path: &str| RepoLocation::Wsl {
            distro: distro.to_string(),
            path: path.to_string(),
        };
        let mut store = RepoStore::in_memory();
        assert!(store.record(&wsl("Ubuntu", "/home/k/proj")));
        assert!(!store.record(&wsl("ubuntu", "/home/k/proj")));
        // POSIX paths are case-sensitive for real — distinct entries.
        assert!(store.record(&wsl("Ubuntu", "/home/k/Proj")));
        assert_eq!(store.entries().len(), 2);
    }

    #[test]
    fn remove_only_reports_true_when_present() {
        let mut store = RepoStore::in_memory();
        store.record(&loc("a"));
        assert!(store.remove(&loc("a")));
        assert!(!store.remove(&loc("a")));
        assert!(store.entries().is_empty());
    }

    #[test]
    fn seed_marks_seeded_even_when_empty() {
        let mut store = RepoStore::in_memory();
        assert!(store.needs_seed());
        store.seed(Vec::new());
        assert!(!store.needs_seed());
    }

    #[test]
    fn persisted_round_trips_through_json() {
        let persisted = Persisted {
            repos: vec![loc("a"), loc("b")],
        };
        let json = serde_json::to_string(&persisted).unwrap();
        let back: Persisted = serde_json::from_str(&json).unwrap();
        assert_eq!(back.repos, persisted.repos);
    }
}
