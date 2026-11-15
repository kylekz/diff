//! A pick that waits for its connection (PLAN §5.2, DESIGN §4.4): a review picked in the
//! switcher (or the source picker, or `open_review`) is never lost and never reported twice.
//!
//! `Reviews::open` already waits out a reconnect that a lost connection under the open starts
//! and runs the open once more. When the open still fails because of its connection — lost
//! under it again (`Failure::disconnected`), or the connection failed or found its distro
//! stopped — the shell keeps the pick, at most one, as the **pending pick** of that connection:
//! - when the open review goes through the same connection, the main area raises nothing: its
//!   status bar and backend-down banner already say what is wrong;
//! - otherwise the main area's banner says "couldn't open <repo>" (the connection's error and
//!   diagnostics in Details), whose Retry runs the pick again;
//! - when the connection comes back (`LocationsEvent::ConnectionChanged { up: true }`, or it is
//!   back already when the open fails), the pick runs again by itself, its banner going.
//!
//! A newer pick replaces it, and the Dismiss of its own banner drops it (another banner's
//! Dismiss leaves it waiting). Running again by itself, it keeps open menus and the keyboard
//! where they are, and it never replaces a review that holds unsaved text: while the "Finish
//! round" sheet is open or a composer holds a draft, it waits on, and runs once the sheet
//! closes or the draft is posted or cancelled (its connection still up: an idle-closed one is,
//! since the next call starts it; one that went down again keeps it waiting). Its banner's
//! Retry is the user's own pick and runs at once. A pick whose open kills its connection every
//! time runs again by itself at most [`AUTOMATIC_RUNS`] times; then it fails as any open does,
//! in the main area's banner. Any other failure of an open is that banner too, with Retry
//! unless running it again can't end differently (`Failure::retryable`).

use std::sync::Arc;

use dv_backend::{Connection, ConnectionKey, ConnectionState};
use dv_core::Location;
use gpui::{App, Context, Window};

use super::{MainBanner, Shell};
use crate::failure::Failure;
use crate::services::ActiveServices as _;
use crate::services::reviews::OpenRequest;

/// How many times a pick runs again by itself (module docs).
pub const AUTOMATIC_RUNS: u32 = 3;

/// A pick that waits for its connection (module docs).
#[derive(Clone)]
pub struct PendingPick {
    pub location: Location,
    pub request: OpenRequest,
    key: ConnectionKey,
    connection: Arc<Connection>,
    /// How many times it has run again by itself.
    runs: u32,
}

impl PendingPick {
    /// The connection it waits for.
    pub fn connection(&self) -> &Arc<Connection> {
        &self.connection
    }
}

impl Shell {
    /// The pick waiting for its connection, if any (module docs).
    pub fn pending_pick(&self) -> Option<&PendingPick> {
        self.pending.as_ref()
    }

    /// The open of `location`'s `request` failed with `failure`, after it ran again by itself
    /// `runs` times: kept as the pending pick when its connection is the trouble, else (and
    /// past [`AUTOMATIC_RUNS`]) the main area's banner with Retry (module docs).
    pub(super) fn open_failed(
        &mut self,
        location: Location,
        request: OpenRequest,
        failure: &Failure,
        runs: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let waits = {
            let locations = cx.services().locations.read(cx);
            let down = |connection: &Arc<Connection>| {
                failure.disconnected
                    || matches!(
                        connection.state(),
                        ConnectionState::Failed { .. } | ConnectionState::Stopped
                    )
            };
            locations
                .connection_for(&location)
                .filter(down)
                .zip(locations.key_for(&location))
                .filter(|_| runs < AUTOMATIC_RUNS)
        };
        // Retry is offered for what running the open again can fix.
        let banner = |pick| MainBanner {
            failure: failure.clone(),
            retry: failure
                .retryable
                .then(|| (location.clone(), request.clone())),
            location: Some(location.clone()),
            expanded: false,
            pick,
            unread: None,
        };
        let Some((connection, key)) = waits else {
            self.banner = Some(banner(false));
            return;
        };
        // The open review's banner and status bar report this connection already.
        let reported = self
            .session
            .as_ref()
            .and_then(|session| session.read(cx).connection().cloned())
            .is_some_and(|open| Arc::ptr_eq(&open, &connection));
        if !reported {
            self.banner = Some(banner(true));
        }
        let back = is_up(&connection.state());
        self.pending = Some(PendingPick {
            location,
            request,
            key,
            connection,
            runs,
        });
        // Back already: its return was heard before the open failed.
        if back {
            self.run_pending(window, cx);
        }
    }

    /// The connection of `key` is back: its pending pick runs again (module docs).
    pub(super) fn connection_back(
        &mut self,
        key: &ConnectionKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| &pending.key == key)
        {
            self.run_pending(window, cx);
        }
    }

    /// Runs the pending pick again by itself, when its connection is up and the open review
    /// holds no unsaved text ([`Shell::holds_unsaved_text`]): menus stay open and the keyboard
    /// where it is. The shell calls it again when the connection comes back, the sheet closes
    /// and a composer goes.
    pub(super) fn run_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.holds_unsaved_text(cx) {
            return;
        }
        let Some(pending) = self
            .pending
            .take_if(|pending| is_up(&pending.connection.state()))
        else {
            return;
        };
        self.begin_open(
            pending.location,
            pending.request,
            pending.runs + 1,
            window,
            cx,
        )
        .detach();
    }

    /// Whether the open review holds text the reviewer hasn't saved, which a pick running
    /// again by itself waits for (module docs): the "Finish round" sheet open over it (a round
    /// being finished included), or a composer with a draft.
    fn holds_unsaved_text(&self, cx: &App) -> bool {
        let unsaved = self.unsaved_text(cx);
        unsaved.sheet || unsaved.draft
    }

    /// The unsaved text [`Self::holds_unsaved_text`] reads, apart: what Restart to update asks
    /// about before an update closes this window (`crate::services::updates`).
    pub(super) fn unsaved_text(&self, cx: &App) -> crate::services::Unsaved {
        crate::services::Unsaved {
            draft: self.diff_pane.read(cx).has_draft(cx) || self.file_view.read(cx).has_draft(cx),
            sheet: self.sheet.is_some(),
        }
    }

    /// The main area's banner's Dismiss: it goes, and so does the pick it reports, when it is
    /// the pick's own; any other banner leaves the pick waiting.
    pub fn dismiss_banner(&mut self, cx: &mut Context<Self>) {
        if self.banner.take().is_some_and(|banner| banner.pick) {
            self.pending = None;
        }
        cx.notify();
    }
}

/// Whether a pick may run on a connection in `state`: connected, or closed for idleness, which
/// the next call starts again (PLAN §5.1).
fn is_up(state: &ConnectionState) -> bool {
    matches!(state, ConnectionState::Connected | ConnectionState::Idle)
}
