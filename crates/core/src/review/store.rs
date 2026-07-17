//! [`ReviewStore`]: the per-repo collection of [`Review`]s persisted under
//! `.git/dv/reviews/*.json`. This is the only piece of the review layer
//! meant to be used from outside [`crate::review`] — [`super::io`] is
//! deliberately private.

use anyhow::{Context, Result};

use super::io::StoreIo;
use super::{Review, check_schema_version};
use crate::git::DiffSource;
use crate::location::RepoLocation;

const REVIEWS_DIR: &str = "dv/reviews";

pub struct ReviewStore {
    io: StoreIo,
    location: RepoLocation,
}

impl ReviewStore {
    /// Cheap: no I/O happens until a method below is called.
    pub fn open(location: RepoLocation) -> Self {
        Self {
            io: StoreIo::new(location.clone()),
            location,
        }
    }

    /// Watch the store for external changes (agent CLI, another window);
    /// `on_change` fires from a background thread — hand off to a channel.
    /// Dropping the returned watcher stops it. See [`super::watch`].
    pub fn watch(
        &self,
        on_change: Box<dyn Fn() + Send + Sync>,
    ) -> Result<super::watch::ReviewWatcher> {
        super::watch::watch(self.location.clone(), on_change)
    }

    /// Every review in the store, newest (`created_ms`) first.
    pub fn list(&self) -> Result<Vec<Review>> {
        let names = self.io.list(REVIEWS_DIR)?;
        let mut reviews = Vec::with_capacity(names.len());
        for name in names {
            // Anything not a `<id>.json` file (a stray `.tmp-*` left by an
            // interrupted write, say) is silently skipped rather than
            // failing the whole listing.
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            if let Some(review) = self.load(id)? {
                reviews.push(review);
            }
        }
        reviews.sort_by_key(|r| std::cmp::Reverse(r.created_ms));
        Ok(reviews)
    }

    /// Load one review by id. `Ok(None)` if it doesn't exist. Errors if
    /// the stored JSON is malformed or its schema `v` is newer than this
    /// build supports.
    pub fn load(&self, id: &str) -> Result<Option<Review>> {
        let rel = review_path(id);
        let Some(bytes) = self.io.read(&rel)? else {
            return Ok(None);
        };
        let review: Review = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing review {id} ({rel})"))?;
        check_schema_version(review.v)?;
        Ok(Some(review))
    }

    /// Persist `review` (create or overwrite). Does not touch
    /// `updated_ms` — callers own that (the mutation helpers on [`Review`]
    /// already bump it).
    pub fn save(&self, review: &Review) -> Result<()> {
        let rel = review_path(&review.id);
        let bytes = serde_json::to_vec_pretty(review)
            .with_context(|| format!("serializing review {}", review.id))?;
        self.io.write_atomic(&rel, &bytes)
    }

    /// Remove a review. Not an error if it doesn't exist.
    pub fn delete(&self, id: &str) -> Result<()> {
        self.io.remove(&review_path(id))
    }

    /// Create, persist, and return a new draft review over `source`.
    pub fn create(&self, source: DiffSource) -> Result<Review> {
        let review = Review::new_draft(source);
        self.save(&review)?;
        Ok(review)
    }

    /// Run `f` (a load-mutate-save critical section) while holding the
    /// store's cross-process lock — the durable-concurrency fix
    /// (docs/backlog.md: "the real fix is a lock file around
    /// load-mutate-save", Phase-2 review P1 residual). Every mutation the
    /// GUI and CLI perform is `load` (fresh, ignoring whatever stale copy
    /// the caller had) → mutate the in-memory `Review` → `save`; wrapping
    /// that whole span here means two writers (GUI + CLI, two CLI
    /// invocations, ...) can never interleave a load and a save and
    /// silently drop each other's update, closing the race the pre-
    /// existing "fresh-load-before-mutating" convention only narrowed.
    ///
    /// Pure reads (`list`/`load` on their own) don't need this — only a
    /// span that reads then later writes back based on what it read.
    ///
    /// `E` needs `From<anyhow::Error>` so callers using `anyhow::Result`
    /// get it for free (the reflexive `impl<T> From<T> for T`), while
    /// callers with their own error enum (`dv_cli`'s `CliError`) only need
    /// one `From` impl to use this too. See [`super::lock`] for the
    /// locking algorithm (representation, staleness, timeout).
    pub fn with_lock<T, E>(&self, f: impl FnOnce() -> Result<T, E>) -> Result<T, E>
    where
        E: From<anyhow::Error>,
    {
        let _guard = super::lock::acquire(&self.io).map_err(E::from)?;
        f()
    }
}

fn review_path(id: &str) -> String {
    format!("{REVIEWS_DIR}/{id}.json")
}
