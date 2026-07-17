//! Global, cross-repo review index: a cache of every review dv knows about
//! across every repo it has ever opened, persisted at
//! `<data_dir>/dv/review_index.json`.
//!
//! **This is a cache, not a source of truth.** Canonical review data stays
//! exactly where it already lives — one JSON file per review under each
//! repo's `.git/dv/reviews/`, owned by [`crate::review::ReviewStore`] and
//! untouched by anything in this module. The index exists so the sidebar
//! can render every known review *instantly*, without a `.git/dv` scan of
//! every repo the app has ever seen — and because it's rebuildable from
//! those repos at any time (see [`hydrate_location`]), it takes a softer
//! load posture than [`crate::review::Review`]'s hard version refusal: a
//! newer-than-supported or corrupt index file simply loads empty rather
//! than erroring, and gets silently rewritten (correctly) on the next
//! save. Losing the index costs a re-hydration pass, never data.
//!
//! This module is pure data (types + hydration + title/label derivation);
//! it does not decide *when* to hydrate or how to honor the WSL
//! boot-storm policy (`crate::remote::manager::has_running_host`) — that
//! orchestration is the app shell's job (docs/phase-6-review-navigator.md).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::git::{DiffSource, DiffTotals, GitRepo};
use crate::github::{ChecksSummary, PrState, ReviewDecision};
use crate::location::RepoLocation;
use crate::review::{CommentStatus, RemoteRef, Review, ReviewState, ReviewStore, now_ms};

/// Schema version written by this build's index file. Unlike
/// [`crate::review::SCHEMA_VERSION`], a mismatch here is never fatal — see
/// the module doc.
pub const INDEX_SCHEMA_VERSION: u32 = 1;

/// One cached, sidebar-ready summary of a review living somewhere on disk.
/// Constructed from a live [`Review`] by [`IndexEntry::from_review`];
/// never hand-built by app code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// == [`Review::id`]; the index's identity key.
    pub review_id: String,
    pub location: RepoLocation,
    pub source: DiffSource,
    /// Line-2 sidebar label — see [`entry_title`].
    pub title: String,
    pub state: ReviewState,
    #[serde(default)]
    pub open_comments: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteRef>,
    /// Cached PR status, filled in by the app (network round trip); never
    /// set by [`IndexEntry::from_review`] itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_status: Option<CachedPrStatus>,
    /// Whole-review `+/−` totals for the sidebar card (R2). Filled by
    /// [`hydrate_location`] (a git numstat per review);
    /// never by [`IndexEntry::from_review`] — so, like `pr_status`, it's a
    /// cache field that [`ReviewIndex::upsert`] and
    /// [`ReviewIndex::apply_hydration`] carry forward when an incoming
    /// entry lacks it. `None` on a pre-R2 index file or when the numstat
    /// failed (e.g. a PR review whose head oid was pruned) — the card just
    /// omits the cluster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diffstat: Option<DiffTotals>,
    /// == [`Review::updated_ms`].
    pub updated_ms: u64,
    #[serde(default)]
    pub last_opened_ms: u64,
    #[serde(default)]
    pub health: EntryHealth,
    /// App-managed "tucked away" flag: an archived review stays fully
    /// intact in its repo's `.git/dv` store (nothing on disk changes) but
    /// the sidebar hides it unless the Archived filter is enabled. Only
    /// [`ReviewIndex::set_archived`] writes it — [`IndexEntry::from_review`]
    /// always produces `false`, so both [`ReviewIndex::upsert`] and
    /// [`ReviewIndex::apply_hydration`] carry the existing value forward
    /// unconditionally (an incoming `false` means "caller doesn't manage
    /// this field", not "unarchive").
    #[serde(default)]
    pub archived: bool,
}

/// Whether an entry's backing repo is currently reachable. Never causes an
/// entry to be dropped from the index by itself — a repo on an unplugged
/// drive or a stopped WSL distro is still a review worth remembering, just
/// one the sidebar should flag rather than silently hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryHealth {
    #[default]
    Ok,
    /// The last hydration attempt against this location failed (repo
    /// moved, drive unmounted, WSL distro unreachable).
    RepoUnavailable,
    /// Reserved for a future "review file itself is gone" state; not
    /// produced by anything in this slice.
    Missing,
}

/// Persistable mirror of [`crate::github::PrStatus`]. Kept as its own type
/// rather than reusing `PrStatus` directly: the live client type isn't
/// `Deserialize` (it only round-trips gh's JSON one way, in), and the
/// on-disk cache format shouldn't be coupled to however that type evolves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedPrStatus {
    pub state: PrState,
    pub is_draft: bool,
    #[serde(default)]
    pub decision: Option<ReviewDecision>,
    pub checks: ChecksSummary,
}

/// Result of a hydration attempt against one repo location — see
/// [`hydrate_location`] / [`ReviewIndex::apply_hydration`].
#[derive(Debug)]
pub enum HydrateOutcome {
    /// The store opened and listed successfully; this is the complete,
    /// current set of reviews at that location (may be empty).
    Reviews(Vec<IndexEntry>),
    /// The store couldn't be listed (repo moved, drive/distro
    /// unreachable). Existing entries for the location are flagged, never
    /// dropped, on this outcome.
    Unavailable,
}

/// The persisted cache: every known review across every repo, most
/// recently opened first (see [`ReviewIndex::load`]).
pub struct ReviewIndex {
    path: Option<PathBuf>,
    entries: Vec<IndexEntry>,
}

/// On-disk shape. A bare `Vec<IndexEntry>` isn't future-proof on its own —
/// wrapping it with a version lets a later format change be distinguished
/// from today's, per the softer cache posture in the module doc.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    v: u32,
    #[serde(default)]
    entries: Vec<IndexEntry>,
}

impl ReviewIndex {
    /// Load from `<data_dir>/dv/review_index.json`. Missing file,
    /// unparseable JSON, and a `v` newer than [`INDEX_SCHEMA_VERSION`] all
    /// yield an empty index rather than an error — see the module doc.
    /// Entries are sorted `last_opened_ms` descending once, here; while
    /// the app runs, [`Self::upsert`] updates in place rather than
    /// reordering (matches `RecentStore::touch`'s no-jumping-under-clicks
    /// rule).
    pub fn load() -> Self {
        let path = Self::default_path();
        let mut entries = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .map(|bytes| parse_persisted(&bytes))
            .unwrap_or_default();
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_opened_ms));
        Self { path, entries }
    }

    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    pub fn get(&self, review_id: &str) -> Option<&IndexEntry> {
        self.entries.iter().find(|e| e.review_id == review_id)
    }

    /// Insert or update `entry` by `review_id`, **in place** on update
    /// (never reorders an existing entry — same rationale as
    /// `RecentStore::touch`); a genuinely new entry is inserted at the
    /// front, also matching `RecentStore::touch`. `opened` distinguishes
    /// two callers: `true` means the user actually selected/opened this
    /// review just now, so recency is stamped to `now_ms()`; `false`
    /// means this is a metadata refresh (e.g. a live `ReviewChanged`
    /// event updating comment counts) that must not disturb the review's
    /// place in a recency-ordered list, so the existing entry's
    /// `last_opened_ms` is carried forward. Returns the entry's index in
    /// [`Self::entries`] and persists.
    pub fn upsert(&mut self, mut entry: IndexEntry, opened: bool) -> usize {
        if opened {
            entry.last_opened_ms = now_ms();
        }
        let index = match self
            .entries
            .iter()
            .position(|e| e.review_id == entry.review_id)
        {
            Some(index) => {
                if !opened {
                    entry.last_opened_ms = self.entries[index].last_opened_ms;
                }
                // A cache-field carry: most upsert callers build their
                // entry via `from_review` (which never computes a
                // diffstat), so an incoming `None` means "unknown", not
                // "zero" — keep the total the last hydration pass
                // computed. (`pr_status` is deliberately NOT carried here:
                // its callers each decide explicitly what status to stamp,
                // see shell.rs's badge-merge logic.)
                if entry.diffstat.is_none() {
                    entry.diffstat = self.entries[index].diffstat;
                }
                // App-managed flag — no upsert caller builds entries with a
                // meaningful `archived`, so the existing value always wins
                // (see the field's doc comment; `set_archived` is the only
                // writer).
                entry.archived = self.entries[index].archived;
                self.entries[index] = entry;
                index
            }
            None => {
                // A brand-new entry — insert at the front, matching
                // `RecentStore::touch`'s "freshly-opened item lands on
                // top" behavior, rather than appending at the tail where
                // it would sit *below* older, less-recent entries despite
                // (when `opened`) carrying the largest `last_opened_ms` in
                // the list.
                self.entries.insert(0, entry);
                0
            }
        };
        self.save();
        index
    }

    pub fn remove(&mut self, review_id: &str) {
        self.entries.retain(|e| e.review_id != review_id);
        self.save();
    }

    /// Set the app-managed archived flag (see [`IndexEntry::archived`]) and
    /// persist. Returns `false` if no entry has that id. In place — never
    /// reorders.
    pub fn set_archived(&mut self, review_id: &str, archived: bool) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|e| e.review_id == review_id) else {
            return false;
        };
        entry.archived = archived;
        self.save();
        true
    }

    /// Fold a [`hydrate_location`] result into the index. `Reviews`
    /// replaces the *entire* current entry set for `location` with the
    /// fresh one — additions and deletions on disk both take effect —
    /// while carrying each surviving review's `last_opened_ms` and
    /// app-filled `pr_status` forward by `review_id` (`from_review` always
    /// produces `pr_status: None`, so without this the network-filled PR
    /// badge would be wiped on every re-hydration pass) so re-hydrating
    /// doesn't reset recency or drop the cached PR badge.
    ///
    /// **Order-preserving**: each surviving entry is replaced *in place*,
    /// never moved — the no-jumping-under-clicks rule ([`Self::upsert`]'s
    /// doc comment; recency ordering applies once, at [`Self::load`]).
    /// This used to re-sort the whole list `last_opened_ms` descending
    /// here, which defeated that rule live: selecting a review stamps its
    /// recency (upsert, in place — no visible move) and then triggers a
    /// location hydration, whose re-sort bubbled the freshly-clicked
    /// review — and, under repo grouping, its whole first-encounter group —
    /// to the top of the sidebar on every click. Only genuinely-new
    /// reviews move: they're inserted at the front (matching `upsert`'s
    /// new-entry rule), in their store order.
    /// `Unavailable` never drops anything; it only flags existing entries
    /// for the location [`EntryHealth::RepoUnavailable`] (see
    /// cross-cutting risk A in docs/phase-6-review-navigator.md's slice
    /// plan: the index must never treat absence-from-a-failed-hydration as
    /// absence-from-disk).
    pub fn apply_hydration(&mut self, location: &RepoLocation, outcome: HydrateOutcome) {
        match outcome {
            HydrateOutcome::Reviews(fresh) => {
                // Carry recency/`pr_status` forward from each fresh
                // review's existing entry, wherever it currently lives.
                // Normally that's under `location` itself, but a stale
                // duplicate can be parked under a *different*,
                // textually-unequal `RepoLocation` key for the same repo
                // (review finding P2-1 — e.g. a mixed-case CLI path
                // argument vs. git's canonically-cased toplevel, which
                // `RepoLocation`'s `Path`-derived `Eq` treats as distinct).
                // Matching on `review_id` across *all* entries here, not
                // just this location's, means such a leftover gets folded
                // in rather than surviving as a second, duplicate row for
                // the same review.
                let fresh_order: Vec<String> = fresh.iter().map(|e| e.review_id.clone()).collect();
                let mut fresh_by_id: HashMap<String, IndexEntry> = fresh
                    .into_iter()
                    .map(|e| (e.review_id.clone(), e))
                    .collect();
                // Walk the current list in order: replace each surviving
                // entry in place (carrying its app-filled fields), drop
                // this location's vanished ones, drop any *later* duplicate
                // of an id already folded in (the P2-1 leftover — its fresh
                // entry was consumed by the first occurrence), and leave
                // every other location's entries untouched where they sit.
                let mut kept: Vec<IndexEntry> = Vec::with_capacity(self.entries.len());
                for existing in self.entries.drain(..) {
                    match fresh_by_id.remove(existing.review_id.as_str()) {
                        Some(mut entry) => {
                            entry.last_opened_ms = existing.last_opened_ms;
                            if entry.pr_status.is_none() {
                                entry.pr_status = existing.pr_status;
                            }
                            if entry.diffstat.is_none() {
                                entry.diffstat = existing.diffstat;
                            }
                            entry.archived = existing.archived;
                            entry.health = EntryHealth::Ok;
                            kept.push(entry);
                        }
                        None => {
                            let was_fresh_duplicate =
                                fresh_order.iter().any(|id| id == &existing.review_id);
                            if &existing.location != location && !was_fresh_duplicate {
                                kept.push(existing);
                            }
                        }
                    }
                }
                self.entries = kept;
                // Genuinely-new reviews (not in the index anywhere) land at
                // the front in their store order — `upsert`'s new-entry
                // rule.
                let new_entries: Vec<IndexEntry> = fresh_order
                    .iter()
                    .filter_map(|id| {
                        let mut entry = fresh_by_id.remove(id)?;
                        entry.health = EntryHealth::Ok;
                        Some(entry)
                    })
                    .collect();
                self.entries.splice(0..0, new_entries);
            }
            HydrateOutcome::Unavailable => {
                for entry in self.entries.iter_mut().filter(|e| &e.location == location) {
                    entry.health = EntryHealth::RepoUnavailable;
                }
            }
        }
        self.save();
    }

    /// Best-effort, atomic-ish write (temp sibling + rename), same
    /// pattern as `RecentStore::save`/`Settings::save` — the index is a
    /// convenience cache, not worth surfacing a write-failure error over.
    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let persisted = Persisted {
            v: INDEX_SCHEMA_VERSION,
            entries: self.entries.clone(),
        };
        if let Ok(bytes) = serde_json::to_vec_pretty(&persisted) {
            // Pid-suffixed temp so two dv processes saving concurrently
            // never interleave into one shared temp file (each renames a
            // complete file; last rename wins).
            let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    /// `<data_dir>/dv/review_index.json`. `None` only if no platform data
    /// dir can be determined, in which case the index works in-memory
    /// (same posture as `RecentStore`/`Settings`).
    pub fn default_path() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("dv").join("review_index.json"))
    }
}

/// Pure parse step behind [`ReviewIndex::load`], factored out so the
/// version/corruption handling is unit-testable without touching the real
/// data dir (mirrors how `review::check_schema_version` is tested
/// directly rather than only through `ReviewStore`).
fn parse_persisted(bytes: &[u8]) -> Vec<IndexEntry> {
    serde_json::from_slice::<Persisted>(bytes)
        .ok()
        .filter(|p| p.v <= INDEX_SCHEMA_VERSION)
        .map(|p| p.entries)
        .unwrap_or_default()
}

impl IndexEntry {
    /// The only constructor used by hydration — keeps `IndexEntry` and
    /// `Review` from drifting apart (title/state/comment-count derivation
    /// lives in exactly one place). `pr_status` always starts `None`: it's
    /// filled in later by the app from its own network fetch, which this
    /// headless crate has no business doing.
    pub fn from_review(location: &RepoLocation, review: &Review) -> Self {
        let open_comments = review
            .comments
            .iter()
            .filter(|c| c.status == CommentStatus::Open)
            .count();
        Self {
            review_id: review.id.clone(),
            location: location.clone(),
            source: review.source.clone(),
            title: entry_title(location, &review.source, review.remote.as_ref()),
            state: review.state.clone(),
            open_comments,
            remote: review.remote.clone(),
            pr_status: None,
            diffstat: None,
            updated_ms: review.updated_ms,
            last_opened_ms: 0,
            health: EntryHealth::Ok,
            archived: false,
        }
    }
}

/// List every review at `location` fresh off disk and map each to an
/// [`IndexEntry`]. **Blocking** — this shells out via
/// [`ReviewStore::list`] (and, for a WSL location, that routes through the
/// host process / `wsl.exe`), so callers on a UI thread must run it on a
/// background executor. Does **not** itself decide whether it's safe to
/// hydrate a WSL location right now (e.g. whether that would boot a
/// stopped distro) — that liveness gate belongs to the caller (app shell),
/// per cross-cutting risk B in docs/phase-6-review-navigator.md's slice
/// plan.
pub fn hydrate_location(location: &RepoLocation) -> HydrateOutcome {
    let store = ReviewStore::open(location.clone());
    match store.list() {
        Ok(reviews) => {
            // One repo handle for the whole pass; a per-review numstat
            // fills the card diffstat. Both are best-effort — a failed
            // open/numstat leaves `diffstat: None`, and
            // `apply_hydration`'s carry-forward keeps whatever total a
            // previous pass cached rather than blanking the card.
            let repo = GitRepo::open(location.clone()).ok();
            HydrateOutcome::Reviews(
                reviews
                    .iter()
                    .map(|r| {
                        let mut entry = IndexEntry::from_review(location, r);
                        entry.diffstat =
                            repo.as_ref().and_then(|repo| repo.diffstat(&r.source).ok());
                        entry
                    })
                    .collect(),
            )
        }
        Err(_) => HydrateOutcome::Unavailable,
    }
}

/// Line-2 sidebar label for a review: a PR-linked review shows `PR #<n>`
/// (the review's derived title until a real fetched PR title is cached —
/// see docs/phase-6-review-navigator.md deliverable 2's "renamable
/// later"); a local review falls back to a description of its diff
/// source, the same wording `recent.rs::title_for` used pre-Phase-6, minus
/// the repo prefix (that's now [`repo_label`]'s job on line 1, so this
/// function no longer needs `location` to build a repo name — the
/// parameter stays for signature symmetry with `repo_label` and as a hook
/// for a future per-repo title tweak).
pub fn entry_title(
    _location: &RepoLocation,
    source: &DiffSource,
    remote: Option<&RemoteRef>,
) -> String {
    if let Some(remote) = remote {
        return format!("PR #{}", remote.pr);
    }
    match source {
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
    }
}

/// Line-1 sidebar label: `owner/repo#<pr>` for a PR-linked review (user
/// refinement 2026-07-12 — no host, no "github.com/" clutter), else the
/// repo's folder name.
pub fn repo_label(location: &RepoLocation, remote: Option<&RemoteRef>) -> String {
    if let Some(remote) = remote {
        // `remote.slug` is `host/owner/repo` (matches `RepoSlug`'s
        // `Display`); drop the host for the sidebar label.
        let owner_repo = match remote.slug.rsplit_once('/') {
            Some((rest, repo)) => {
                let owner = rest.rsplit('/').next().unwrap_or(rest);
                format!("{owner}/{repo}")
            }
            None => remote.slug.clone(),
        };
        return format!("{owner_repo}#{}", remote.pr);
    }
    repo_short_name(location)
}

/// The last path component of a repo location, for a compact label.
/// Ported from `recent.rs::repo_short_name` (crates/app can't be a
/// dependency of dv-core, so this can't simply be shared — see
/// docs/phase-6-review-navigator.md's S6a/S6b split).
fn repo_short_name(location: &RepoLocation) -> String {
    let full = location.display_name();
    full.rsplit(['/', '\\', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or(&full)
        .to_string()
}

fn short_rev(rev: &str) -> String {
    if rev.len() >= 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        rev[..7].to_string()
    } else {
        rev.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::Verdict;

    fn local(name: &str) -> RepoLocation {
        RepoLocation::Local(PathBuf::from(format!("D:\\code\\{name}")))
    }

    fn wsl(name: &str) -> RepoLocation {
        RepoLocation::Wsl {
            distro: "Ubuntu".to_string(),
            path: format!("/home/kyle/{name}"),
        }
    }

    fn remote_ref(pr: u64) -> RemoteRef {
        RemoteRef {
            provider: "github".to_string(),
            slug: "github.com/kylekz/difftest".to_string(),
            pr,
            url: format!("https://github.com/kylekz/difftest/pull/{pr}"),
            submitted_review_id: None,
            submitted_url: None,
        }
    }

    fn sample_entry(id: &str) -> IndexEntry {
        IndexEntry {
            review_id: id.to_string(),
            location: local("difftest"),
            source: DiffSource::WorkingTree,
            title: "working tree".to_string(),
            state: ReviewState::Draft,
            open_comments: 2,
            remote: Some(remote_ref(7)),
            pr_status: Some(CachedPrStatus {
                state: PrState::Open,
                is_draft: false,
                decision: Some(ReviewDecision::Approved),
                checks: ChecksSummary::Passing,
            }),
            diffstat: Some(DiffTotals {
                additions: 12,
                deletions: 3,
            }),
            archived: false,
            updated_ms: 1_700_000_000_000,
            last_opened_ms: 1_700_000_000_500,
            health: EntryHealth::Ok,
        }
    }

    // --- round trips / forward compat -----------------------------------

    #[test]
    fn round_trips_through_json() {
        let entry = sample_entry("r-1");
        let json = serde_json::to_string_pretty(&entry).unwrap();
        let round_tripped: IndexEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, entry);

        let persisted = Persisted {
            v: INDEX_SCHEMA_VERSION,
            entries: vec![entry.clone()],
        };
        let json = serde_json::to_string(&persisted).unwrap();
        let round_tripped: Persisted = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.v, INDEX_SCHEMA_VERSION);
        assert_eq!(round_tripped.entries, vec![entry]);
    }

    #[test]
    fn old_index_json_missing_new_fields_loads() {
        // Simulate a file written by an earlier build that only knew
        // about review_id/location/source/title/state/updated_ms — strip
        // every field this slice added defaults for.
        let full = sample_entry("r-2");
        let mut value = serde_json::to_value(&full).unwrap();
        let obj = value.as_object_mut().unwrap();
        obj.remove("open_comments");
        obj.remove("remote");
        obj.remove("pr_status");
        obj.remove("diffstat");
        obj.remove("last_opened_ms");
        obj.remove("health");

        let parsed: IndexEntry = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.review_id, "r-2");
        assert_eq!(parsed.open_comments, 0);
        assert!(parsed.remote.is_none());
        assert!(parsed.pr_status.is_none());
        assert!(parsed.diffstat.is_none());
        assert_eq!(parsed.last_opened_ms, 0);
        assert_eq!(parsed.health, EntryHealth::Ok);
    }

    #[test]
    fn newer_version_or_corrupt_loads_empty() {
        assert!(parse_persisted(b"not json at all").is_empty());
        assert!(parse_persisted(b"").is_empty());

        let entry = sample_entry("r-1");
        let future = serde_json::json!({
            "v": 99,
            "entries": [entry],
        });
        assert!(
            parse_persisted(future.to_string().as_bytes()).is_empty(),
            "a v newer than this build supports must load empty, not partially"
        );
    }

    #[test]
    fn current_version_parses_its_entries() {
        let entry = sample_entry("r-1");
        let current = serde_json::json!({
            "v": INDEX_SCHEMA_VERSION,
            "entries": [entry],
        });
        let parsed = parse_persisted(current.to_string().as_bytes());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].review_id, "r-1");
    }

    // --- upsert ------------------------------------------------------------

    #[test]
    fn upsert_dedups_by_review_id_and_bumps_last_opened() {
        let mut index = ReviewIndex {
            path: None,
            entries: Vec::new(),
        };
        let mut e1 = sample_entry("r-1");
        e1.last_opened_ms = 0;
        let idx0 = index.upsert(e1.clone(), false);
        assert_eq!(idx0, 0);
        assert_eq!(index.entries().len(), 1);
        // opened=false on first insert: whatever the caller passed stands.
        assert_eq!(index.entries()[0].last_opened_ms, 0);

        let mut e1_refresh = e1.clone();
        e1_refresh.open_comments = 5;
        let idx1 = index.upsert(e1_refresh, true);
        assert_eq!(idx1, 0, "update must stay in place, not move to the end");
        assert_eq!(index.entries().len(), 1, "must dedup by review_id");
        assert_eq!(index.entries()[0].open_comments, 5);
        assert!(
            index.entries()[0].last_opened_ms > 0,
            "opened=true must bump last_opened_ms"
        );

        // A second review lands as a new entry, at the front — matching
        // `RecentStore::touch` (a brand-new entry must not sink below
        // older ones, notably when `opened` stamps it with the largest
        // `last_opened_ms` in the list).
        let e2 = sample_entry("r-2");
        let idx2 = index.upsert(e2, false);
        assert_eq!(idx2, 0);
        assert_eq!(index.entries().len(), 2);
        assert_eq!(index.entries()[0].review_id, "r-2");
        assert_eq!(index.entries()[1].review_id, "r-1");
    }

    #[test]
    fn upsert_opened_false_preserves_existing_recency() {
        let mut index = ReviewIndex {
            path: None,
            entries: Vec::new(),
        };
        let mut e1 = sample_entry("r-1");
        e1.last_opened_ms = 12_345;
        index.upsert(e1.clone(), false);

        // A metadata-only refresh (opened=false) must not reset recency
        // back to whatever the caller's fresh IndexEntry happened to
        // carry (e.g. 0 from `from_review`), or a live `ReviewChanged`
        // update would silently reorder the sidebar on next load.
        let mut refreshed = e1.clone();
        refreshed.last_opened_ms = 0;
        refreshed.open_comments = 9;
        index.upsert(refreshed, false);
        assert_eq!(index.entries()[0].last_opened_ms, 12_345);
        assert_eq!(index.entries()[0].open_comments, 9);
    }

    // --- apply_hydration -----------------------------------------------

    #[test]
    fn apply_hydration_replaces_location_set_preserving_last_opened_and_dropping_deleted() {
        let mut index = ReviewIndex {
            path: None,
            entries: Vec::new(),
        };
        let loc = local("difftest");
        let mut kept = sample_entry("r-kept");
        kept.location = loc.clone();
        kept.last_opened_ms = 999;
        let mut deleted = sample_entry("r-deleted");
        deleted.location = loc.clone();
        deleted.last_opened_ms = 111;
        let mut other_repo = sample_entry("r-other-repo");
        other_repo.location = local("unrelated");
        index.entries = vec![kept.clone(), deleted, other_repo.clone()];

        let mut fresh_kept = kept.clone();
        fresh_kept.last_opened_ms = 0; // hydration never knows recency
        fresh_kept.open_comments = 42; // but does carry fresh metadata
        let mut new_entry = sample_entry("r-new");
        new_entry.location = loc.clone();
        new_entry.last_opened_ms = 0;

        index.apply_hydration(&loc, HydrateOutcome::Reviews(vec![fresh_kept, new_entry]));

        assert_eq!(
            index.entries().len(),
            3,
            "kept + new + other-repo untouched"
        );
        let kept_after = index.get("r-kept").unwrap();
        assert_eq!(
            kept_after.last_opened_ms, 999,
            "surviving review's last_opened_ms must carry forward"
        );
        assert_eq!(kept_after.open_comments, 42);
        assert_eq!(kept_after.health, EntryHealth::Ok);
        assert!(
            index.get("r-deleted").is_none(),
            "vanished review is dropped"
        );
        assert!(index.get("r-new").is_some(), "new-on-disk review is added");
        assert!(
            index.get("r-other-repo").is_some(),
            "other locations must be untouched"
        );
    }

    #[test]
    fn apply_hydration_preserves_current_order_even_against_recency() {
        // The no-jumping-under-clicks rule (Kyle's long-standing sidebar
        // bug): selecting a review stamps its recency in place and then
        // triggers a location hydration — that hydration must NOT re-sort
        // the list, or the freshly-clicked review (largest
        // `last_opened_ms`) visibly jumps to the top on every click.
        // Recency ordering applies once, at `load()`.
        let loc1 = local("difftest");
        let loc2 = local("other");
        let mut a = sample_entry("r-a");
        a.location = loc1.clone();
        a.last_opened_ms = 300;
        let mut b = sample_entry("r-b");
        b.location = loc2.clone();
        b.last_opened_ms = 200;
        let mut c = sample_entry("r-c");
        c.location = loc1.clone();
        c.last_opened_ms = 100;
        let mut index = ReviewIndex {
            path: None,
            entries: vec![a.clone(), b.clone(), c.clone()],
        };

        // The user clicks r-c: upsert stamps its recency in place (now the
        // largest in the list, but still in third position)...
        index.upsert(c.clone(), true);
        assert_eq!(index.entries()[2].review_id, "r-c");
        assert!(index.entries()[2].last_opened_ms > 300);

        // ...and the follow-up hydration of its location must leave every
        // survivor exactly where it was.
        let mut fresh_a = a.clone();
        fresh_a.last_opened_ms = 0;
        let mut fresh_c = c.clone();
        fresh_c.last_opened_ms = 0;
        index.apply_hydration(&loc1, HydrateOutcome::Reviews(vec![fresh_c, fresh_a]));

        let ids: Vec<&str> = index
            .entries()
            .iter()
            .map(|e| e.review_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["r-a", "r-b", "r-c"],
            "hydration must never reorder surviving entries, whatever their recency"
        );
        assert!(
            index.get("r-c").unwrap().last_opened_ms > 300,
            "the click's recency stamp still survives the hydration (for the next load)"
        );
    }

    #[test]
    fn apply_hydration_inserts_genuinely_new_reviews_at_the_front() {
        // Matches `upsert`'s new-entry rule: a review that appeared on
        // disk since the last pass (created via CLI, another window…)
        // lands at the top of the sidebar, in store order, without
        // disturbing anything below.
        let loc = local("difftest");
        let mut old = sample_entry("r-old");
        old.location = loc.clone();
        let mut other = sample_entry("r-other-repo");
        other.location = local("unrelated");
        let mut index = ReviewIndex {
            path: None,
            entries: vec![old.clone(), other.clone()],
        };

        let mut new1 = sample_entry("r-new1");
        new1.location = loc.clone();
        let mut new2 = sample_entry("r-new2");
        new2.location = loc.clone();
        index.apply_hydration(&loc, HydrateOutcome::Reviews(vec![new1, old.clone(), new2]));

        let ids: Vec<&str> = index
            .entries()
            .iter()
            .map(|e| e.review_id.as_str())
            .collect();
        assert_eq!(ids, vec!["r-new1", "r-new2", "r-old", "r-other-repo"]);
    }

    #[test]
    fn archived_flag_survives_upsert_and_hydration_and_only_set_archived_writes_it() {
        let loc = local("difftest");
        let mut entry = sample_entry("r-1");
        entry.location = loc.clone();
        let mut index = ReviewIndex {
            path: None,
            entries: vec![entry.clone()],
        };

        assert!(index.set_archived("r-1", true));
        assert!(index.entries()[0].archived);
        assert!(!index.set_archived("r-missing", true));

        // A metadata refresh (`from_review` never sets archived)...
        let mut refresh = entry.clone();
        refresh.archived = false;
        refresh.open_comments = 7;
        index.upsert(refresh, false);
        assert!(
            index.entries()[0].archived,
            "upsert must carry the app-managed flag forward"
        );

        // ...and a full location re-hydration both leave it intact.
        let mut fresh = entry.clone();
        fresh.archived = false;
        index.apply_hydration(&loc, HydrateOutcome::Reviews(vec![fresh]));
        assert!(
            index.entries()[0].archived,
            "hydration must carry the app-managed flag forward"
        );

        assert!(index.set_archived("r-1", false));
        assert!(!index.entries()[0].archived);
    }

    #[test]
    fn upsert_carries_diffstat_forward_when_incoming_lacks_it() {
        // Shell upsert callers build entries via `from_review`, which
        // never computes a diffstat — a live metadata refresh must not
        // blank the total the last hydration pass cached.
        let mut index = ReviewIndex {
            path: None,
            entries: Vec::new(),
        };
        let hydrated = sample_entry("r-1");
        let cached = hydrated.diffstat;
        assert!(cached.is_some());
        index.upsert(hydrated, false);

        let mut refresh = sample_entry("r-1");
        refresh.diffstat = None; // as `from_review` would produce
        refresh.open_comments = 9;
        index.upsert(refresh, false);
        assert_eq!(index.entries()[0].diffstat, cached);
        assert_eq!(index.entries()[0].open_comments, 9);

        // But a *known* fresh total wins — carry-forward only fills the
        // unknown case, it never pins the first value forever.
        let mut newer = sample_entry("r-1");
        newer.diffstat = Some(DiffTotals {
            additions: 100,
            deletions: 50,
        });
        index.upsert(newer.clone(), false);
        assert_eq!(index.entries()[0].diffstat, newer.diffstat);
    }

    #[test]
    fn apply_hydration_carries_diffstat_forward_when_fresh_lacks_it() {
        // A hydration pass where the numstat failed (repo open error, a
        // pruned PR head oid) produces `diffstat: None`; the previously
        // cached total must survive, same as `pr_status`.
        let loc = local("difftest");
        let mut existing = sample_entry("r-1");
        existing.location = loc.clone();
        let cached = existing.diffstat;
        assert!(cached.is_some());
        let mut index = ReviewIndex {
            path: None,
            entries: vec![existing],
        };

        let mut fresh = sample_entry("r-1");
        fresh.location = loc.clone();
        fresh.diffstat = None;
        fresh.last_opened_ms = 0;
        index.apply_hydration(&loc, HydrateOutcome::Reviews(vec![fresh]));
        assert_eq!(index.get("r-1").unwrap().diffstat, cached);
    }

    #[test]
    fn apply_hydration_carries_pr_status_forward_when_fresh_lacks_it() {
        // `from_review` always produces `pr_status: None` (dv-core can't
        // do the network fetch); a real hydration pass must not wipe out
        // a PR status the app previously filled in and cached.
        let loc = local("difftest");
        let mut existing = sample_entry("r-1");
        existing.location = loc.clone();
        let cached_status = existing.pr_status.clone();
        assert!(cached_status.is_some());
        let mut index = ReviewIndex {
            path: None,
            entries: vec![existing],
        };

        let mut fresh = sample_entry("r-1");
        fresh.location = loc.clone();
        fresh.pr_status = None; // as `from_review` would produce
        fresh.last_opened_ms = 0;

        index.apply_hydration(&loc, HydrateOutcome::Reviews(vec![fresh]));

        assert_eq!(
            index.get("r-1").unwrap().pr_status,
            cached_status,
            "cached pr_status must survive a re-hydration pass"
        );
    }

    #[test]
    fn apply_hydration_drops_a_stale_duplicate_under_a_different_location_key() {
        // Regression for review finding P2-1: a mixed-case CLI path
        // argument (or any other textually-unequal-but-same-repo
        // `RepoLocation`) can leave an entry for the same `review_id`
        // parked under a *different* location key than the one this
        // hydration pass is keyed by. The fold must recognize `review_id`
        // as the true identity and end up with exactly one entry, not two.
        let proper = local("difftest"); // e.g. git's canonically-cased toplevel
        let stale = RepoLocation::Local(PathBuf::from("D:\\code\\DIFFTEST"));
        // Sanity: these two locations really are distinct under `Eq` —
        // otherwise this test wouldn't be exercising the bug at all.
        assert_ne!(proper, stale);

        let mut duplicate = sample_entry("r-dup");
        duplicate.location = stale.clone();
        duplicate.last_opened_ms = 555;
        let mut index = ReviewIndex {
            path: None,
            entries: vec![duplicate],
        };

        let mut fresh = sample_entry("r-dup");
        fresh.location = proper.clone();
        fresh.pr_status = None; // as `from_review` would produce
        fresh.last_opened_ms = 0;

        index.apply_hydration(&proper, HydrateOutcome::Reviews(vec![fresh]));

        let matches: Vec<&IndexEntry> = index
            .entries()
            .iter()
            .filter(|e| e.review_id == "r-dup")
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "the same review_id must never appear under two locations"
        );
        assert_eq!(matches[0].location, proper);
        assert_eq!(
            matches[0].last_opened_ms, 555,
            "recency from the stale-location duplicate must still carry forward"
        );
    }

    #[test]
    fn apply_hydration_unavailable_marks_not_drops() {
        let mut index = ReviewIndex {
            path: None,
            entries: Vec::new(),
        };
        let loc = wsl("proj");
        let mut e1 = sample_entry("r-1");
        e1.location = loc.clone();
        let mut other = sample_entry("r-2");
        other.location = local("elsewhere");
        index.entries = vec![e1, other];

        index.apply_hydration(&loc, HydrateOutcome::Unavailable);

        assert_eq!(
            index.entries().len(),
            2,
            "nothing is dropped on Unavailable"
        );
        assert_eq!(
            index.get("r-1").unwrap().health,
            EntryHealth::RepoUnavailable
        );
        assert_eq!(
            index.get("r-2").unwrap().health,
            EntryHealth::Ok,
            "unrelated locations must not be flagged"
        );
    }

    // --- entry_title / repo_label ----------------------------------------

    #[test]
    fn entry_title_and_repo_label_for_local_working_tree() {
        let loc = local("difftest");
        assert_eq!(
            entry_title(&loc, &DiffSource::WorkingTree, None),
            "working tree"
        );
        assert_eq!(repo_label(&loc, None), "difftest");
    }

    #[test]
    fn entry_title_and_repo_label_for_local_range() {
        let loc = local("difftest");
        let source = DiffSource::Range {
            base: "a".repeat(40),
            head: "main".to_string(),
            merge_base: true,
        };
        assert_eq!(entry_title(&loc, &source, None), "aaaaaaa...main");
        assert_eq!(repo_label(&loc, None), "difftest");
    }

    #[test]
    fn entry_title_and_repo_label_for_wsl() {
        let loc = wsl("proj");
        assert_eq!(entry_title(&loc, &DiffSource::Staged, None), "staged");
        assert_eq!(repo_label(&loc, None), "proj");
    }

    #[test]
    fn entry_title_and_repo_label_for_pr_linked() {
        let loc = local("difftest");
        let remote = remote_ref(12345);
        let source = DiffSource::Range {
            base: "main".to_string(),
            head: "b".repeat(40),
            merge_base: true,
        };
        assert_eq!(entry_title(&loc, &source, Some(&remote)), "PR #12345");
        assert_eq!(repo_label(&loc, Some(&remote)), "kylekz/difftest#12345");
    }

    // --- from_review / hydrate_location -----------------------------------

    #[test]
    fn from_review_derives_open_comment_count_and_title() {
        let mut review = Review::new_draft(DiffSource::WorkingTree);
        review
            .add_comment("a.txt", crate::review::Side::New, 1, 1, None, "one", "kyle")
            .unwrap();
        let id2 = review
            .add_comment("b.txt", crate::review::Side::New, 2, 2, None, "two", "kyle")
            .unwrap()
            .id
            .clone();
        review.set_status(&id2, CommentStatus::Resolved).unwrap();

        let loc = local("difftest");
        let entry = IndexEntry::from_review(&loc, &review);
        assert_eq!(entry.review_id, review.id);
        assert_eq!(entry.open_comments, 1, "only the unresolved comment counts");
        assert_eq!(entry.title, "working tree");
        assert_eq!(entry.state, ReviewState::Draft);
        assert_eq!(entry.last_opened_ms, 0);
        assert!(entry.pr_status.is_none());
    }

    #[test]
    fn from_review_submitted_state_round_trips() {
        let mut review = Review::new_draft(DiffSource::WorkingTree);
        review.set_state(ReviewState::Submitted {
            verdict: Verdict::Approve,
            at_ms: 1,
        });
        let entry = IndexEntry::from_review(&local("difftest"), &review);
        assert!(matches!(entry.state, ReviewState::Submitted { .. }));
    }

    #[test]
    fn hydrate_location_reports_unavailable_for_a_nonexistent_repo() {
        // No `.git` at this path — `ReviewStore::list` must fail cleanly,
        // and hydration must turn that into `Unavailable`, not panic.
        let loc = RepoLocation::Local(PathBuf::from(
            "D:\\this\\path\\definitely\\does\\not\\exist\\dv-index-test",
        ));
        let outcome = hydrate_location(&loc);
        assert!(matches!(outcome, HydrateOutcome::Unavailable));
    }

    #[test]
    fn repo_location_wsl_round_trips_through_json() {
        let loc = wsl("proj");
        let json = serde_json::to_string(&loc).unwrap();
        let round_tripped: RepoLocation = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, loc);
    }
}
