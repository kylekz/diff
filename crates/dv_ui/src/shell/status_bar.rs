//! The status bar (DESIGN §2, §4.1): where the review's repository lives and how dv reaches it
//! (the location and the backend's state), the review's status, round and threads, and the key
//! that finishes the round.
//!
//! A remote repository (a WSL distro's, PLAN §5.2) is named by its connection, whose state
//! comes first: "Ubuntu · installing dv (3.1 / 7.4 MB)…" and "· connecting…" in `fg3`,
//! "· reconnecting…" in yellow, "· stopped" (the distro was shut down; dv starts it only when
//! asked) and "· disconnected" in red (the backend-down banner says why and offers Start or
//! Retry); once it is connected, the review's own state as for a local one. A connection closed
//! for idleness (PLAN §5.1: an open review whose watch couldn't start holds none) is up too,
//! since the next call starts it again: the review's own state shows.
//!
//! At the right, when an update of dv is downloaded (PLAN D9, `crate::services::Updates`),
//! "dv <version> is ready" in `accentText` and **Restart to update**, which applies it, asking
//! first over the status bar when that would close other dv processes or this window's unsent
//! text; a restart that failed reads "Update failed · details" in `red` (why in its tooltip) with
//! **Retry**.

use dv_backend::ConnectionState;
use dv_core::Location;
use dv_core::review::ThreadCounts;
use gpui::{
    AnyElement, App, Context, Hsla, InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, anchored, deferred, div, point,
    prelude::FluentBuilder as _, px,
};

use super::Shell;
use crate::automation::AutomationTargetExt as _;
use crate::components::{icon, kbd, menu_panel, small_button, status_label, tooltip};
use crate::icons::DvIcon;
use crate::keymap;
use crate::services::ActiveServices as _;
use crate::theme::{DvTheme, StyledText as _};

/// How the backend is doing, as the status bar's dot shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// Nothing open.
    Idle,
    /// Opened and following changes.
    Live,
    /// Opened, but the watch can't follow the worktree (the review refreshes on focus), or the
    /// connection to its remote backend is reconnecting.
    Degraded,
    /// It couldn't be opened, its last refresh failed, or its connection failed.
    Failing,
}

impl Health {
    /// `"idle"`, `"live"`, `"degraded"` or `"failing"`, for automation state.
    pub fn key(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Live => "live",
            Self::Degraded => "degraded",
            Self::Failing => "failing",
        }
    }

    fn color(self, theme: &DvTheme) -> Hsla {
        let c = &theme.colors;
        match self {
            Self::Idle => c.fg3,
            Self::Live => c.green,
            Self::Degraded => c.yellow,
            Self::Failing => c.red,
        }
    }
}

/// What the status bar says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusLine {
    pub health: Health,
    /// "Local · watching for changes", "Ubuntu · can't be opened", "Local · api · no review
    /// open" (a repository that opened, beside the banner of what failed in it), "No repo".
    pub location: String,
    /// "Changes requested · round 2 · 3 open · 1 resolved", with a review open; an interdiff
    /// adds "· since round 1".
    pub review: Option<String>,
    /// Whether "Ctrl Enter finish review" shows (an open review that isn't finished).
    pub finish: bool,
}

/// `Local`, or a WSL repository's distro as WSL spells it, as far as dv has seen
/// (`Locations::canonical`).
fn place(location: &Location, cx: &App) -> String {
    match cx.services().locations.read(cx).canonical(location) {
        Location::Local(_) => "Local".to_string(),
        Location::Wsl { distro, .. } => distro,
    }
}

/// How a connection that isn't up reads, and its dot; `None` while it is up: connected, or
/// closed for idleness, which the next call starts again (module docs).
fn connection_line(state: &ConnectionState) -> Option<(Health, String)> {
    Some(match state {
        ConnectionState::Connected | ConnectionState::Idle => return None,
        ConnectionState::Connecting => (Health::Idle, "connecting…".to_string()),
        ConnectionState::Installing { done, total } => (
            Health::Idle,
            format!(
                "installing dv ({} / {} MB)…",
                megabytes(*done),
                megabytes(*total)
            ),
        ),
        ConnectionState::Reconnecting { .. } => (Health::Degraded, "reconnecting…".to_string()),
        ConnectionState::Stopped => (Health::Failing, "stopped".to_string()),
        ConnectionState::Failed { .. } => (Health::Failing, "disconnected".to_string()),
    })
}

/// How an open review's backend reads, and its dot: its connection's state while that isn't up
/// (`connection`, `None` for a review opened in process), else the review's own: its refresh
/// failed (`failed`), its watch can't follow the worktree (`unwatched`, it refreshes when the
/// window is focused), or it is watching for changes.
fn review_line(
    connection: Option<&ConnectionState>,
    failed: bool,
    unwatched: bool,
) -> (Health, String) {
    match connection.and_then(connection_line) {
        Some(line) => line,
        None if failed => (Health::Failing, "refresh failed".into()),
        None if unwatched => (Health::Degraded, "refreshes when focused".into()),
        None => (Health::Live, "watching for changes".into()),
    }
}

/// `bytes` in megabytes (10^6) to one decimal: `3.1`.
fn megabytes(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1_000_000.0)
}

/// "3 open · 1 addressed · 1 resolved", leaving out what is zero; "no comments" when all are.
/// Then the agent's annotations (PLAN §19.3), "· 5 annotations", "· 5 annotations hidden"
/// while `A` hides them (`shown` false).
pub fn thread_counts(counts: ThreadCounts, shown: bool) -> String {
    let parts: Vec<String> = [
        (counts.open, "open"),
        (counts.addressed, "addressed"),
        (counts.resolved, "resolved"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, what)| format!("{count} {what}"))
    .collect();
    let mut text = if parts.is_empty() {
        "no comments".to_string()
    } else {
        parts.join(" · ")
    };
    if counts.annotation > 0 {
        let noun = if counts.annotation == 1 {
            "annotation"
        } else {
            "annotations"
        };
        let hidden = if shown { "" } else { " hidden" };
        text.push_str(&format!(" · {} {noun}{hidden}", counts.annotation));
    }
    text
}

impl Shell {
    /// What the status bar says now.
    pub fn status_line(&self, cx: &App) -> StatusLine {
        if let Some(opening) = &self.opening {
            // Opening a WSL repository may start its distro and install dv there first.
            let connection = cx
                .services()
                .locations
                .read(cx)
                .connection_for(&opening.location);
            let (health, state) = connection
                .and_then(|connection| match connection.state() {
                    state @ (ConnectionState::Installing { .. } | ConnectionState::Connecting) => {
                        connection_line(&state)
                    }
                    _ => None,
                })
                .unwrap_or((Health::Idle, "opening…".to_string()));
            return StatusLine {
                health,
                location: format!("{} · {state}", place(&opening.location, cx)),
                review: None,
                finish: false,
            };
        }
        if let Some(session) = &self.session {
            let session = session.read(cx);
            let (health, state) = review_line(
                session.connection_state(),
                session.error().is_some(),
                session.watch_notice().is_some(),
            );
            let review = session.review();
            // An interdiff says so (PLAN §2.3).
            let since = session
                .comparison()
                .round()
                .map(|n| format!(" · since round {n}"))
                .unwrap_or_default();
            // A finished review names the round that finished it, not one that never opens.
            return StatusLine {
                health,
                location: format!("{} · {state}", session.backend_place()),
                review: Some(format!(
                    "{} · round {} · {}{since}",
                    status_label(review.status),
                    review.shown_round(),
                    thread_counts(review.summary().threads, session.annotations_shown()),
                )),
                finish: !session.is_read_only(),
            };
        }
        // An open that failed: because its repository can't be opened, or inside a
        // repository that did open (a pull request GitHub wouldn't tell about, a review
        // that isn't there), which then reads as any repository with no review open.
        if let Some(location) = self.banner.as_ref().and_then(|b| b.location.as_ref()) {
            let locations = cx.services().locations.read(cx);
            let Some(backend) = locations.cached(&locations.canonical(location)) else {
                return StatusLine {
                    health: Health::Failing,
                    location: format!("{} · can't be opened", place(location, cx)),
                    review: None,
                    finish: false,
                };
            };
            let (health, state) = locations
                .connection_for(location)
                .and_then(|connection| connection_line(&connection.state()))
                .unwrap_or((Health::Idle, "no review open".to_string()));
            return StatusLine {
                health,
                location: format!(
                    "{} · {} · {state}",
                    place(location, cx),
                    backend.repo().location.short_name()
                ),
                review: None,
                finish: false,
            };
        }
        StatusLine {
            health: Health::Idle,
            location: "No repo".to_string(),
            review: None,
            finish: false,
        }
    }

    /// The update downloaded and ready, with Restart to update, or the restart that failed,
    /// with Retry (module docs), if there is one.
    fn render_update_item(&self, theme: &DvTheme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let updates = cx.services().updates.read(cx);
        let version = updates.ready_version()?.to_string();
        let failure = updates.failure().map(str::to_string);
        let c = &theme.colors;
        let (label, tip) = match &failure {
            Some(_) => ("Retry", format!("Try installing dv {version} again")),
            None => (
                "Restart to update",
                format!("Quit dv, install {version} and start it again"),
            ),
        };
        let restart = small_button("update-restart", label, theme)
            .h(theme.sizes.control_xsmall_height)
            .tooltip(tooltip(tip))
            .on_click(cx.listener(|this, _, _, cx| this.request_update_restart(cx)))
            .automation_target("status/update/restart");
        let line = match failure {
            Some(error) => div()
                .id("update-failed")
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_color(c.red)
                .child(icon(DvIcon::TriangleAlert, px(13.), c.red))
                .child(
                    div()
                        .text_spec(&theme.text.secondary)
                        .child("Update failed · details"),
                )
                .tooltip(tooltip(format!("dv {version} wasn't installed: {error}")))
                .automation_target("status/update/failed")
                .into_any_element(),
            None => div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_color(c.accent_text)
                .child(icon(DvIcon::ArrowUp, px(13.), c.accent_text))
                .child(
                    div()
                        .text_spec(&theme.text.secondary)
                        .child(format!("dv {version} is ready")),
                )
                .into_any_element(),
        };
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(line)
                .child(restart)
                .automation_target("status/update"),
        )
    }

    /// Restart to update (or Retry): with what this window holds, which the update service
    /// asks about before it restarts (`Updates::request_restart`).
    fn request_update_restart(&mut self, cx: &mut Context<Self>) {
        let unsaved = self.unsaved_text(cx);
        let updates = cx.services().updates.clone();
        updates.update(cx, |updates, cx| updates.request_restart(unsaved, cx));
    }

    /// The confirmation Restart to update shows when applying would close something (module
    /// docs): what will close, **Not now** and **Restart anyway**, over the status bar's right
    /// end, as a language server's details are.
    pub(super) fn render_update_confirm(
        &self,
        window: &Window,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let updates = cx.services().updates.clone();
        let read = updates.read(cx);
        let closes = read.confirm()?.to_vec();
        let version = read.ready_version().unwrap_or_default().to_string();
        let c = &theme.colors;
        let not_now = {
            let updates = updates.clone();
            small_button("update-not-now", "Not now", theme)
                .on_click(move |_, _, cx| updates.update(cx, |u, cx| u.cancel_restart(cx)))
                .automation_target("status/update/not_now")
        };
        let anyway = small_button("update-restart-anyway", "Restart anyway", theme)
            .on_click(move |_, _, cx| updates.update(cx, |u, cx| u.confirm_restart(cx)))
            .automation_target("status/update/restart_anyway");
        let panel = menu_panel("update-confirm", theme)
            .w(px(420.))
            .p(theme.spacing.lg)
            .flex()
            .flex_col()
            .gap(theme.spacing.sm)
            .text_spec(&theme.text.body)
            .child(
                div()
                    .text_spec(&theme.text.title)
                    .child(format!("Restart into dv {version}?")),
            )
            .child(
                div()
                    .text_color(c.fg2)
                    .child("Installing the update now closes:"),
            )
            .children(closes.into_iter().map(|line| {
                div()
                    .flex()
                    .gap(theme.spacing.sm)
                    .text_color(c.fg)
                    .child(div().text_color(c.fg3).child("•"))
                    .child(div().flex_1().min_w_0().child(line))
            }))
            .child(
                div()
                    .mt(theme.spacing.sm)
                    .flex()
                    .justify_end()
                    .gap(theme.spacing.sm)
                    .child(not_now)
                    .child(anyway),
            );
        let bottom = window.viewport_size().height - theme.sizes.status_bar_height;
        let right = window.viewport_size().width - theme.spacing.lg;
        Some(
            deferred(
                anchored()
                    .position(point(right, bottom - px(6.)))
                    .anchor(gpui::Anchor::BottomRight)
                    .snap_to_window_with_margin(px(8.))
                    .child(panel.automation_target("status/update/confirm")),
            )
            .with_priority(super::render::MENU_PRIORITY)
            .into_any_element(),
        )
    }

    pub(super) fn render_status_bar(&self, theme: &DvTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let line = self.status_line(cx);
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.))
            .h(theme.sizes.status_bar_height)
            .px(theme.spacing.lg)
            .bg(c.chrome)
            .border_t_1()
            .border_color(c.line)
            .text_spec(&theme.text.secondary)
            .text_color(c.fg2)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.sm)
                    .child(
                        div()
                            .size(px(7.))
                            .rounded_full()
                            .bg(line.health.color(theme)),
                    )
                    // A connection in trouble says so in its colour too ("· reconnecting…" in
                    // `yellow`, "· disconnected" in `red`, DESIGN §4.4); a healthy one in `fg2`.
                    .when(
                        matches!(line.health, Health::Degraded | Health::Failing),
                        |this| this.text_color(line.health.color(theme)),
                    )
                    .child(line.location)
                    .automation_target("statusbar/location"),
            )
            .when_some(line.review, |this, review| {
                this.child(div().flex_none().text_color(c.fg3).child("|"))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(review)
                            .automation_target("statusbar/review"),
                    )
            })
            .child(div().flex_1())
            .children(self.render_update_item(theme, cx))
            .children(self.render_skill_notice(theme, cx))
            .children(self.render_server_items(theme, cx))
            .when(line.finish, |this| {
                this.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(kbd(keymap::label("secondary-enter"), theme))
                        .child("finish review")
                        .automation_target("statusbar/finish"),
                )
            })
            .automation_target("statusbar")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_that_isnt_up_says_how_it_stands() {
        assert_eq!(connection_line(&ConnectionState::Connected), None);
        assert_eq!(connection_line(&ConnectionState::Idle), None);
        assert_eq!(
            connection_line(&ConnectionState::Connecting),
            Some((Health::Idle, "connecting…".into()))
        );
        assert_eq!(
            connection_line(&ConnectionState::Installing {
                done: 3_100_000,
                total: 7_430_000,
            }),
            Some((Health::Idle, "installing dv (3.1 / 7.4 MB)…".into()))
        );
        assert_eq!(
            connection_line(&ConnectionState::Reconnecting { attempt: 2 }),
            Some((Health::Degraded, "reconnecting…".into()))
        );
        assert_eq!(
            connection_line(&ConnectionState::Stopped),
            Some((Health::Failing, "stopped".into()))
        );
        assert_eq!(
            connection_line(&ConnectionState::Failed {
                error: "dv died".into()
            }),
            Some((Health::Failing, "disconnected".into()))
        );
    }

    #[test]
    fn an_open_review_whose_connection_idled_out_reads_as_its_own_state() {
        // Its watch couldn't start, so nothing kept the server: the next call starts it again,
        // and meanwhile the review refreshes when focused, which is what the bar says.
        let idle = Some(&ConnectionState::Idle);
        assert_eq!(
            review_line(idle, false, true),
            (Health::Degraded, "refreshes when focused".into())
        );
        assert_eq!(
            review_line(idle, false, false),
            (Health::Live, "watching for changes".into())
        );
        assert_eq!(
            review_line(idle, true, true),
            (Health::Failing, "refresh failed".into())
        );
        // A connection that is down still comes first.
        assert_eq!(
            review_line(
                Some(&ConnectionState::Reconnecting { attempt: 1 }),
                false,
                true
            ),
            (Health::Degraded, "reconnecting…".into())
        );
        assert_eq!(
            review_line(Some(&ConnectionState::Connecting), false, true),
            (Health::Idle, "connecting…".into())
        );
        assert_eq!(
            review_line(None, false, true),
            (Health::Degraded, "refreshes when focused".into())
        );
    }

    #[test]
    fn thread_counts_leave_out_zeros() {
        let counts = |open, addressed, resolved| ThreadCounts {
            open,
            addressed,
            resolved,
            annotation: 0,
        };
        assert_eq!(thread_counts(counts(0, 0, 0), true), "no comments");
        assert_eq!(thread_counts(counts(3, 0, 1), true), "3 open · 1 resolved");
        assert_eq!(
            thread_counts(counts(1, 2, 3), true),
            "1 open · 2 addressed · 3 resolved"
        );
        // The agent's annotations, apart (PLAN §19.3).
        let annotated = |annotation| ThreadCounts {
            annotation,
            ..counts(0, 0, 0)
        };
        assert_eq!(
            thread_counts(annotated(5), true),
            "no comments · 5 annotations"
        );
        assert_eq!(
            thread_counts(annotated(1), false),
            "no comments · 1 annotation hidden"
        );
    }
}
