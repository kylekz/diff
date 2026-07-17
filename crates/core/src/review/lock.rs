//! Durable concurrency for the review store (docs/backlog.md: "Review
//! store: durable concurrency answer ... the real fix is a lock file
//! around load-mutate-save", Phase-2 review P1 residual).
//!
//! [`ReviewStore::with_lock`](super::store::ReviewStore::with_lock) is the
//! only entry point this module exists for: every read-modify-write
//! mutation (comment add/reply/resolve/edit/delete, submit writeback, PR
//! draft find-or-create, ...) wraps its whole load-mutate-save span in it,
//! so the fresh-load-before-mutating discipline those call sites already
//! had (which only *shrinks* the race window) becomes airtight instead —
//! two writers can no longer interleave a load/save pair and silently drop
//! each other's update.
//!
//! **Representation**: a plain FILE (not a directory) at `dv/.lock` —
//! deliberately a SIBLING of `dv/reviews/`, not inside it, so it never
//! shows up in [`super::store::ReviewStore::list`]'s directory listing or
//! perturbs the local `notify` watcher (which watches `dv/reviews`
//! non-recursively) or the WSL digest-poll fallback (which digests
//! `dv/reviews` only) — no filtering logic needed anywhere else, the lock
//! is simply never inside anything either of those enumerate. Content is
//! `"<pid>:<created_ms>"`, written in one shot by
//! [`super::io::StoreIo::create_exclusive`] (which every backend — local
//! `std::fs`, a live `fs_lock`-capable `dv-host` connection, or the WSL
//! shell `noclobber` fallback — implements as a real atomic create-if-
//! absent, never a check-then-write).
//!
//! **Acquisition**: try [`super::io::StoreIo::create_exclusive`]; if
//! something's already there, read it back and check its age. Older than
//! [`STALE_AGE_MS`] means the holder almost certainly crashed (a live
//! critical section is milliseconds, never seconds) — break it (best-
//! effort remove) and retry immediately. Otherwise sleep
//! [`RETRY_INTERVAL`] and try again, bounded by [`ACQUIRE_TIMEOUT`]
//! overall; exceeding that with no stale lock in sight fails with a clear,
//! actionable error rather than blocking forever. A lock file whose
//! content doesn't parse as `<pid>:<ms>` (a partial write from an
//! extremely unlucky crash, say) is treated as stale immediately — there's
//! nothing trustworthy to wait out.
//!
//! **Release**: dropping the returned guard removes the lock file
//! (best-effort — a failed remove here just means the NEXT acquirer pays
//! the stale-break tax instead of finding it gone outright, not a
//! correctness problem).

use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use super::io::StoreIo;
use super::now_ms;

/// Sibling of `store::REVIEWS_DIR` (`"dv/reviews"`), deliberately NOT
/// inside it — see the module doc for why that placement alone is what
/// keeps the lock invisible to `list`/the watchers, with no extra
/// filtering needed.
const LOCK_REL: &str = "dv/.lock";

/// A lock older than this is presumed abandoned by a crashed holder: every
/// real critical section this guards (load one JSON file, mutate an
/// in-memory struct, save it back) runs in low single-digit milliseconds
/// locally and rarely more than a couple hundred over the WSL host
/// connection — several seconds of margin above that before a lock is
/// ever second-guessed.
const STALE_AGE_MS: u64 = 5_000;

/// Bounded total wait for [`acquire`] before giving up — long enough to
/// ride out ordinary contention (another writer's whole critical section
/// is milliseconds) without ever hanging a CLI invocation or a GUI
/// background task indefinitely on a wedged lock that somehow isn't stale
/// yet.
const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(2_000);

/// How long to sleep between contended acquisition attempts.
const RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// Held for the duration of a load-mutate-save critical section; dropping
/// it releases the lock (best-effort — see the module doc).
pub(super) struct LockGuard<'a> {
    io: &'a StoreIo,
    /// Set once release has already happened (or was never needed, e.g. a
    /// construction failure this module never actually produces) — purely
    /// defensive against a future double-drop path; today every `LockGuard`
    /// is created already holding the lock.
    released: bool,
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            // Best-effort: a failed remove here just means the next
            // acquirer pays the stale-break tax (see module doc) instead of
            // finding it already gone — never a correctness problem, since
            // the content this held is meaningless once we're done with it.
            let _ = self.io.remove(LOCK_REL);
        }
    }
}

impl std::fmt::Debug for LockGuard<'_> {
    // `StoreIo` itself isn't `Debug` (it wraps a `CommandBuilder`/
    // `RepoLocation` with no particular need for one) — a minimal opaque
    // impl is all `Result::expect_err` et al need in tests.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockGuard").finish_non_exhaustive()
    }
}

/// Acquire the store lock, blocking (with short sleeps) for up to
/// [`ACQUIRE_TIMEOUT`] total. See the module doc for the full algorithm.
pub(super) fn acquire(io: &StoreIo) -> Result<LockGuard<'_>> {
    let owner = format!("{}:{}", std::process::id(), now_ms());
    let deadline = Instant::now() + ACQUIRE_TIMEOUT;

    loop {
        if io.create_exclusive(LOCK_REL, owner.as_bytes())? {
            return Ok(LockGuard {
                io,
                released: false,
            });
        }

        // Contended: see whether the existing lock is stale enough to
        // break. `Ok(None)` here (removed between our failed create and
        // this read by whoever held it, or by another breaker) means the
        // contention already cleared — loop straight back to
        // `create_exclusive` without sleeping. A transient `Err` here (the
        // Windows "pending delete" quirk `create_exclusive_at` already
        // works around on the create side can equally surface on a `read`
        // racing another thread's create/remove churn under heavy
        // contention — observed directly under this module's own hammer
        // test) is swallowed rather than propagated: it means "couldn't
        // tell if the lock is stale THIS instant," not "the store is
        // broken," so just fall through to the timeout/sleep check and try
        // again next iteration.
        if let Ok(Some(bytes)) = io.read(LOCK_REL)
            && !is_fresh(&bytes)
        {
            // Best-effort: another process may win the race to break it
            // too (both see it as stale and both remove it) — harmless,
            // `remove` is idempotent and the next `create_exclusive` still
            // only lets exactly one of us through.
            let _ = io.remove(LOCK_REL);
            continue;
        }

        if Instant::now() >= deadline {
            bail!(
                "timed out after {:?} waiting for the review store lock ({LOCK_REL}); \
                 another dv process appears to be actively using this review store",
                ACQUIRE_TIMEOUT
            );
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
}

/// Whether `bytes` (a lock file's contents) names an owner recent enough
/// to still trust — i.e. NOT stale. Unparsable content (a corrupt/partial
/// write) is treated as NOT fresh: there's nothing trustworthy to wait
/// out, so recovering immediately is safer than potentially blocking for
/// the full timeout over a lock nobody can even interpret.
fn is_fresh(bytes: &[u8]) -> bool {
    let Some(age) = lock_age_ms(bytes) else {
        return false;
    };
    age <= STALE_AGE_MS
}

/// Parse a lock file's `"<pid>:<created_ms>"` payload and return its age
/// in milliseconds (saturating at 0 if the clock has since moved
/// backward — never a negative/overflowing age). `None` if the content
/// doesn't parse at all.
fn lock_age_ms(bytes: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (_pid, created_str) = text.split_once(':')?;
    let created_ms: u64 = created_str.trim().parse().ok()?;
    Some(now_ms().saturating_sub(created_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::RepoLocation;

    fn scratch_repo(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dv-lock-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".git")).expect("create scratch .git");
        dir
    }

    fn io_for(root: &std::path::Path) -> StoreIo {
        StoreIo::new(RepoLocation::Local(root.to_path_buf()))
    }

    #[test]
    fn acquire_then_drop_releases_the_lock() {
        let dir = scratch_repo("acquire-release");
        let io = io_for(&dir);

        let guard = acquire(&io).expect("first acquire should succeed immediately");
        drop(guard);

        // Released: a second acquire must also succeed immediately (well
        // under the timeout), proving the lock file is actually gone.
        let started = Instant::now();
        let _guard2 = acquire(&io).expect("should reacquire after release");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "reacquiring a released lock should be near-instant"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn acquire_blocks_while_held_and_succeeds_once_released() {
        let dir = scratch_repo("contention-timing");
        let io = io_for(&dir);

        let guard = acquire(&io).expect("first acquire");

        // A second acquire on a SEPARATE StoreIo (same location) must not
        // succeed while the first is still held — release it from a
        // background thread after a short delay and confirm the waiter
        // unblocks right around then, not immediately and not by timing
        // out. `thread::scope` (not `thread::spawn`) because `guard`
        // borrows `io`, which isn't `'static`.
        let io2 = io_for(&dir);
        let released_at = std::sync::Mutex::new(None::<Instant>);
        let start = Instant::now();
        let acquired_at = std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(150));
                drop(guard);
                *released_at.lock().unwrap() = Some(Instant::now());
            });

            let _guard3 = acquire(&io2).expect("should acquire once the holder releases");
            Instant::now()
        });

        assert!(
            start.elapsed() >= Duration::from_millis(100),
            "must actually have waited for the holder, not raced past it"
        );
        let released_at = released_at.lock().unwrap().expect("release recorded");
        assert!(
            acquired_at >= released_at,
            "must not acquire before the prior holder actually released"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_lock_is_broken_and_acquisition_recovers_within_the_bound() {
        let dir = scratch_repo("stale-break");
        let io = io_for(&dir);

        // Simulate a crashed holder: a lock file that's already older than
        // `STALE_AGE_MS`, pre-created directly (never released).
        let ancient_owner = format!("999999:{}", now_ms().saturating_sub(STALE_AGE_MS + 1_000));
        assert!(
            io.create_exclusive("dv/.lock", ancient_owner.as_bytes())
                .unwrap(),
            "precondition: scratch dir must start with no lock"
        );

        let started = Instant::now();
        let _guard = acquire(&io).expect("must recover a stale lock, not time out");
        assert!(
            started.elapsed() < ACQUIRE_TIMEOUT,
            "stale-lock recovery must complete within the bounded acquire wait"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_lock_content_is_treated_as_stale() {
        let dir = scratch_repo("corrupt-lock");
        let io = io_for(&dir);
        assert!(
            io.create_exclusive("dv/.lock", b"not-a-valid-payload")
                .unwrap()
        );

        let started = Instant::now();
        let _guard = acquire(&io).expect("unparsable lock content must not block forever");
        assert!(started.elapsed() < ACQUIRE_TIMEOUT);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn acquire_times_out_with_a_clear_error_when_genuinely_held() {
        let dir = scratch_repo("timeout");
        let io = io_for(&dir);

        // A FRESH lock (created "now") that nothing ever releases: the
        // waiter must give up at the bound instead of hanging, and the
        // stale-break path must not fire (it isn't stale yet).
        let fresh_owner = format!("123:{}", now_ms());
        assert!(
            io.create_exclusive("dv/.lock", fresh_owner.as_bytes())
                .unwrap()
        );

        let started = Instant::now();
        let err = acquire(&io).expect_err("a genuinely held, non-stale lock must time out");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= ACQUIRE_TIMEOUT.mul_f32(0.8),
            "should have waited close to the full bound, took {elapsed:?}"
        );
        assert!(
            err.to_string().contains("timed out"),
            "error should clearly say what happened: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn contention_two_threads_hammering_interleaved_adds_loses_nothing() {
        // The actual regression this whole slice exists to fix: two
        // writers racing load-mutate-save on the SAME review must not
        // silently drop each other's update. Modeled directly against the
        // lock primitive (rather than `ReviewStore::with_lock`, which is
        // exercised by the CLI race script and `review_integration.rs`
        // instead) — plain load/append/save against a shared JSON file
        // under `io`, gated by `acquire`/drop.
        use std::sync::Arc;

        let dir = scratch_repo("contention-hammer");
        let io = Arc::new(io_for(&dir));
        io.write_atomic("dv/reviews/r-1.json", b"[]").unwrap();

        const PER_THREAD: usize = 50;
        let mut handles = Vec::new();
        for t in 0..2 {
            let io = Arc::clone(&io);
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let _guard = acquire(&io).expect("acquire under test contention");
                    let bytes = io.read("dv/reviews/r-1.json").unwrap().unwrap();
                    let mut items: Vec<String> = serde_json::from_slice(&bytes).unwrap();
                    items.push(format!("t{t}-{i}"));
                    let updated = serde_json::to_vec(&items).unwrap();
                    io.write_atomic("dv/reviews/r-1.json", &updated).unwrap();
                    // guard drops here, releasing before the next iteration
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let bytes = io.read("dv/reviews/r-1.json").unwrap().unwrap();
        let items: Vec<String> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            items.len(),
            2 * PER_THREAD,
            "every append from both threads must have landed — got {}",
            items.len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
