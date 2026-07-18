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
//! is simply never inside anything either of those enumerate. Content is a
//! unique per-acquisition OWNER TOKEN,
//! `"<pid>:<process-nonce>:<acquire-seq>:<created_ms>"`, written in one
//! shot by [`super::io::StoreIo::create_exclusive`] (which every backend —
//! local `std::fs`, a live `fs_lock`-capable `dv-host` connection, or the
//! WSL shell `noclobber` fallback — implements as a real atomic
//! create-if-absent, never a check-then-write). `created_ms` is carried
//! for human debugging only and is NEVER trusted for staleness: the writer
//! and a contender can live in different clock domains (Windows `dv.exe`
//! vs. a WSL-side `dv-cli`, where multi-second WSL2 clock drift after a
//! host sleep is a real phenomenon), so a wall-clock age computed across
//! that boundary can break a perfectly live lock on first contention.
//!
//! **Acquisition** (observation-based staleness + verify-before-remove +
//! owner-token verification — capstone P2-A/P2-B redesign):
//!
//!   1. Try [`super::io::StoreIo::create_exclusive`] with our token. On
//!      success, READ BACK and confirm the file still holds OUR token
//!      before entering the critical section — a racing breaker acting on
//!      a stale observation could have deleted our just-created lock in
//!      the sliver between its verify-read and its remove; the read-back
//!      is what turns that residual window into a harmless retry instead
//!      of two concurrent holders.
//!   2. On contention, read the current payload. If it IS our own token
//!      (a lost host response made our successful create look contended —
//!      see `StoreIo::try_host_fs`'s idempotency note), we already own the
//!      lock. If its `pid:process-nonce` prefix names THIS process but no
//!      live [`LockGuard`] in this process holds that exact token, it's a
//!      strand from our own process (host death mid-create/mid-release) —
//!      break it immediately, no waiting.
//!   3. Otherwise, never trust the payload's timestamp: record the exact
//!      payload bytes observed and only treat the lock as stale after
//!      [`STALE_AGE_MS`] of OUR OWN LOCAL polling during which the payload
//!      never changed. Any payload change (a new holder) resets the
//!      observation clock; unparsable content gets the same observation
//!      window (there's nothing else trustworthy to key off).
//!   4. Breaking = [`super::io::StoreIo::remove_if_matches`]: remove ONLY
//!      if a fresh read still returns the exact observed payload (on the
//!      WSL shell path that compare-and-remove is a single `sh -c`
//!      invocation, so the window is one op, not two). Combined with the
//!      read-back in step 1, a mid-break re-creation by the real owner is
//!      never deleted.
//!
//! The whole loop is bounded by [`ACQUIRE_TIMEOUT`]; see the constants'
//! doc comments for how the budget relates to [`STALE_AGE_MS`].
//!
//! **Release**: dropping the returned guard removes the lock file ONLY if
//! it still holds this guard's own token (the same compare-and-remove as
//! breaking) — an overstaying holder whose lock was legitimately broken
//! must not delete its successor's live lock. Best-effort: a failed remove
//! just means the NEXT acquirer pays the stale-observation tax instead of
//! finding it gone outright, not a correctness problem.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use super::io::StoreIo;
use super::now_ms;

/// Sibling of `store::REVIEWS_DIR` (`"dv/reviews"`), deliberately NOT
/// inside it — see the module doc for why that placement alone is what
/// keeps the lock invisible to `list`/the watchers, with no extra
/// filtering needed.
const LOCK_REL: &str = "dv/.lock";

/// How long a contender must observe an UNCHANGING lock payload (via its
/// own local polling — never the payload's embedded timestamp, see the
/// module doc) before presuming the holder crashed and breaking the lock.
/// Every real critical section this guards (load one JSON file, mutate an
/// in-memory struct, save it back) runs in low single-digit milliseconds
/// locally and rarely more than a couple hundred over the WSL host
/// connection — two seconds of stable observation is an order of magnitude
/// above that before a lock is ever second-guessed, while staying well
/// inside [`ACQUIRE_TIMEOUT`]'s budget (below).
const STALE_AGE_MS: u64 = 2_000;

/// Bounded total wait for [`acquire`] before giving up. The budget must
/// cover the WORST recoverable case — a contender arriving just after a
/// holder crashed pays one full [`STALE_AGE_MS`] observation window before
/// it may break, plus the break + re-create round trips (each can be a
/// `wsl.exe` spawn on the shell fallback path, ~hundreds of ms) — so
/// `ACQUIRE_TIMEOUT >= STALE_AGE_MS + generous margin` is a hard
/// relationship: shrink [`STALE_AGE_MS`] rather than this margin if the
/// window ever needs tightening. Exceeding the bound fails with a clear,
/// actionable error rather than blocking a CLI invocation or GUI task
/// forever on a lock that keeps changing hands.
const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(6_000);

/// How long to sleep between contended acquisition attempts.
const RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// Tokens of every lock currently held by a live [`LockGuard`] — or
/// reserved by an in-flight [`acquire`] call ([`TokenReservation`]) — in
/// THIS process. Two purposes: (a) the self-strand fast-break (module doc,
/// acquisition step 2) must not fire for a lock a SIBLING THREAD of this
/// process legitimately holds (or is mid-acquiring) right now; (b) `Drop`
/// unregisters, so a token left here is by definition live.
fn active_tokens() -> &'static Mutex<Vec<String>> {
    static ACTIVE: Mutex<Vec<String>> = Mutex::new(Vec::new());
    &ACTIVE
}

/// A per-process random nonce distinguishing THIS process from any other
/// process that happens to share (or recycle) its numeric pid — including
/// a same-pid process on the other side of the Windows/WSL boundary, where
/// pid namespaces are entirely disjoint. `RandomState` is seeded with
/// fresh OS entropy per process, which is exactly the property needed;
/// no extra dependency required.
fn process_nonce() -> u64 {
    use std::hash::BuildHasher as _;
    static NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *NONCE
        .get_or_init(|| std::collections::hash_map::RandomState::new().hash_one(std::process::id()))
}

/// A fresh, process-unique owner token: `pid:process-nonce:acquire-seq:ms`
/// (see the module doc for what each field is — and is not — used for).
fn new_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ACQUIRE_SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}:{:016x}:{}:{}",
        std::process::id(),
        process_nonce(),
        ACQUIRE_SEQ.fetch_add(1, Ordering::Relaxed),
        now_ms()
    )
}

/// Whether `bytes` names a token minted by THIS process (`pid` AND the
/// per-process random nonce both match — pid alone is not identity across
/// the Windows/WSL pid-namespace boundary) that no live [`LockGuard`] in
/// this process currently holds — i.e. a strand our own process left
/// behind (host death mid-create/mid-release), breakable immediately
/// without the observation window.
fn is_own_stranded_token(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let own_prefix = format!("{}:{:016x}:", std::process::id(), process_nonce());
    if !text.starts_with(&own_prefix) {
        return false;
    }
    !active_tokens()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|t| t == text)
}

/// Held for the duration of a load-mutate-save critical section; dropping
/// it releases the lock (compare-and-remove of this guard's own token —
/// see the module doc's Release section).
pub(super) struct LockGuard<'a> {
    io: &'a StoreIo,
    token: String,
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        active_tokens()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|t| t != &self.token);
        // Compare-and-remove: only if the file still holds OUR token. An
        // overstayed guard whose lock was legitimately broken (and possibly
        // re-acquired by a successor) must never delete the successor's
        // live lock. Best-effort: a failed remove just means the next
        // acquirer pays the stale-observation tax (see module doc) instead
        // of finding it already gone — never a correctness problem.
        let _ = self.io.remove_if_matches(LOCK_REL, self.token.as_bytes());
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

/// Keeps a token registered in [`active_tokens`] for the LIFETIME OF THE
/// WHOLE `acquire` CALL, not just from guard construction onward: without
/// this, a sibling thread of the same process reading the lock in the
/// sliver between our `create_exclusive` and the guard's registration
/// would see our own-process token with no registered holder and
/// fast-break our LIVE, about-to-be-returned lock (a two-holder race the
/// two-contender unit test reproduced deterministically). Disarmed on
/// success — from that point the returned [`LockGuard`]'s `Drop` owns the
/// deregistration — and cleaned up by `Drop` on every failure path
/// (timeout, propagated I/O error).
struct TokenReservation {
    token: String,
    armed: bool,
}

impl Drop for TokenReservation {
    fn drop(&mut self) {
        if self.armed {
            active_tokens()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|t| t != &self.token);
        }
    }
}

/// Acquire the store lock, blocking (with short sleeps) for up to
/// [`ACQUIRE_TIMEOUT`] total. See the module doc for the full algorithm.
pub(super) fn acquire(io: &StoreIo) -> Result<LockGuard<'_>> {
    let token = new_token();
    active_tokens()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(token.clone());
    let mut reservation = TokenReservation {
        token: token.clone(),
        armed: true,
    };
    let deadline = Instant::now() + ACQUIRE_TIMEOUT;
    // The payload last observed on a contended lock, and when THIS process
    // first observed exactly those bytes — the module doc's step-3
    // observation clock. Never seeded from the payload's own timestamp.
    let mut observed: Option<(Vec<u8>, Instant)> = None;

    loop {
        if io.create_exclusive(LOCK_REL, token.as_bytes())? {
            // Step 1's read-back: confirm the lock still holds OUR token
            // before entering the critical section. A racing breaker whose
            // verify-read predated our create can have removed it in the
            // meantime — retrying is cheap; proceeding would mean two
            // holders.
            match io.read(LOCK_REL) {
                Ok(Some(bytes)) if bytes == token.as_bytes() => {
                    // Registration stays put — ownership of it just moves
                    // from the reservation to the guard's Drop.
                    reservation.armed = false;
                    return Ok(LockGuard { io, token });
                }
                _ => {
                    observed = None;
                    // Fall through to the deadline check + sleep, then
                    // retry the create.
                }
            }
        } else {
            // Contended. A transient `Err` on this read (the Windows
            // "pending delete" quirk `create_exclusive_at` works around on
            // the create side can equally surface on a `read` racing
            // another thread's create/remove churn — observed directly
            // under this module's own hammer test) means "couldn't tell
            // THIS instant", not "the store is broken": keep the current
            // observation and fall through to the sleep.
            match io.read(LOCK_REL) {
                Ok(Some(bytes)) => {
                    if bytes == token.as_bytes() {
                        // Our own create actually landed but was reported
                        // as contended (a lost host response, re-run via
                        // the shell fallback) — we already own the lock.
                        reservation.armed = false;
                        return Ok(LockGuard { io, token });
                    }
                    let now = Instant::now();
                    let stale = match &observed {
                        Some((prev, since)) if *prev == bytes => {
                            now.duration_since(*since) >= Duration::from_millis(STALE_AGE_MS)
                        }
                        _ => {
                            observed = Some((bytes.clone(), now));
                            false
                        }
                    };
                    if stale || is_own_stranded_token(&bytes) {
                        // Step 4: break by compare-and-remove of the exact
                        // observed payload — if the payload changed in the
                        // meantime (the presumed-dead holder is alive, or a
                        // faster breaker already recycled the lock), this
                        // removes nothing and the next read re-observes.
                        // Best-effort on failure: retry next tick.
                        let _ = io.remove_if_matches(LOCK_REL, &bytes);
                        observed = None;
                        continue;
                    }
                }
                Ok(None) => {
                    // Freed (or broken by someone else) between our failed
                    // create and this read — retry the create immediately.
                    observed = None;
                    continue;
                }
                Err(_) => {}
            }
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

    /// A payload that could only have come from ANOTHER process: pid 0 is
    /// never a real dv process, and the nonce field can't collide with this
    /// process's random one.
    fn foreign_token(created_ms: u64) -> String {
        format!("0:ffffffffffffffff:0:{created_ms}")
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
        // out. This also exercises the self-strand fast-break's guard rail:
        // the sibling thread's held token IS registered in `active_tokens`,
        // so the waiter must not fast-break it despite the matching
        // process prefix. `thread::scope` (not `thread::spawn`) because
        // `guard` borrows `io`, which isn't `'static`.
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
    fn stale_foreign_lock_is_broken_after_the_observation_window() {
        let dir = scratch_repo("stale-break");
        let io = io_for(&dir);

        // Simulate a crashed FOREIGN holder: a pre-created lock nothing
        // ever releases. Its embedded timestamp is recent — staleness must
        // come from OUR OWN observation window, never the payload's clock.
        assert!(
            io.create_exclusive("dv/.lock", foreign_token(now_ms()).as_bytes())
                .unwrap(),
            "precondition: scratch dir must start with no lock"
        );

        let started = Instant::now();
        let _guard = acquire(&io).expect("must recover a stale lock, not time out");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(STALE_AGE_MS),
            "must not break a foreign lock before the observation window elapses (took {elapsed:?})"
        );
        assert!(
            elapsed < ACQUIRE_TIMEOUT,
            "stale-lock recovery must complete within the bounded acquire wait"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clock_skew_far_future_created_ms_is_still_breakable() {
        // The P2-B scenario: a payload whose embedded clock is from another
        // clock domain (here: far in the future, as post-sleep WSL2 drift
        // can produce). An age-based check would consider this lock
        // "fresh" forever; observation-based staleness must still recover.
        let dir = scratch_repo("clock-skew");
        let io = io_for(&dir);
        let future_ms = now_ms() + 86_400_000; // a full day ahead
        assert!(
            io.create_exclusive("dv/.lock", foreign_token(future_ms).as_bytes())
                .unwrap()
        );

        let started = Instant::now();
        let _guard = acquire(&io).expect("a future-dated abandoned lock must still be breakable");
        assert!(started.elapsed() < ACQUIRE_TIMEOUT);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_lock_content_is_broken_after_the_observation_window() {
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
    fn own_stranded_token_is_fast_broken_without_the_observation_window() {
        // P3-9: a lock bearing THIS process's pid + process nonce with no
        // live guard registered for it (a host-death strand) is breakable
        // immediately — no STALE_AGE_MS wait.
        let dir = scratch_repo("self-strand");
        let io = io_for(&dir);
        let stranded = new_token();
        assert!(
            io.create_exclusive("dv/.lock", stranded.as_bytes())
                .unwrap()
        );

        let started = Instant::now();
        let _guard = acquire(&io).expect("must fast-break our own stranded lock");
        assert!(
            started.elapsed() < Duration::from_millis(STALE_AGE_MS),
            "self-strand break must not wait out the observation window (took {:?})",
            started.elapsed()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn acquire_times_out_when_the_lock_keeps_changing_hands() {
        // With observation-based staleness, a lock whose payload keeps
        // CHANGING is never breakable (each change resets the observation
        // clock — that's the design: a changing payload means live
        // holders). A contender that always loses must give up at the
        // bound with a clear error instead of hanging.
        let dir = scratch_repo("timeout");
        let io = io_for(&dir);
        assert!(
            io.create_exclusive("dv/.lock", foreign_token(now_ms()).as_bytes())
                .unwrap()
        );

        let churn_stop = std::sync::atomic::AtomicBool::new(false);
        let io_churn = io_for(&dir);
        let err = std::thread::scope(|scope| {
            scope.spawn(|| {
                // Rewrite the lock with a fresh foreign payload well inside
                // every observation window, simulating continuous foreign
                // turnover.
                let mut i = 0u64;
                while !churn_stop.load(std::sync::atomic::Ordering::SeqCst) {
                    let _ = io_churn.write_atomic(
                        "dv/.lock",
                        format!("0:ffffffffffffffff:{i}:{}", now_ms()).as_bytes(),
                    );
                    i += 1;
                    std::thread::sleep(Duration::from_millis(200));
                }
            });
            let started = Instant::now();
            let err = acquire(&io).expect_err("a perpetually-churning lock must time out");
            let elapsed = started.elapsed();
            churn_stop.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(
                elapsed >= ACQUIRE_TIMEOUT.mul_f32(0.8),
                "should have waited close to the full bound, took {elapsed:?}"
            );
            err
        });
        assert!(
            err.to_string().contains("timed out"),
            "error should clearly say what happened: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overstayed_drop_does_not_delete_a_successors_lock() {
        // P2-A's second half: a guard whose lock was broken out from under
        // it (simulated directly) must not delete the lock a successor now
        // legitimately holds.
        let dir = scratch_repo("overstay-drop");
        let io = io_for(&dir);
        let guard = acquire(&io).expect("first acquire");

        // Simulate a breaker + successor: replace the lock content.
        let successor = foreign_token(now_ms());
        io.remove("dv/.lock").unwrap();
        assert!(
            io.create_exclusive("dv/.lock", successor.as_bytes())
                .unwrap()
        );

        drop(guard);
        let bytes = io
            .read("dv/.lock")
            .unwrap()
            .expect("successor's lock must survive the overstayed drop");
        assert_eq!(bytes, successor.as_bytes());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn break_race_verify_before_remove_spares_a_recreated_lock() {
        // The P2-A break race, driven at the primitive this module breaks
        // with: contender B observed payload X, but between B's observation
        // and B's remove, A broke the lock and re-created it as Y. B's
        // compare-and-remove keyed on X must NOT delete Y.
        let dir = scratch_repo("break-race");
        let io = io_for(&dir);
        let x = foreign_token(1);
        let y = foreign_token(2);
        assert!(io.create_exclusive("dv/.lock", x.as_bytes()).unwrap());
        // A's break + re-create lands first:
        io.remove("dv/.lock").unwrap();
        assert!(io.create_exclusive("dv/.lock", y.as_bytes()).unwrap());
        // B's remove, still keyed on its stale observation of X:
        io.remove_if_matches("dv/.lock", x.as_bytes()).unwrap();
        assert_eq!(
            io.read("dv/.lock").unwrap().as_deref(),
            Some(y.as_bytes()),
            "a compare-and-remove keyed on a stale observation must remove nothing"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_contenders_racing_a_preseeded_stale_lock_both_eventually_win_exclusively() {
        // Two threads facing the same pre-seeded crashed-holder lock: both
        // must eventually acquire (serially, via observation + break), and
        // the mutual exclusion they provide must hold — checked with a
        // shared counter that would show interleaving if both ever held the
        // lock at once.
        use std::sync::atomic::{AtomicU32, Ordering};
        let dir = scratch_repo("stale-two-contenders");
        let io = io_for(&dir);
        assert!(
            io.create_exclusive("dv/.lock", foreign_token(now_ms()).as_bytes())
                .unwrap()
        );

        let inside = AtomicU32::new(0);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let dir = dir.clone();
                let inside = &inside;
                scope.spawn(move || {
                    let io = io_for(&dir);
                    let _guard = acquire(&io).expect("contender must recover the stale lock");
                    let concurrent = inside.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(concurrent, 0, "two lock holders at once");
                    std::thread::sleep(Duration::from_millis(20));
                    inside.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });

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
