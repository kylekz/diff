//! The onboarding overlay's own state (`shell.rs` owns the background
//! dispatch, the boot-storm-safe distro gating, and the render tree — same
//! split as the settings panel/theme picker: this module is just the data,
//! not the widget). Shown on true first run (`crate::setup::SetupState`)
//! and, later in a session, auto-surfaced whenever a per-launch or
//! `open_review`-triggered consistency check finds something that actually
//! needs a human (see `shell.rs`'s `AppShell::apply_consistency_report`).
//!
//! Rows map 1:1 onto `dv_core::provision::ComponentReport`s. A fresh
//! `ConsistencyReport` is MERGED into the existing row set by title (unique
//! per component — `"<base title>"` for `gh`, `"<base title> — <distro>"`
//! for the other three; see `shell.rs`'s `onboarding_titles`), not applied
//! by wholesale replacement: a per-distro check (`open_review`'s trigger,
//! or a post-consent-install reverify) only ever covers the ONE distro it
//! was asked about, so replacing the whole row set would erase every OTHER
//! live distro's rows — including a pending consent row — from an open
//! page even though that distro is still allowed and simply wasn't part of
//! this particular check (S8e review, P3).

use std::collections::HashSet;

use dv_core::provision::{ComponentId, ComponentState, ConsentAction, ConsistencyReport};

/// The onboarding page, while open.
pub(crate) struct OnboardingPage {
    pub(crate) rows: Vec<Row>,
    /// `true` only for the page's very first-ever appearance (drives the
    /// "Welcome to dv" vs. plain "Setup status" header, and whether closing
    /// it calls `SetupState::mark_complete` — see `shell.rs`'s
    /// `close_onboarding_page`). A later, auto-surfaced-by-drift page is
    /// never `first_run`, even if the user never dismissed the very first
    /// one (closing it is what stamps the marker, not merely opening one).
    pub(crate) first_run: bool,
    /// `true` while the background check that will populate `rows` is
    /// still in flight — every row shows [`RowState::Checking`] until the
    /// first [`Self::apply_report`] call lands.
    pub(crate) running: bool,
    /// Titles (the same merge key `apply_report` uses) of rows with a
    /// consent-triggered `install_vtsls` genuinely in flight right now
    /// (`shell.rs`'s `on_onboarding_consent_install`). A consistency check
    /// can complete WHILE that install is still running — either one already
    /// in flight before the Install click (starting an install does not bump
    /// `onboarding_check_gen`), or a fresh one spawned by opening another WSL
    /// repo during the up-to-180s window — and its detection ran against a
    /// half-written `npm install`, so its answer for this exact row is stale
    /// before it even lands. Both [`Self::apply_report`] and
    /// [`Self::apply_report_if_unanswered`] skip any row whose title is in
    /// this set instead of overwriting it back to `Consent`/whatever the
    /// concurrent check saw (S8e review, P2). Cleared by the install's own
    /// completion handler (`Self::end_install`) before it hands the row back
    /// to a normal check — the success arm right before spawning the
    /// reverify, so THAT check's own report is free to land.
    pub(crate) installing: HashSet<String>,
}

/// One row: a component the onboarding spine tracks, and where its last
/// check left it.
pub(crate) struct Row {
    // Only read by `automation_state`'s "onboarding" dump (the render path
    // and the consent-install handler both key off row INDEX now, not
    // `ComponentId` — see `shell.rs`'s `on_onboarding_consent_install`,
    // S8e review, P2) — `allow`ed rather than `cfg`'d out under
    // `--no-default-features` since it's still unconditionally written by
    // `placeholder`/`apply_report` (same pattern as `ReviewBadge`'s own
    // automation-only fields).
    #[cfg_attr(not(feature = "automation"), allow(dead_code))]
    pub(crate) id: ComponentId,
    pub(crate) title: String,
    pub(crate) state: RowState,
}

/// Mirrors [`ComponentState`] one-for-one, plus `Checking` — the
/// placeholder state a row starts in before the first real answer arrives
/// (`ComponentState` itself has no such state; [`super::ComponentState::Installing`]
/// is the closest, but that's reserved for an install genuinely in flight,
/// which `Checking` (merely awaiting the FIRST read) is not).
pub(crate) enum RowState {
    Checking,
    Ok(String),
    Missing(String),
    Consent(ConsentAction, String),
    Installing,
    Failed(String),
    Skipped(String),
}

impl From<ComponentState> for RowState {
    fn from(state: ComponentState) -> Self {
        match state {
            ComponentState::Ok { detail } => RowState::Ok(detail),
            ComponentState::Missing { guidance } => RowState::Missing(guidance),
            ComponentState::NeedsConsent { action, detail } => RowState::Consent(action, detail),
            ComponentState::Installing => RowState::Installing,
            ComponentState::Failed { error } => RowState::Failed(error),
            ComponentState::Skipped { reason } => RowState::Skipped(reason),
        }
    }
}

impl OnboardingPage {
    /// Build the page with every row in [`RowState::Checking`] — shown
    /// immediately (first paint never waits on a WSL round trip), before
    /// the background [`ConsistencyReport`] has landed. `titles` is the
    /// exact `(ComponentId, title)` list the page will show once real data
    /// arrives (see `shell.rs`'s `onboarding_titles`, which mirrors
    /// `dv_core::provision::consistency_check`'s own title format so the
    /// placeholder never visibly reflows into a different row set).
    pub(crate) fn placeholder(first_run: bool, titles: Vec<(ComponentId, String)>) -> Self {
        Self {
            rows: titles
                .into_iter()
                .map(|(id, title)| Row {
                    id,
                    title,
                    state: RowState::Checking,
                })
                .collect(),
            first_run,
            running: true,
            installing: HashSet::new(),
        }
    }

    /// Merge `report`'s real answers into the row set by title (see the
    /// module doc): a row whose title already exists gets its state
    /// updated in place; a title the page hasn't seen before (e.g. the
    /// very first report landing on a `Checking`-only placeholder page, or
    /// a distro discovered after the page opened) is appended. Rows for a
    /// title the report simply didn't cover are left untouched — this
    /// check may only have covered one distro out of several the page is
    /// showing. A row with an install genuinely in flight (`self.installing`
    /// — see its doc comment) is skipped entirely: this is the CURRENT
    /// check's own answer, not a stale one, but it's still reading a
    /// half-written `npm install`, so it must not clobber `Installing` back
    /// to `Consent`/whatever it detected (S8e review, P2).
    pub(crate) fn apply_report(&mut self, report: ConsistencyReport) {
        for c in report.components {
            if self.installing.contains(&c.title) {
                continue;
            }
            if self.is_superseded_generic_placeholder(c.id, &c.title) {
                continue;
            }
            self.drop_generic_placeholder(c.id, &c.title);
            match self.rows.iter_mut().find(|row| row.title == c.title) {
                Some(row) => row.state = RowState::from(c.state),
                None => self.rows.push(Row {
                    id: c.id,
                    title: c.title,
                    state: RowState::from(c.state),
                }),
            }
        }
        self.running = false;
    }

    /// Merge `report` the same way as [`Self::apply_report`], except a row
    /// that already has a non-[`RowState::Checking`] state is left
    /// untouched instead of being overwritten. Used for a report from a
    /// SUPERSEDED check generation (`shell.rs`'s `apply_consistency_report`):
    /// its answers are still real and should fill in any row still awaiting
    /// its very first answer (an idempotent "first answer wins"), but must
    /// never regress a row a newer, non-superseded check already resolved —
    /// unlike the single-slot generation guard this replaces, which dropped
    /// the ENTIRE stale report and left every row it alone would have
    /// answered stuck at `Checking` forever (S8e review, P2 — deterministic
    /// on a first-run WSL-seeded launch, where `AppShell::new`'s own
    /// placeholder check is superseded by `open_review`'s per-distro one
    /// before it can complete). Also respects `self.installing`, same as
    /// `apply_report`.
    pub(crate) fn apply_report_if_unanswered(&mut self, report: ConsistencyReport) {
        for c in report.components {
            if self.installing.contains(&c.title) {
                continue;
            }
            if self.is_superseded_generic_placeholder(c.id, &c.title) {
                continue;
            }
            self.drop_generic_placeholder(c.id, &c.title);
            match self.rows.iter_mut().find(|row| row.title == c.title) {
                Some(row) if matches!(row.state, RowState::Checking) => {
                    row.state = RowState::from(c.state);
                }
                // A row genuinely stuck `Installing` with no in-flight
                // marker (`self.installing`, already checked above) means
                // the install finished and THIS report is the only answer
                // that will ever arrive for it — a reverify scoped to a
                // different distro's titles never touches it (S8e review,
                // P3: otherwise the row reads "installing…" forever after a
                // successful install superseded by a second one).
                Some(row) if matches!(row.state, RowState::Installing) => {
                    row.state = RowState::from(c.state);
                }
                Some(_) => {} // already has a newer answer; don't regress it
                None => self.rows.push(Row {
                    id: c.id,
                    title: c.title,
                    state: RowState::from(c.state),
                }),
            }
        }
        // Deliberately does NOT touch `running`: this report's completion
        // doesn't mean the check that superseded it is done too.
    }

    /// Remove the generic, distro-unsuffixed placeholder row for `id` (the
    /// `Skipped("no WSL distro running")` row `shell.rs`'s
    /// `pad_no_distro_rows`/`onboarding_titles` synthesize before any real
    /// distro is live) once a REAL per-distro report for the same component
    /// arrives — `title` carrying a `" — <distro>"` suffix is exactly that
    /// signal. Without this, the two rows coexist indefinitely on an open
    /// page: a generic row claiming no distro is running directly above a
    /// live per-distro row for the same component, once a WSL repo is opened
    /// after the page showed the no-distro placeholders (S8e review, P3).
    fn drop_generic_placeholder(&mut self, id: ComponentId, title: &str) {
        let base = id.title();
        if title != base {
            self.rows.retain(|row| !(row.id == id && row.title == base));
        }
    }

    /// The inverse of [`Self::drop_generic_placeholder`]: `true` when `c` is
    /// itself a generic, distro-unsuffixed placeholder (`title == id.title()`)
    /// AND a real distro-suffixed row for the same component already exists.
    /// Two consistency-check generations can be in flight together — a
    /// per-distro check (gen N) and a broader, empty-allow-list launch check
    /// (gen N+1, padded with generic `Skipped` rows by `shell.rs`'s
    /// `pad_no_distro_rows`) — in either landing order. If the per-distro
    /// report lands FIRST, its suffixed rows already displaced the generic
    /// placeholder; the later generic report must not re-add it, or the page
    /// shows both "dv-host — Ubuntu: Ok" and "dv-host: no WSL distro running"
    /// at once (S8e review, P3). Callers must still check this BEFORE
    /// `drop_generic_placeholder` runs for the same component, since a
    /// generic `c` never triggers `drop_generic_placeholder` itself (that
    /// only fires for a suffixed title).
    fn is_superseded_generic_placeholder(&self, id: ComponentId, title: &str) -> bool {
        let base = id.title();
        title == base
            && self
                .rows
                .iter()
                .any(|row| row.id == id && row.title != base)
    }

    /// `Some` when any row is in a state that genuinely needs a human to
    /// look at it, uniquely identifying WHICH rows (see this fn's own doc
    /// below); `None` otherwise — used by `shell.rs`'s
    /// `close_onboarding_page` to decide whether closing this page should
    /// latch `drift_page_dismissed`/persist a dismissal (S8e review, P2;
    /// phase-8 capstone review, P3): closing a page that showed nothing but
    /// `Ok`/`Skipped`/`Checking` rows (the common first-run-with-no-live-
    /// distro case) must not suppress a LATER, genuinely different drift
    /// from auto-surfacing — the latch's whole rationale ("the user has
    /// seen and dismissed a drift report") only holds when a drift report
    /// was actually on screen.
    ///
    /// This page's counterpart to
    /// [`dv_core::provision::ConsistencyReport::needs_human_fingerprint`] —
    /// same `(id, state KIND, title)` shape, computed from this page's own
    /// merged `rows` rather than a single raw report (a page can be the
    /// product of several merged per-distro checks — see this module's own
    /// doc comment). `shell.rs`'s `close_onboarding_page` persists this on
    /// dismissal (`crate::setup::SetupState::mark_drift_dismissed`) so a
    /// permanent-by-choice state (no `gh`, a declined vtsls consent)
    /// doesn't re-pop the page every single launch (phase-8 capstone
    /// review, P3).
    pub(crate) fn needs_human_fingerprint(&self) -> Option<String> {
        let mut entries: Vec<(String, &'static str, String)> = self
            .rows
            .iter()
            .filter_map(|row| {
                let kind = match &row.state {
                    RowState::Missing(_) => "missing",
                    RowState::Consent(..) => "consent",
                    RowState::Installing => "installing",
                    RowState::Failed(_) => "failed",
                    RowState::Checking | RowState::Ok(_) | RowState::Skipped(_) => return None,
                };
                Some((format!("{:?}", row.id), kind, row.title.clone()))
            })
            .collect();
        if entries.is_empty() {
            return None;
        }
        entries.sort();
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        entries.hash(&mut hasher);
        Some(format!("{:016x}", hasher.finish()))
    }

    /// Seed rows already known to be mid-install at the shell level
    /// (`AppShell::onboarding_installing`, persisted across a page close/
    /// reopen — S8e review, P3): mark each matching row `Installing`
    /// immediately, before any check has even run, and copy `installing`
    /// into `self.installing` so `apply_report`/`apply_report_if_unanswered`
    /// keep skipping it too. Without this, a fresh page built while an
    /// install is still running in the background has no memory of that —
    /// its own check re-detects the half-written `npm install` as
    /// `NeedsConsent` ("present but not responding — reinstall"), and a
    /// second click starts a second concurrent install into the same
    /// distro.
    pub(crate) fn seed_installing(&mut self, installing: &HashSet<String>) {
        for row in &mut self.rows {
            if installing.contains(&row.title) {
                row.state = RowState::Installing;
            }
        }
        self.installing = installing.clone();
    }

    /// Optimistically flip the row AT `idx` to [`RowState::Installing`] and
    /// mark its title in-flight (`self.installing`) — called the instant the
    /// user clicks "Install" on a consent row, before the background install
    /// task itself has produced any answer (see `shell.rs`'s
    /// `on_onboarding_consent_install`). Keyed by row index, not
    /// [`ComponentId`]: two different live distros can both land on a
    /// `NodeVtsls` consent row in the same render, and flipping every row
    /// matching the id would mark the WRONG distro's row `Installing` too
    /// (S8e review, P2).
    pub(crate) fn begin_install(&mut self, idx: usize) {
        if let Some(row) = self.rows.get_mut(idx) {
            row.state = RowState::Installing;
            self.installing.insert(row.title.clone());
        }
    }

    /// Clear `title`'s in-flight marker once its install has actually
    /// finished (success or failure) — keyed by title, not the row index
    /// captured at click time, since the page can have been closed and
    /// reopened with a different row layout during the up-to-180s install
    /// (S8e review, P3). Must be called BEFORE handing the row back to a
    /// normal check (`shell.rs`'s `on_onboarding_consent_install` does this
    /// ahead of both its success and failure arms), or `apply_report`/
    /// `apply_report_if_unanswered` will keep skipping it forever.
    pub(crate) fn end_install(&mut self, title: &str) {
        self.installing.remove(title);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_report(components: Vec<(ComponentId, &str)>) -> ConsistencyReport {
        ConsistencyReport {
            components: components
                .into_iter()
                .map(|(id, title)| dv_core::provision::ComponentReport {
                    id,
                    title: title.to_string(),
                    state: ComponentState::Ok {
                        detail: "fine".to_string(),
                    },
                })
                .collect(),
            drift: false,
        }
    }

    /// S8e review, P3: a distro-suffixed row for a component must displace
    /// the generic no-distro placeholder for the SAME component, not
    /// coexist with it.
    #[test]
    fn apply_report_drops_the_generic_placeholder_once_a_distro_row_for_the_same_id_lands() {
        let mut page = OnboardingPage::placeholder(
            false,
            vec![(ComponentId::DvHost, ComponentId::DvHost.title().to_string())],
        );
        page.apply_report(ok_report(vec![(
            ComponentId::DvHost,
            ComponentId::DvHost.title(),
        )]));
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].title, ComponentId::DvHost.title());

        // A real per-distro report for the same id arrives.
        page.apply_report(ok_report(vec![(
            ComponentId::DvHost,
            &format!("{} — Ubuntu", ComponentId::DvHost.title()),
        )]));

        assert_eq!(
            page.rows.len(),
            1,
            "the generic placeholder must be removed, not left alongside the distro row: {:?}",
            page.rows.iter().map(|r| &r.title).collect::<Vec<_>>()
        );
        assert_eq!(
            page.rows[0].title,
            format!("{} — Ubuntu", ComponentId::DvHost.title())
        );
    }

    /// A DIFFERENT component's generic placeholder (e.g. `dv-cli`) must
    /// survive a distro row landing for `dv-host` — the dedup is per-id.
    #[test]
    fn apply_report_dedup_is_scoped_to_the_matching_component_id() {
        let mut page = OnboardingPage::placeholder(
            false,
            vec![
                (ComponentId::DvHost, ComponentId::DvHost.title().to_string()),
                (ComponentId::DvCli, ComponentId::DvCli.title().to_string()),
            ],
        );
        page.apply_report(ok_report(vec![(
            ComponentId::DvHost,
            &format!("{} — Ubuntu", ComponentId::DvHost.title()),
        )]));
        assert_eq!(page.rows.len(), 2);
        assert!(
            page.rows
                .iter()
                .any(|r| r.title == ComponentId::DvCli.title())
        );
    }

    #[test]
    fn needs_human_fingerprint_is_none_while_checking_and_once_ok_or_skipped() {
        let mut page = OnboardingPage::placeholder(
            false,
            vec![(ComponentId::GhCli, ComponentId::GhCli.title().to_string())],
        );
        assert_eq!(
            page.needs_human_fingerprint(),
            None,
            "Checking must not count"
        );
        page.apply_report(ok_report(vec![(
            ComponentId::GhCli,
            ComponentId::GhCli.title(),
        )]));
        assert_eq!(page.needs_human_fingerprint(), None, "Ok must not count");
        page.rows[0].state = RowState::Skipped("n/a".to_string());
        assert_eq!(
            page.needs_human_fingerprint(),
            None,
            "Skipped must not count"
        );
    }

    #[test]
    fn needs_human_fingerprint_is_some_for_missing_consent_installing_and_failed() {
        for state in [
            RowState::Missing("x".to_string()),
            RowState::Consent(
                ConsentAction::InstallVtsls {
                    distro: "Ubuntu".to_string(),
                    node: dv_core::provision::NodeVtsls {
                        node_path: "/x/node".to_string(),
                        node_version: "v1".to_string(),
                        vtsls_path: None,
                        vtsls_version: None,
                    },
                },
                "x".to_string(),
            ),
            RowState::Installing,
            RowState::Failed("x".to_string()),
        ] {
            let mut page = OnboardingPage::placeholder(false, Vec::new());
            page.rows.push(Row {
                id: ComponentId::NodeVtsls,
                title: "x".to_string(),
                state,
            });
            assert!(page.needs_human_fingerprint().is_some());
        }
    }

    #[test]
    fn needs_human_fingerprint_is_none_for_ok_skipped_and_checking() {
        let mut page = OnboardingPage::placeholder(false, Vec::new());
        for state in [
            RowState::Checking,
            RowState::Ok("fine".to_string()),
            RowState::Skipped("n/a".to_string()),
        ] {
            page.rows = vec![Row {
                id: ComponentId::GhCli,
                title: "gh".to_string(),
                state,
            }];
            assert_eq!(page.needs_human_fingerprint(), None);
        }
    }

    #[test]
    fn needs_human_fingerprint_matches_across_row_order_and_message_wording() {
        let mut a = OnboardingPage::placeholder(false, Vec::new());
        a.rows = vec![
            Row {
                id: ComponentId::GhCli,
                title: "gh".to_string(),
                state: RowState::Missing("install from https://cli.github.com".to_string()),
            },
            Row {
                id: ComponentId::NodeVtsls,
                title: "vtsls — Ubuntu".to_string(),
                state: RowState::Failed("x".to_string()),
            },
        ];
        let mut b = OnboardingPage::placeholder(false, Vec::new());
        b.rows = vec![
            Row {
                id: ComponentId::NodeVtsls,
                title: "vtsls — Ubuntu".to_string(),
                state: RowState::Failed("a completely different message".to_string()),
            },
            Row {
                id: ComponentId::GhCli,
                title: "gh".to_string(),
                state: RowState::Missing("totally reworded guidance".to_string()),
            },
        ];
        let fp_a = a.needs_human_fingerprint();
        let fp_b = b.needs_human_fingerprint();
        assert!(fp_a.is_some());
        assert_eq!(fp_a, fp_b);
    }

    /// S8e review, P3: a title persisted at the shell level must be
    /// immediately visible as `Installing` on a freshly built page, even
    /// before any check has run — not left at `Checking`.
    #[test]
    fn seed_installing_marks_matching_rows_installing_up_front() {
        let mut page = OnboardingPage::placeholder(
            false,
            vec![(
                ComponentId::NodeVtsls,
                format!("{} — Ubuntu", ComponentId::NodeVtsls.title()),
            )],
        );
        let mut installing = HashSet::new();
        installing.insert(format!("{} — Ubuntu", ComponentId::NodeVtsls.title()));
        page.seed_installing(&installing);

        assert!(matches!(page.rows[0].state, RowState::Installing));
        // And a subsequent stale/superseded report landing on this title
        // must still be skipped, not overwrite `Installing`.
        page.apply_report_if_unanswered(ok_report(vec![(
            ComponentId::NodeVtsls,
            &format!("{} — Ubuntu", ComponentId::NodeVtsls.title()),
        )]));
        assert!(matches!(page.rows[0].state, RowState::Installing));
    }
}
