//! Round histories in the session (PLAN §8 "Round history"): what a closed round's snapshot
//! says about the threads anchored in it that the current text alone can't place — a new-side
//! thread whose file isn't listed, or that relocation calls outdated
//! (`dv_core::review::read_round_history`, read in the background over the session's
//! `GitRepo`, so through the WSL backend too).
//!
//! **Which rounds.** Only rounds some thread needs, for the paths of those threads: a thread
//! whose round's history isn't read for its path is `Pending` (`dv_core::review::place_thread`),
//! never outdated first and then moved, and the session asks for it when, placed without any
//! history, it would be outdated ([`ReviewSession::ensure_histories`], after a listing, a
//! file's load, a full read of a file and a fold). A thread placed before keeps that
//! placement while only its history is missing (`placement`), so a card on screen, and a reply
//! being typed in it, stay until the history lands. A read covers the paths live threads of
//! its round still ask for; one asked for while another runs follows it, with the paths
//! asked for meanwhile, so reads never pile up.
//!
//! **Keeping.** A history is kept while the target resolves to the same sides, the review's
//! files (paths and statuses) are the same and no refresh touches a path it read (its files,
//! what they are called now, the candidates a deleted file was compared with); a rescan or a
//! git change reads it again. While it is read again the one it had keeps placing its threads,
//! so nothing they show flickers; a read again that fails after the basis changed drops it, so
//! nothing is placed against a history that no longer holds. A snapshot never changes, so
//! a read again takes the snapshot's texts from the history it replaces and reads only the
//! interdiff's list and the pairing candidates.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use dv_core::DiffSpec;
use dv_core::diff::{FileEntry, FileStatus};
use dv_core::review::{
    AnchorSide, History, RoundHistory, Thread, ThreadFile, has_side, place_thread,
    read_round_history,
};
use gpui::{AppContext as _, Context, Task};

use super::load::Batch;
use super::{Card, ReviewSession, SessionEvent};
use crate::failure::Failure;

/// One round's history as the session has it.
pub(super) struct RoundRead {
    /// The paths read, or being read.
    paths: BTreeSet<String>,
    /// The last read that landed; it keeps placing threads while another runs.
    read: Option<Arc<RoundHistory>>,
    /// A read under way: which, and the paths it reads.
    loading: Option<Loading>,
    /// The last read failed (not a lost connection): its threads are placed without it.
    failed: bool,
    /// What `read` was read against: the target resolved, and the review's files.
    basis: Option<Basis>,
    /// Another read was asked for while one ran (more paths, or a basis that changed): it
    /// starts when that one lands.
    again: bool,
}

struct Loading {
    token: u64,
    basis: Basis,
    _task: Task<()>,
}

/// What a history was read against.
#[derive(Clone, PartialEq, Eq)]
struct Basis {
    resolved: DiffSpec,
    files: Vec<(String, FileStatus)>,
}

/// A history as placement's cache key holds it (`placement`): which read it is.
#[derive(Clone, Debug)]
pub(super) enum HistoryKey {
    None,
    Pending,
    Read(Arc<RoundHistory>),
}

impl PartialEq for HistoryKey {
    fn eq(&self, other: &HistoryKey) -> bool {
        match (self, other) {
            (Self::None, Self::None) | (Self::Pending, Self::Pending) => true,
            (Self::Read(a), Self::Read(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl ReviewSession {
    /// The closed round whose history `thread` may be placed with: its anchor's, on the new
    /// side (a snapshot is a new side).
    fn history_round(&self, thread: &Thread) -> Option<u32> {
        let anchor = &thread.anchor;
        (anchor.side == AnchorSide::New
            && anchor.round >= 1
            && anchor.round <= self.review.closed_rounds())
        .then_some(anchor.round)
    }

    /// `thread`'s round history as the session has it (module docs).
    pub(super) fn history_for(&self, thread: &Thread) -> History<'_> {
        let Some(round) = self.history_round(thread) else {
            return History::None;
        };
        let Some(state) = self.histories.get(&round) else {
            return History::Pending;
        };
        let path = thread.anchor.path.as_str();
        match &state.read {
            Some(read) if read.file(path).is_some() => History::Read(read),
            // A read that failed leaves the threads it didn't cover unplaced by it.
            _ if state.failed && state.loading.is_none() => History::None,
            Some(read) => History::Read(read),
            None => History::Pending,
        }
    }

    /// [`ReviewSession::history_for`] as placement's cache key holds it.
    pub(super) fn history_key(&self, thread: &Thread) -> HistoryKey {
        match self.history_for(thread) {
            History::None => HistoryKey::None,
            History::Pending => HistoryKey::Pending,
            History::Read(_) => self
                .history_round(thread)
                .and_then(|round| self.histories.get(&round))
                .and_then(|state| state.read.clone())
                .map_or(HistoryKey::Pending, HistoryKey::Read),
        }
    }

    /// Whether `thread`, waiting, waits for its round's history: placed without one, its file
    /// isn't listed or relocation calls it outdated (module docs). Its file's text not there
    /// yet, it doesn't: it is asked again once that lands.
    fn wants_history(&self, thread: &Thread) -> bool {
        let entry = match self.thread_file_with(thread, History::None) {
            ThreadFile::Found(entry) => entry,
            ThreadFile::Gone => return true,
            ThreadFile::Pending(_) => return false,
        };
        if !has_side(entry, thread.anchor.side) {
            return true;
        }
        // A comment on the whole file stands in its file once found: no text to trace.
        if thread.anchor.is_file_level() {
            return false;
        }
        let drawn = self.draws(Card::Local(thread), entry);
        let Some(basis) = self.basis(&entry.path, thread.anchor.side, drawn) else {
            return false;
        };
        place_thread(
            thread,
            std::slice::from_ref(entry),
            History::None,
            |entry, side| self.lines_of(&entry.path, side, basis),
        )
        .is_outdated()
    }

    /// Starts reading the round histories threads wait for (module docs, "Which rounds"):
    /// every closed round some new-side thread needs whose history isn't read, or being read,
    /// for its path. Blocking work, so in the background.
    pub(super) fn ensure_histories(&mut self, cx: &mut Context<Self>) {
        if !self.listed || self.resolved.is_none() {
            return;
        }
        let mut wanted: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
        for thread in self.review.live_threads() {
            let Some(round) = self.history_round(thread) else {
                continue;
            };
            let path = &thread.anchor.path;
            let covered = self.histories.get(&round).is_some_and(|state| {
                state.paths.contains(path) && (state.loading.is_some() || !state.failed)
            });
            let waiting = self.placement(thread).is_pending() || self.placement_held(thread);
            if covered || !waiting || !self.wants_history(thread) {
                continue;
            }
            wanted.entry(round).or_default().insert(path.clone());
        }
        for (round, paths) in wanted {
            self.start_history(round, paths, cx);
        }
    }

    /// Reads round `round`'s history for `paths` and the paths it covers already that live
    /// threads of the round still name; with a read under way, once that lands.
    fn start_history(&mut self, round: u32, paths: BTreeSet<String>, cx: &mut Context<Self>) {
        let (Some(resolved), Some(closed)) = (
            self.resolved.clone(),
            self.review.rounds.get(round as usize - 1).cloned(),
        ) else {
            return;
        };
        let live: BTreeSet<String> = self
            .review
            .live_threads()
            .filter(|thread| self.history_round(thread) == Some(round))
            .map(|thread| thread.anchor.path.clone())
            .collect();
        let state = self.histories.entry(round).or_insert_with(|| RoundRead {
            paths: BTreeSet::new(),
            read: None,
            loading: None,
            failed: false,
            basis: None,
            again: false,
        });
        state.paths.extend(paths);
        state.paths.retain(|path| live.contains(path));
        if state.loading.is_some() || self.histories_held {
            state.again = true;
            return;
        }
        state.again = false;
        let paths: Vec<String> = state.paths.iter().cloned().collect();
        let previous = state.read.clone();
        self.next_token += 1;
        let token = self.next_token;
        let basis = Basis {
            resolved: resolved.clone(),
            files: files_key(&self.target_index().files),
        };
        let repo = self.repo.clone();
        let job = cx.background_spawn(async move {
            read_round_history(&repo, &resolved, &closed, &paths, previous.as_deref())
        });
        let task = cx.spawn(async move |this, cx| {
            let read = job.await;
            this.update(cx, |this, cx| this.finish_history(round, token, read, cx))
                .ok();
        });
        if let Some(state) = self.histories.get_mut(&round) {
            state.loading = Some(Loading {
                token,
                basis,
                _task: task,
            });
        }
    }

    fn finish_history(
        &mut self,
        round: u32,
        token: u64,
        read: Result<RoundHistory, dv_core::diff::DiffError>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.histories.get_mut(&round) else {
            return;
        };
        if state
            .loading
            .as_ref()
            .is_none_or(|loading| loading.token != token)
        {
            return;
        }
        let Some(loading) = state.loading.take() else {
            return;
        };
        let again = std::mem::take(&mut state.again);
        match read {
            Ok(read) => {
                state.read = Some(Arc::new(read));
                state.basis = Some(loading.basis);
                state.failed = false;
            }
            Err(error) => {
                // Read against another basis, the history it had no longer holds.
                if state.basis.as_ref() != Some(&loading.basis) {
                    state.read = None;
                    state.basis = None;
                }
                let failure = Failure::from(&error);
                if self.lost_connection(&failure).is_some() {
                    // The reconnect's rescan reads it again; meanwhile its threads wait, or
                    // keep the history they had.
                    cx.emit(SessionEvent::HistoryLoaded);
                    cx.notify();
                    return;
                }
                log::warn!(
                    "review {}: couldn't read round {round}'s history: {error}",
                    self.review.id
                );
                if let Some(state) = self.histories.get_mut(&round) {
                    state.failed = true;
                }
            }
        }
        if again {
            self.start_history(round, BTreeSet::new(), cx);
        }
        // The files its threads show in now load, and those it can't place against what the
        // stream shows are read in full.
        self.requeue_wanted();
        self.pump(cx);
        self.ensure_review_texts(cx);
        self.ensure_side_reads(cx);
        cx.emit(SessionEvent::HistoryLoaded);
        cx.notify();
    }

    /// After a listing (module docs, "Keeping"): reads again every history whose basis
    /// changed, or that `batch` touches; the one it had keeps placing its threads meanwhile.
    pub(super) fn refresh_histories(&mut self, batch: &Batch, cx: &mut Context<Self>) {
        let Some(resolved) = self.resolved.clone() else {
            return;
        };
        let now = Basis {
            resolved,
            files: files_key(&self.target_index().files),
        };
        let stale: Vec<(u32, BTreeSet<String>)> =
            self.histories
                .iter()
                .filter(|(_, state)| {
                    let basis = state
                        .loading
                        .as_ref()
                        .map(|loading| &loading.basis)
                        .or(state.basis.as_ref());
                    let touched = state.read.as_ref().is_some_and(|read| {
                        batch.paths.iter().any(|path| {
                            read.read_paths().contains(path) || state.paths.contains(path)
                        })
                    });
                    batch.rescan || batch.git || basis != Some(&now) || touched
                })
                .map(|(&round, state)| (round, state.paths.clone()))
                .collect();
        for (round, paths) in stale {
            self.start_history(round, paths, cx);
        }
    }

    /// Whether a history read is under way (automation's `settled`).
    pub(super) fn histories_loading(&self) -> bool {
        self.histories.values().any(|state| state.loading.is_some())
    }

    /// Holds history reads until `held` is false again, then starts the ones asked for
    /// meanwhile (tests: what the stream shows while a history is missing).
    #[cfg(test)]
    pub(crate) fn hold_histories(&mut self, held: bool, cx: &mut Context<Self>) {
        self.histories_held = held;
        if held {
            return;
        }
        let waiting: Vec<u32> = self
            .histories
            .iter()
            .filter(|(_, state)| state.again)
            .map(|(&round, _)| round)
            .collect();
        for round in waiting {
            self.start_history(round, BTreeSet::new(), cx);
        }
    }
}

/// The review's files as a history's basis holds them: paths and statuses.
fn files_key(files: &[FileEntry]) -> Vec<(String, FileStatus)> {
    files
        .iter()
        .map(|entry| (entry.path.clone(), entry.status))
        .collect()
}
