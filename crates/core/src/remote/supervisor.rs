//! Supervisor for host-backed watch subscriptions (docs/backlog.md "Remote
//! watchers — resubscribe-on-respawn + subscribe off the GUI thread").
//!
//! One supervisor thread per watcher, owned by the watcher handle. It — not
//! the caller — does every piece of blocking work the old inline code did on
//! the caller's (GUI) thread: acquiring a host client (which can spawn a
//! host / boot a distro, seconds), the `watch/subscribe` RPC (up to
//! `REQUEST_TIMEOUT` against a wedged-alive host), and the best-effort
//! `watch/unsubscribe` at teardown. `spawn` itself only starts a thread and
//! returns, so `ReviewStore::watch` / `watch_worktree` are now cheap from
//! any thread.
//!
//! The supervisor also closes the S4 gap this backlog entry names: it
//! registers a [`HostClient::on_death`] listener after every successful
//! subscribe, so a mid-session host death (crash, `kill -9`, distro
//! stopped) wakes it immediately — it then waits out
//! [`super::manager`]'s respawn cool-down (retrying `client_source` once
//! per `poll_interval`, which is cheap while the manager reports
//! cooling-down) and resubscribes on the NEW connection, repeatedly, for as
//! many crash/respawn cycles as the session sees. On every re-arm it fires
//! one synthetic `on_change` so consumers re-check state and catch anything
//! missed during the outage (the GUI reloads from the store on any event;
//! `dv review wait` diffs snapshots — both are synthetic-safe).
//!
//! Lock discipline: the supervisor holds NO lock across any RPC. The only
//! lock-heavy call is `client_source` (production: `manager::client_for`,
//! which holds its per-distro slot lock across a spawn by design — a
//! pre-existing, deliberate serialization), and `watch_subscribe` happens
//! strictly after that call returns.
//!
//! `client_source` and `fallback_tick` are injectable so the whole state
//! machine is testable against local scripted hosts
//! (`HostClient::spawn_command`) without WSL — see
//! crates/host/tests/host_protocol.rs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use super::client::HostClient;

/// Cadence of the degraded loop (fallback poll ticks + host-reacquisition
/// retries) while no live subscription exists. Matches the pre-supervisor
/// WSL digest-poll interval.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// After a `watch/subscribe` RPC fails against a client the source handed
/// us (a live host answering with an error, say), wait this many ticks
/// before asking again — one failed subscribe per second against the same
/// healthy-but-refusing host would be pure stderr spam. Host DEATH is not
/// throttled by this: it wakes the loop via `on_death` and the manager's
/// own cool-down paces the respawn.
const SUBSCRIBE_RETRY_TICKS: u32 = 30;

/// What the supervisor thread runs on.
pub struct SupervisorConfig {
    /// Human-readable repo label for log lines.
    pub label: String,
    /// Absolute in-distro repo root — `watch/subscribe`'s `root` param.
    pub root: String,
    /// `"store"` or `"worktree"` — `watch/subscribe`'s `kind` param.
    pub kind: &'static str,
    /// The watcher's callback contract, unchanged from the pre-supervisor
    /// watchers: invoked from a background thread (the host client's
    /// watch-dispatch thread for real events, this supervisor's thread for
    /// synthetic ones and fallback-poll hits); must be cheap.
    pub on_change: Arc<dyn Fn() + Send + Sync>,
    /// Where clients come from. Production: `manager::client_for(distro)`
    /// filtered to alive + `watch`-capable — which means the MANAGER owns
    /// spawn pacing (cool-downs) and this loop just asks again next tick.
    /// `None` = no usable host right now.
    pub client_source: Box<dyn FnMut() -> Option<Arc<HostClient>> + Send>,
    /// Degraded-mode poll, run once per `poll_interval` while no
    /// subscription is live; returns whether a change was observed since
    /// the previous call (the first call takes a baseline and returns
    /// `false`). The store watcher passes its digest poll here — so during
    /// an outage it degrades to the same 1s digest poll a host-less session
    /// gets, instead of going silent. `None` (the worktree watcher) means
    /// no fallback exists: the loop just idles between reacquisition
    /// attempts.
    pub fallback_tick: Option<Box<dyn FnMut() -> bool + Send>>,
    /// Fire one synthetic `on_change` on the FIRST arm (initial subscribe,
    /// or first fallback baseline) — not just on re-arms. The store watcher
    /// needs this: `dv review wait` arms the watcher BEFORE its baseline
    /// snapshot and relies on that ordering, but `spawn` returns before the
    /// subscription actually exists, so the synthetic is what tells it
    /// "armed now — re-snapshot" and closes the gap. The worktree watcher
    /// leaves it off (its only consumer recomputed its state immediately
    /// before watching; an extra full revalidate per open would be waste).
    /// Re-arms after an outage ALWAYS fire a synthetic, regardless.
    pub synthetic_on_first_arm: bool,
    /// Tick cadence — [`DEFAULT_POLL_INTERVAL`] in production, shrunk in
    /// tests.
    pub poll_interval: Duration,
}

enum Wake {
    /// A registered `on_death` listener fired — the connection it was
    /// registered on is gone. Advisory: the loop re-checks its CURRENT
    /// client's liveness, so a stale note from an older connection is
    /// ignored.
    HostDied,
    /// The watcher handle was dropped.
    Shutdown,
}

/// Keeps the supervised watch alive; dropping it stops event delivery
/// (best-effort — an in-flight callback may still complete, same contract
/// as every other watcher in this codebase) and tells the supervisor
/// thread to tear down (unregister + best-effort unsubscribe) and exit.
/// Drop itself never blocks: teardown RPCs happen on the supervisor's
/// thread, not the dropper's.
pub struct RemoteWatchSupervisor {
    stopped: Arc<AtomicBool>,
    tx: mpsc::Sender<Wake>,
}

impl Drop for RemoteWatchSupervisor {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Wake::Shutdown);
    }
}

/// Start the supervisor thread and return its owning handle. Never blocks
/// beyond the thread spawn itself.
pub fn spawn(config: SupervisorConfig) -> RemoteWatchSupervisor {
    let stopped = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Wake>();
    let handle = RemoteWatchSupervisor {
        stopped: Arc::clone(&stopped),
        tx: tx.clone(),
    };
    std::thread::Builder::new()
        .name("dv-remote-watch".into())
        .spawn(move || run(config, stopped, tx, rx))
        .expect("failed to spawn dv remote watch supervisor thread");
    handle
}

/// The supervisor state machine. Two states, looped forever until
/// shutdown:
///
///   - **Subscribed**: a live `watch/subscribe` exists; block on the wake
///     channel until the host dies (unregister, loop) or the handle drops
///     (unregister + best-effort unsubscribe, exit).
///   - **Degraded**: no subscription; once per tick, try to (re)acquire a
///     client and subscribe, and run the fallback poll if one exists.
fn run(
    mut config: SupervisorConfig,
    stopped: Arc<AtomicBool>,
    tx: mpsc::Sender<Wake>,
    rx: mpsc::Receiver<Wake>,
) {
    // Whether any arm (subscribe or fallback baseline) has happened yet —
    // once true, every future arm is a RE-arm and fires a synthetic.
    let mut armed_once = false;
    // Whether the degraded fallback is currently the active event source
    // (drives the enter-fallback transition exactly once per outage).
    let mut in_fallback = false;
    let mut logged_fallback = false;
    let mut subscribe_backoff: u32 = 0;

    'outer: loop {
        if stopped.load(Ordering::SeqCst) {
            return;
        }

        // ---- try to establish (or re-establish) a host subscription -----
        if subscribe_backoff == 0 {
            if let Some(client) = (config.client_source)() {
                match client.watch_subscribe(&config.root, config.kind) {
                    Ok(watch_id) => {
                        let cb_on_change = Arc::clone(&config.on_change);
                        let cb_stopped = Arc::clone(&stopped);
                        client.register_watch_callback(watch_id, move |_params| {
                            if !cb_stopped.load(Ordering::SeqCst) {
                                cb_on_change();
                            }
                        });
                        // Death listener AFTER a successful subscribe only —
                        // registering per attempt would accumulate listeners
                        // on a live client whose subscribe keeps failing. If
                        // the host died in the gap since `watch_subscribe`
                        // returned, `on_death` fires the listener inline and
                        // the recv loop below wakes immediately.
                        let death_tx = tx.clone();
                        client.on_death(move || {
                            let _ = death_tx.send(Wake::HostDied);
                        });

                        let synthetic = armed_once || config.synthetic_on_first_arm;
                        armed_once = true;
                        in_fallback = false;
                        if synthetic && !stopped.load(Ordering::SeqCst) {
                            (config.on_change)();
                        }

                        // ---- subscribed: wait for death or shutdown -----
                        loop {
                            match rx.recv() {
                                Ok(Wake::Shutdown) | Err(_) => {
                                    client.unregister_watch_callback(watch_id);
                                    let _ = client.watch_unsubscribe(watch_id);
                                    return;
                                }
                                Ok(Wake::HostDied) => {
                                    if !client.is_alive() {
                                        // Outage. No unsubscribe RPC — the
                                        // connection is gone; just stop
                                        // routing and go degrade/retry.
                                        client.unregister_watch_callback(watch_id);
                                        continue 'outer;
                                    }
                                    // Stale note from an older connection.
                                }
                            }
                        }
                    }
                    Err(err) => {
                        subscribe_backoff = SUBSCRIBE_RETRY_TICKS;
                        eprintln!(
                            "[dv remote watch] {} subscribe failed for {} (retrying in ~{}s): \
                             {err:#}",
                            config.kind,
                            config.label,
                            SUBSCRIBE_RETRY_TICKS as u64 * config.poll_interval.as_secs().max(1),
                        );
                    }
                }
            }
        } else {
            subscribe_backoff -= 1;
        }

        // ---- degraded: fallback poll (if any) + paced retry -------------
        if !in_fallback {
            in_fallback = true;
            if let Some(tick) = config.fallback_tick.as_mut() {
                if !logged_fallback && std::env::var("DV_HOST_DEBUG").as_deref() == Ok("1") {
                    // Same grep-able marker the pre-supervisor code emitted
                    // (plan §8 S4 gate: its ABSENCE in a healthy session's
                    // log proves the poll fallback never engaged).
                    eprintln!(
                        "[dv review watcher] starting WSL digest-poll fallback for {}",
                        config.label
                    );
                    logged_fallback = true;
                }
                // Baseline (first entry) or drift check (an outage entry
                // compares against the pre-outage baseline, catching store
                // changes that landed while we were subscribed — redundant
                // with events already delivered, but redundancy is the safe
                // direction). Collapsed with the arm synthetic into at most
                // ONE callback invocation.
                let drift = tick();
                let synthetic = armed_once || config.synthetic_on_first_arm;
                armed_once = true;
                if (drift || synthetic) && !stopped.load(Ordering::SeqCst) {
                    (config.on_change)();
                }
            }
        }

        match rx.recv_timeout(config.poll_interval) {
            Ok(Wake::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            // Stale death note (our current state has no live client) —
            // fall through to the next tick.
            Ok(Wake::HostDied) => {}
            Err(RecvTimeoutError::Timeout) => {}
        }
        if stopped.load(Ordering::SeqCst) {
            return;
        }
        if let Some(tick) = config.fallback_tick.as_mut()
            && tick()
            && !stopped.load(Ordering::SeqCst)
        {
            (config.on_change)();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if condition() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Collects `on_change` invocations; the `Arc` strong count doubles as
    /// the thread-liveness probe (the supervisor thread and any callback
    /// registrations hold clones — count back at 1 means everything
    /// released, i.e. the thread exited).
    fn counting_on_change() -> (Arc<dyn Fn() + Send + Sync>, Arc<AtomicU32>) {
        let count = Arc::new(AtomicU32::new(0));
        let count_clone = Arc::clone(&count);
        let on_change: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });
        (on_change, count)
    }

    #[test]
    fn fallback_mode_fires_first_arm_synthetic_then_change_events_then_stops_on_drop() {
        let (on_change, events) = counting_on_change();
        let changed = Arc::new(AtomicBool::new(false));
        let changed_clone = Arc::clone(&changed);

        let supervisor = spawn(SupervisorConfig {
            label: "test-repo".into(),
            root: "/tmp/never-used".into(),
            kind: "store",
            on_change: Arc::clone(&on_change),
            // No host ever — pure fallback, the hosts-disabled/CLI shape.
            client_source: Box::new(|| None),
            fallback_tick: Some(Box::new(move || {
                changed_clone.swap(false, Ordering::SeqCst)
            })),
            synthetic_on_first_arm: true,
            poll_interval: Duration::from_millis(20),
        });

        // First arm: exactly one synthetic (the tick baseline reports no
        // change; the synthetic is the arm signal `dv review wait` needs).
        assert!(
            wait_until(Duration::from_secs(2), || events.load(Ordering::SeqCst)
                == 1),
            "expected exactly one first-arm synthetic, got {}",
            events.load(Ordering::SeqCst)
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            events.load(Ordering::SeqCst),
            1,
            "quiet fallback ticks must not fire events"
        );

        // A fallback-observed change fires.
        changed.store(true, Ordering::SeqCst);
        assert!(
            wait_until(Duration::from_secs(2), || events.load(Ordering::SeqCst)
                == 2),
            "expected the fallback tick's change to fire an event"
        );

        // Drop stops delivery and the thread exits (releasing its clone of
        // `on_change`, observable via the strong count).
        drop(supervisor);
        assert!(
            wait_until(Duration::from_secs(2), || Arc::strong_count(&on_change)
                == 1),
            "supervisor thread should exit (and release on_change) after drop"
        );
        let after = events.load(Ordering::SeqCst);
        changed.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            events.load(Ordering::SeqCst),
            after,
            "no events may be delivered after drop"
        );
    }

    #[test]
    fn no_fallback_mode_stays_silent_and_exits_cleanly_on_drop() {
        let (on_change, events) = counting_on_change();
        let source_calls = Arc::new(AtomicU32::new(0));
        let source_calls_clone = Arc::clone(&source_calls);

        let supervisor = spawn(SupervisorConfig {
            label: "test-repo".into(),
            root: "/tmp/never-used".into(),
            kind: "worktree",
            on_change: Arc::clone(&on_change),
            client_source: Box::new(move || {
                source_calls_clone.fetch_add(1, Ordering::SeqCst);
                None
            }),
            fallback_tick: None,
            synthetic_on_first_arm: false,
            poll_interval: Duration::from_millis(20),
        });

        // It keeps retrying the source (paced), but never fires anything.
        assert!(
            wait_until(Duration::from_secs(2), || {
                source_calls.load(Ordering::SeqCst) >= 3
            }),
            "supervisor should keep retrying the client source"
        );
        assert_eq!(
            events.load(Ordering::SeqCst),
            0,
            "worktree mode with no host and no fallback must fire nothing"
        );

        let dropped_at = Instant::now();
        drop(supervisor);
        assert!(
            dropped_at.elapsed() < Duration::from_millis(500),
            "drop must not block on the supervisor thread"
        );
        assert!(
            wait_until(Duration::from_secs(2), || Arc::strong_count(&on_change)
                == 1),
            "supervisor thread should exit after drop"
        );
    }

    #[test]
    fn fallback_drift_on_outage_entry_collapses_into_one_event() {
        // The enter-fallback transition runs one tick (drift check) AND may
        // owe a synthetic — they must collapse into a single callback, not
        // two. Modeled by a tick that reports "changed" on its very first
        // call (as a post-outage drift would).
        let (on_change, events) = counting_on_change();
        let ticks = Arc::new(AtomicU32::new(0));
        let ticks_clone = Arc::clone(&ticks);

        let supervisor = spawn(SupervisorConfig {
            label: "test-repo".into(),
            root: "/tmp/never-used".into(),
            kind: "store",
            on_change: Arc::clone(&on_change),
            client_source: Box::new(|| None),
            fallback_tick: Some(Box::new(move || {
                ticks_clone.fetch_add(1, Ordering::SeqCst) == 0
            })),
            synthetic_on_first_arm: true,
            poll_interval: Duration::from_millis(20),
        });

        assert!(
            wait_until(Duration::from_secs(2), || ticks.load(Ordering::SeqCst) >= 3),
            "fallback should keep ticking"
        );
        assert_eq!(
            events.load(Ordering::SeqCst),
            1,
            "drift + synthetic on the same arm must fire exactly one event"
        );
        drop(supervisor);
    }
}
