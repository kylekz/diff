//! The application shell: the review-navigator sidebar plus the main area
//! that hosts the active review. dv is review-centric — the sidebar is a
//! library of reviews across repos (see docs/ui-design.md).

use std::path::PathBuf;

use dv_core::{
    ChecksSummary, DiffSource, GithubClient, PrState, RemoteRef, RepoLocation, RepoSlug,
    ReviewDecision,
};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, Sizable as _, StyledExt, TitleBar, h_flex, v_flex};

use std::collections::HashMap;

use crate::recent::{RecentEntry, RecentStore, title_for};
use crate::settings::Settings;
use crate::themes;
use crate::workspace::{
    ReviewChanged, Workspace, checks_word, pr_state_word, review_decision_word,
};

actions!(
    shell,
    [
        NewReview,
        RefreshBadges,
        OpenThemePicker,
        ThemePickerNext,
        ThemePickerPrev,
        ThemePickerClose,
        ThemePickerChoose
    ]
);

const KEY_CONTEXT: &str = "AppShell";
/// Stamped onto the shell's key context while the theme picker is open
/// (same mechanism `workspace.rs` uses for its palette/PR-picker overlays).
const THEME_PICKER_CONTEXT: &str = "ThemePickerOpen";

/// Make a local location absolute (lexically, without touching the
/// filesystem) so it survives being persisted and re-opened from a
/// different working directory. WSL locations are already absolute.
fn absolutize(location: RepoLocation) -> RepoLocation {
    match location {
        RepoLocation::Local(path) => {
            RepoLocation::Local(std::path::absolute(&path).unwrap_or(path))
        }
        other => other,
    }
}

/// The latest review's badge for a repo, local fields only (no network) —
/// `None` when it has no reviews (or the store is unreadable — the sidebar
/// just shows nothing). Also hands back the latest review's `remote`
/// linkage (if any), so a second pass ([`fetch_pr_badge`]) can decide
/// whether/how to fetch PR status without re-reading the store.
fn compute_local_badge(location: RepoLocation) -> Option<(ReviewBadge, Option<RemoteRef>)> {
    let latest = dv_core::ReviewStore::open(location)
        .list()
        .ok()?
        .into_iter()
        .next()?;
    let open = latest
        .comments
        .iter()
        .filter(|c| c.status == dv_core::CommentStatus::Open)
        .count();
    let submitted = matches!(latest.state, dv_core::ReviewState::Submitted { .. });
    Some((
        ReviewBadge {
            open,
            submitted,
            pr: None,
        },
        latest.remote,
    ))
}

/// [`AppShell::refresh_badge`]'s local-recompute step, as a pure function:
/// `fresh_local` always wins for `open`/`submitted` (it just re-read the
/// store), but `pr` only gets clobbered when `fetch_remote` is true — a
/// `false` call (the `ReviewChanged` subscription, review finding P3-3)
/// instead carries over whatever `pr` badge `existing` already had, so a
/// comment save doesn't flash the PR cluster off while a fetch it never
/// asked for re-fetches it. Kept separate from the `cx.spawn` plumbing
/// around it so it's unit-testable without a running `Context` (matches
/// `workspace.rs`'s `SubmitFlow` pure-transition-function pattern).
fn merge_local_badge(
    existing: Option<ReviewBadge>,
    fresh_local: ReviewBadge,
    fetch_remote: bool,
) -> ReviewBadge {
    if fetch_remote {
        return fresh_local;
    }
    ReviewBadge {
        pr: existing.and_then(|b| b.pr),
        ..fresh_local
    }
}

/// `gh pr view --json state,isDraft,reviewDecision,statusCheckRollup` for a
/// review's linked PR (docs/phase-3-github.md deliverable 3/5) — built
/// straight from `remote`'s own `slug`/`pr` (no `GitRepo`/`origin` remote
/// read needed at all, see [`GithubClient::for_slug`]). Silent `None` on
/// any failure (gh missing, unauthenticated, network down, PR deleted,
/// ...): a badge with no PR cluster is a perfectly good fallback, and this
/// runs on every startup for every recent entry, so it must never surface
/// an error the user didn't ask for.
fn fetch_pr_badge(remote: &RemoteRef) -> Option<PrBadge> {
    let mut parts = remote.slug.splitn(3, '/');
    let host = parts.next()?;
    let owner = parts.next()?;
    let repo = parts.next()?;
    if host.is_empty() || owner.is_empty() || repo.is_empty() {
        return None;
    }
    let slug = RepoSlug {
        host: host.to_string(),
        owner: owner.to_string(),
        repo: repo.to_string(),
    };
    let client = GithubClient::for_slug(slug).ok()?;
    let status = client.pr_status(remote.pr).ok()?;
    Some(PrBadge {
        state: status.state,
        is_draft: status.is_draft,
        decision: status.review_decision,
        checks: status.checks,
    })
}

pub fn init(cx: &mut App) {
    let shell = Some(KEY_CONTEXT);
    let theme_picker = Some("AppShell && ThemePickerOpen");
    cx.bind_keys([
        KeyBinding::new("cmd-n", NewReview, shell),
        KeyBinding::new("ctrl-n", NewReview, shell),
        KeyBinding::new("ctrl-shift-t", OpenThemePicker, shell),
    ]);
    cx.bind_keys([
        KeyBinding::new("down", ThemePickerNext, theme_picker),
        KeyBinding::new("up", ThemePickerPrev, theme_picker),
        KeyBinding::new("escape", ThemePickerClose, theme_picker),
        KeyBinding::new("enter", ThemePickerChoose, theme_picker),
    ]);
}

pub struct AppShell {
    focus_handle: FocusHandle,
    recent: RecentStore,
    active: Option<Entity<Workspace>>,
    /// Index into `recent.entries()` of the active review, for highlighting.
    selected: Option<usize>,
    /// True under `--automation`: blocks the native folder picker, which
    /// would wedge the foreground executor (and thus the whole automation
    /// channel) until a human dismissed it.
    automation: bool,
    /// Per-repo review badge (latest review's open-comment count /
    /// submitted flag), refreshed off-thread. Keyed by location, not list
    /// index — the recent list shifts when new entries insert at the top
    /// (review finding: index keys wore the wrong rows' badges).
    badges: HashMap<RepoLocation, ReviewBadge>,
    /// Keeps the active workspace's ReviewChanged subscription alive.
    _ws_subscription: Option<Subscription>,
    /// Persisted app-wide settings (currently just the active theme name).
    /// Loaded once at startup; updated and re-saved on every theme-picker
    /// choice.
    settings: Settings,
    /// The theme-picker overlay (`ctrl-shift-t`), when open. Unlike the
    /// PR picker there's nothing to load — the registry is a static list —
    /// so this is just a cursor into `themes::names()`.
    theme_picker: Option<ThemePicker>,
}

/// The theme picker overlay, while open.
struct ThemePicker {
    /// Cursor into `themes::names()`.
    selected: usize,
}

/// Sidebar badge for one repo's latest review.
#[derive(Debug, Clone, Copy)]
struct ReviewBadge {
    open: usize,
    submitted: bool,
    /// PR status (docs/phase-3-github.md deliverable 3/5), when the latest
    /// review is linked to one and the network fetch succeeded — `None`
    /// either way renders no PR cluster at all (see [`fetch_pr_badge`]).
    pr: Option<PrBadge>,
}

/// Sidebar PR-status cluster for one repo's latest review: state glyph,
/// review-decision marker, and CI dot (rendered in [`AppShell::render_recent_row`]).
#[derive(Debug, Clone, Copy)]
struct PrBadge {
    state: PrState,
    is_draft: bool,
    decision: Option<ReviewDecision>,
    checks: ChecksSummary,
}

impl AppShell {
    pub fn new(
        seed: Option<(RepoLocation, DiffSource)>,
        automation: bool,
        pending_pr: Option<u64>,
        settings: Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            recent: RecentStore::load(),
            active: None,
            selected: None,
            automation,
            badges: HashMap::new(),
            _ws_subscription: None,
            settings,
            theme_picker: None,
        };
        // App-open refresh (docs/phase-3-github.md deliverable 3): skip the
        // network pr_status pass for WSL-located entries here specifically
        // — a cold distro hasn't booted yet, and walking every WSL entry
        // sequentially at startup would stack a wsl.exe boot behind each
        // one. A manual refresh (`RefreshBadges`) or simply opening that
        // review (`refresh_badge`, not WSL-skipped) still fetches it.
        this.refresh_all_badges(true, cx);
        match seed {
            Some((location, source)) => this.open_review(location, source, pending_pr, window, cx),
            // Nothing to focus into, so hold focus on the shell — otherwise
            // the advertised Ctrl+N binding (in the shell's key context) has
            // no focused node on its dispatch path and never fires.
            None => window.focus(&this.focus_handle, cx),
        }
        this
    }

    /// Open a review for `location`/`source`: record it as most-recent,
    /// spin up a fresh Workspace, and focus it so keyboard nav is live.
    /// `pending_pr` is only ever `Some` on the very first review a freshly
    /// launched `dv pr <number|url>` opens — `open_recent`/`automation_open`
    /// always pass `None`.
    fn open_review(
        &mut self,
        location: RepoLocation,
        source: DiffSource,
        pending_pr: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Persist an absolute path: a relative one (`dv .`) would resolve
        // against whatever cwd the app is next launched from.
        let location = absolutize(location);
        let title = title_for(&location, &source);
        let index = self.recent.touch(RecentEntry {
            location: location.clone(),
            source: source.clone(),
            title,
            last_opened_ms: 0, // touch stamps the real time
        });
        self.selected = Some(index);
        // Review-open refresh (docs/phase-3-github.md deliverable 3): not
        // WSL-skipped — opening this specific review already implies its
        // distro (if any) is live, so there's no cold-boot backlog hazard
        // the way there is walking every recent entry at startup. Full
        // refresh (local + network) — see `refresh_badge`'s doc comment.
        self.refresh_badge(index, true, cx);

        let workspace = cx.new(|cx| Workspace::new(location, source, pending_pr, window, cx));
        let handle = workspace.focus_handle(cx);
        window.focus(&handle, cx);
        // Keep this entry's badge live while the review is being worked on.
        self._ws_subscription = Some(cx.subscribe(
            &workspace,
            move |this: &mut Self, _, _: &ReviewChanged, cx| {
                if let Some(selected) = this.selected {
                    // Local-only, no network — see `refresh_badge`'s doc
                    // comment (review finding P3-3).
                    this.refresh_badge(selected, false, cx);
                }
            },
        ));
        self.active = Some(workspace);
        cx.notify();
    }

    /// Recompute one entry's badge off-thread (store I/O may hit WSL):
    /// local fields first (fast — one paint), then, if `fetch_remote` and
    /// the latest review is PR-linked, a network `pr_status` pass that
    /// patches in `badge.pr` (docs/phase-3-github.md deliverable 3).
    ///
    /// `fetch_remote` is `true` only for [`Self::open_review`]'s call site
    /// — opening a review is a deliberate, infrequent act, so paying for a
    /// `gh` round trip there is fine. It's `false` for the workspace's
    /// `ReviewChanged` subscription (review finding P3-3: every single
    /// comment save/status change/reply used to run this same network pass
    /// — a `gh` subprocess per keystroke-adjacent action — and, worse,
    /// clobbered the existing `badge.pr` with `None` first, causing a
    /// visible flicker before the fetch patched it back in). When
    /// `fetch_remote` is `false`, the local recompute instead carries over
    /// whatever `pr` badge already exists rather than clobbering it — the
    /// network view stays exactly as fresh as the last real refresh (app
    /// open, review open, or a manual `RefreshBadges`).
    fn refresh_badge(&mut self, index: usize, fetch_remote: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.recent.entries().get(index) else {
            return;
        };
        let location = entry.location.clone();
        cx.spawn(async move |this, cx| {
            let key = location.clone();
            let local = cx
                .background_executor()
                .spawn(async move { compute_local_badge(location) })
                .await;
            let remote = local.as_ref().and_then(|(_, remote)| remote.clone());
            this.update(cx, |this, cx| {
                match local {
                    Some((badge, _)) => {
                        let existing = this.badges.get(&key).copied();
                        this.badges.insert(
                            key.clone(),
                            merge_local_badge(existing, badge, fetch_remote),
                        );
                    }
                    None => {
                        this.badges.remove(&key);
                    }
                }
                cx.notify();
            })
            .ok();

            if !fetch_remote {
                return;
            }
            let Some(remote) = remote else { return };
            let pr = cx
                .background_executor()
                .spawn(async move { fetch_pr_badge(&remote) })
                .await;
            if let Some(pr) = pr {
                this.update(cx, |this, cx| {
                    if let Some(badge) = this.badges.get_mut(&key) {
                        badge.pr = Some(pr);
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Recompute every entry's badge. Two passes, so the sidebar never
    /// waits on the network (docs/phase-3-github.md deliverable 3):
    /// 1. Local fields for every entry (fast), applied and painted first.
    /// 2. `pr_status` for every entry whose latest review is PR-linked,
    ///    sequential (one `gh` subprocess at a time — cheap, and avoids
    ///    hammering `gh`/GitHub with a burst).
    ///
    /// `startup` (true only for the app-open call in `AppShell::new`) picks
    /// between two different semantics for the local pass (review finding
    /// P2-1 — this used to be a single merge-don't-clobber `or_insert` no
    /// matter which caller asked, so an explicit `RefreshBadges` click
    /// never actually updated an entry that already had a badge, nor ever
    /// fetched PR status for one — exactly the WSL-skipped-at-startup
    /// entries a manual refresh exists to fill in):
    /// - `true` (startup): merge-don't-clobber — entries opened (or
    ///   manually refreshed) while this walk ran already have fresher
    ///   badges, so only a location this pass is the first to see gets
    ///   queued for the network pass, and WSL-located entries are skipped
    ///   entirely (dodges a distro-boot pile-up at cold start).
    /// - `false` (explicit `RefreshBadges`): replace every entry's local
    ///   fields unconditionally, and queue the network pass for every
    ///   PR-linked entry — including WSL ones, since this is a deliberate,
    ///   infrequent ask with no cold-boot backlog concern.
    fn refresh_all_badges(&mut self, startup: bool, cx: &mut Context<Self>) {
        let locations: Vec<_> = self
            .recent
            .entries()
            .iter()
            .map(|e| e.location.clone())
            .collect();
        cx.spawn(async move |this, cx| {
            let local = cx
                .background_executor()
                .spawn(async move {
                    locations
                        .into_iter()
                        .filter_map(|loc| {
                            compute_local_badge(loc.clone())
                                .map(|(badge, remote)| (loc, badge, remote))
                        })
                        .collect::<Vec<_>>()
                })
                .await;

            let mut to_fetch = Vec::new();
            this.update(cx, |this, cx| {
                for (loc, badge, remote) in local {
                    if startup {
                        // Merge rather than replace: entries opened (or
                        // manually refreshed) while this walk ran already
                        // have fresher badges — only a location this pass
                        // is the first to see gets queued for the network
                        // pass below.
                        let is_new = !this.badges.contains_key(&loc);
                        this.badges.entry(loc.clone()).or_insert(badge);
                        if is_new
                            && let Some(remote) = remote
                            && !matches!(loc, RepoLocation::Wsl { .. })
                        {
                            to_fetch.push((loc, remote));
                        }
                    } else {
                        // Explicit ask: recompute unconditionally, and
                        // fetch PR status for every linked entry — WSL
                        // included.
                        this.badges.insert(loc.clone(), badge);
                        if let Some(remote) = remote {
                            to_fetch.push((loc, remote));
                        }
                    }
                }
                cx.notify();
            })
            .ok();

            for (loc, remote) in to_fetch {
                let pr = cx
                    .background_executor()
                    .spawn(async move { fetch_pr_badge(&remote) })
                    .await;
                if let Some(pr) = pr {
                    this.update(cx, |this, cx| {
                        if let Some(badge) = this.badges.get_mut(&loc) {
                            badge.pr = Some(pr);
                        }
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    fn on_refresh_badges(&mut self, _: &RefreshBadges, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_all_badges(false, cx);
    }

    // ---- Theme picker ---------------------------------------------------

    fn on_open_theme_picker(
        &mut self,
        _: &OpenThemePicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.theme_picker.is_some() {
            return;
        }
        let selected = themes::names()
            .position(|n| n == self.settings.theme)
            .unwrap_or(0);
        self.theme_picker = Some(ThemePicker { selected });
        // Capture focus onto the shell itself while the picker is open. Its
        // up/down/enter/escape bindings live in the shell's own key context
        // ("AppShell && ThemePickerOpen"); if focus stayed wherever it was
        // (typically deep inside the active workspace), that context would
        // be a strict *ancestor* of the focused node rather than on its
        // dispatch path — and worse, the workspace's own browse bindings
        // for the same keys (arrows, `j`/`k`) would still be live there too.
        // Moving focus here sidesteps the ambiguity entirely, the same way
        // `AppShell::new`'s empty-state fallback does.
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn close_theme_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.theme_picker.take().is_some() {
            // Restore focus to wherever it would otherwise be: the active
            // workspace if one is open, else the shell's own handle (mirrors
            // `AppShell::new`'s "nothing to focus into" fallback).
            match &self.active {
                Some(ws) => window.focus(&ws.focus_handle(cx), cx),
                None => window.focus(&self.focus_handle, cx),
            }
            cx.notify();
        }
    }

    fn on_theme_picker_close(
        &mut self,
        _: &ThemePickerClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_theme_picker(window, cx);
    }

    fn on_theme_picker_next(&mut self, _: &ThemePickerNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = &mut self.theme_picker {
            let len = themes::names().count();
            if len > 0 {
                picker.selected = (picker.selected + 1).min(len - 1);
                cx.notify();
            }
        }
    }

    fn on_theme_picker_prev(&mut self, _: &ThemePickerPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = &mut self.theme_picker {
            picker.selected = picker.selected.saturating_sub(1);
            cx.notify();
        }
    }

    fn on_theme_picker_choose(
        &mut self,
        _: &ThemePickerChoose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = &self.theme_picker else {
            return;
        };
        let Some(name) = themes::names().nth(picker.selected) else {
            return;
        };
        self.choose_theme(name, window, cx);
    }

    /// Apply + persist `name` (a no-op re-apply when it's already the active
    /// theme — see the doc on the picker's marker), then close the picker.
    /// Shared by the keyboard path ([`Self::on_theme_picker_choose`]) and the
    /// picker row's mouse click.
    fn choose_theme(&mut self, name: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if name != self.settings.theme {
            themes::apply_theme(name, Some(window), cx);
            self.settings.theme = name.to_string();
            self.settings.save();
            // UI chrome picks up the new theme for free (reads `cx.theme()`
            // fresh every render), but the active workspace's diff pane
            // cached its syntax highlighting with the *old* theme's
            // concrete colors baked in — see `Workspace::on_theme_changed`.
            if let Some(ws) = &self.active {
                ws.update(cx, |ws, cx| ws.on_theme_changed(cx));
            }
        }
        self.close_theme_picker(window, cx);
    }

    fn open_recent(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.recent.entries().get(index).cloned() else {
            return;
        };
        self.open_review(entry.location, entry.source, None, window, cx);
    }

    /// The "new review" flow: a native folder picker. Because the picker can
    /// navigate to `\\wsl.localhost\<distro>\…` UNC paths,
    /// [`RepoLocation::from_path_arg`] transparently yields a WSL location —
    /// so this one control covers both local and WSL repos.
    fn on_new_review(&mut self, _: &NewReview, window: &mut Window, cx: &mut Context<Self>) {
        if self.automation {
            // The dialog's Show() blocks the foreground executor, so under
            // automation nothing — not even stdin-EOF quit — would ever run
            // again. Scripts use `{"cmd":"open","path":...}` instead.
            return;
        }
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Repository".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return; // dialog cancelled or errored
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.open_path(path, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        // The default source for a freshly opened repo is the pre-commit
        // review case: working tree vs HEAD. A non-repo/UNC path that can't
        // be parsed is dropped silently for now; a validation surface lands
        // with Phase 2.
        if let Ok(location) = RepoLocation::from_path_arg(&path.to_string_lossy()) {
            self.open_review(location, DiffSource::WorkingTree, None, window, cx);
        }
    }

    /// Semantic state for `--automation`: the sidebar plus the active
    /// review's own dump (see [`Workspace::automation_state`]).
    pub(crate) fn automation_state(&self, cx: &App) -> serde_json::Value {
        use serde_json::json;
        let theme = cx.theme();
        json!({
            "recent": self.recent.entries().iter().map(|e| e.title.clone()).collect::<Vec<_>>(),
            "selected": self.selected,
            "workspace": self.active.as_ref().map(|ws| ws.read(cx).automation_state()),
            // Per-recent-entry badge dump (docs/phase-3-github.md
            // deliverable 3/4), same order as `recent` above.
            "badges": self.recent.entries().iter().map(|entry| {
                let badge = self.badges.get(&entry.location);
                json!({
                    "title": entry.title,
                    "open": badge.map(|b| b.open),
                    "submitted": badge.map(|b| b.submitted),
                    "pr": badge.and_then(|b| b.pr.as_ref()).map(|pr| json!({
                        "state": pr_state_word(pr.state),
                        "is_draft": pr.is_draft,
                        "decision": pr.decision.map(review_decision_word),
                        "checks": checks_word(pr.checks),
                    })),
                })
            }).collect::<Vec<_>>(),
            // Theme deliverable: the currently-applied theme's own name
            // (read off the live global `Theme`, not `self.settings`, so
            // this can never lie about what's actually painted), whether
            // the picker overlay is open, and the resolved mono font family
            // so agents can assert JetBrains Mono is really active without
            // eyeballing a screenshot.
            "theme": theme.theme_name().to_string(),
            "theme_picker_open": self.theme_picker.is_some(),
            "mono_font": theme.mono_font_family.to_string(),
        })
    }

    /// True when nothing is loading — a bare shell counts as settled.
    pub(crate) fn automation_settled(&self, cx: &App) -> bool {
        match &self.active {
            Some(ws) => ws.read(cx).automation_settled(),
            None => true,
        }
    }

    /// Select the nth changed file in the active review. Errors (rather
    /// than silently no-oping) so automation responses never claim a
    /// selection that didn't happen.
    pub(crate) fn automation_select_file(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        match &self.active {
            Some(ws) => ws.update(cx, |ws, cx| ws.automation_select_file(index, window, cx)),
            None => Err(anyhow::anyhow!("no active review")),
        }
    }

    /// Scripted replacement for the folder picker (`{"cmd":"open"}`): open
    /// a working-tree review of `location` directly.
    pub(crate) fn automation_open(
        &mut self,
        location: RepoLocation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_review(location, DiffSource::WorkingTree, None, window, cx);
    }

    /// `{"cmd":"open_pr","number":N}`: open PR `number` in the active
    /// review's workspace (`Workspace::open_pr`). Errors (rather than
    /// silently no-oping) when there's no active review, matching
    /// `automation_select_file`'s contract.
    pub(crate) fn automation_open_pr(
        &mut self,
        number: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        match &self.active {
            Some(ws) => {
                ws.update(cx, |ws, cx| ws.open_pr(number, window, cx));
                Ok(())
            }
            None => Err(anyhow::anyhow!("no active review")),
        }
    }

    fn render_recent_row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let entry = &self.recent.entries()[index];
        let theme = cx.theme();
        let selected = self.selected == Some(index);
        h_flex()
            .id(index)
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .when(selected, |el| el.bg(theme.accent))
            .hover(|el| el.bg(theme.accent.opacity(0.5)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| this.open_recent(index, window, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_sm()
                    .truncate()
                    .child(entry.title.clone()),
            )
            .children(self.badges.get(&entry.location).map(|badge| {
                h_flex()
                    .flex_none()
                    .gap_2()
                    .items_center()
                    .child(if badge.open > 0 {
                        div()
                            .flex_none()
                            .px_1p5()
                            .rounded_full()
                            .bg(theme.primary.opacity(0.25))
                            .text_xs()
                            .text_color(theme.primary)
                            .child(format!("{}", badge.open))
                            .into_any_element()
                    } else if badge.submitted {
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.success)
                            .child("\u{2713}")
                            .into_any_element()
                    } else {
                        div().into_any_element()
                    })
                    .children(badge.pr.as_ref().map(|pr| {
                        // PR-status cluster (docs/phase-3-github.md
                        // deliverable 3/5): state glyph, review-decision
                        // marker, CI marker — subtle, one glyph each, kept
                        // well inside the row's ~24px height. The state
                        // glyph and the CI marker deliberately use different
                        // shapes (round dot vs. small square), not just
                        // different colors — an open PR (green ●) with
                        // passing checks (used to also be a green ●) was
                        // otherwise two indistinguishable dots side by side
                        // (review finding P3-2).
                        let (state_glyph, state_color) = if pr.is_draft {
                            ("\u{25d0}", theme.muted_foreground) // draft
                        } else {
                            match pr.state {
                                PrState::Merged => ("\u{21d7}", theme.primary), // merged
                                PrState::Open => ("\u{25cf}", theme.success),   // open
                                PrState::Closed => ("\u{25cf}", theme.danger),  // closed
                            }
                        };
                        let decision = match pr.decision {
                            Some(ReviewDecision::Approved) => Some(("\u{2713}", theme.success)),
                            Some(ReviewDecision::ChangesRequested) => {
                                Some(("\u{b1}", theme.danger))
                            }
                            Some(ReviewDecision::ReviewRequired) | None => None,
                        };
                        let ci_color = match pr.checks {
                            ChecksSummary::Passing => Some(theme.success),
                            ChecksSummary::Failing => Some(theme.danger),
                            ChecksSummary::Pending => Some(theme.warning),
                            ChecksSummary::None => None,
                        };
                        h_flex()
                            .id("pr-badge")
                            .flex_none()
                            .gap_1()
                            .items_center()
                            .text_xs()
                            .child(div().text_color(state_color).child(state_glyph))
                            .children(
                                decision.map(|(glyph, color)| div().text_color(color).child(glyph)),
                            )
                            .children(
                                // Small square (▪), not a dot — see the
                                // comment above on why this must not share
                                // the state glyph's shape.
                                ci_color.map(|color| div().text_color(color).child("\u{25aa}")),
                            )
                    }))
            }))
    }

    /// The theme picker overlay (`ctrl-shift-t`), when open: same
    /// popover-over-everything chrome as `workspace.rs`'s `render_pr_picker`,
    /// but listing the static theme registry instead — no loading/error
    /// state, since there's no async fetch involved.
    fn render_theme_picker(&self, cx: &mut Context<Self>) -> Option<Div> {
        let picker = self.theme_picker.as_ref()?;

        let theme = cx.theme();
        let border = theme.border;
        let popover = theme.popover;
        let popover_fg = theme.popover_foreground;
        let accent = theme.accent;
        let muted = theme.muted_foreground;
        let success = theme.success;
        let active_theme = self.settings.theme.clone();

        Some(
            div()
                .absolute()
                .top(px(48.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    v_flex()
                        .w(px(360.))
                        .max_w_full()
                        .overflow_hidden()
                        .p_2()
                        .gap_2()
                        // Same swallow-the-click-on-chrome reasoning as
                        // `render_pr_picker`.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .bg(popover)
                        .text_color(popover_fg)
                        .border_1()
                        .border_color(border)
                        .rounded_lg()
                        .shadow_lg()
                        .child(
                            div()
                                .px_2()
                                .pt_1()
                                .text_xs()
                                .text_color(muted)
                                .child("Theme \u{b7} enter to apply, esc to close"),
                        )
                        .child(
                            v_flex().w_full().children(themes::names().enumerate().map(
                                |(i, name)| {
                                    let selected = i == picker.selected;
                                    let is_active = name == active_theme;
                                    h_flex()
                                        .id(("theme-picker-row", i))
                                        .w_full()
                                        .gap_2()
                                        .px_2()
                                        .py_1()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .when(selected, |el| el.bg(accent))
                                        .hover(|el| el.bg(accent.opacity(0.5)))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                this.choose_theme(name, window, cx);
                                            }),
                                        )
                                        .child(
                                            div()
                                                .flex_none()
                                                .w(px(16.))
                                                .text_color(success)
                                                .child(if is_active { "\u{2713}" } else { "" }),
                                        )
                                        .child(div().flex_1().text_sm().child(name))
                                },
                            )),
                        ),
                ),
        )
    }
}

impl Render for AppShell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let recent_count = self.recent.entries().len();

        let active_title = self
            .selected
            .and_then(|i| self.recent.entries().get(i))
            .map(|e| e.title.clone());

        let has_active = self.active.is_some();
        let main: AnyElement = match &self.active {
            Some(workspace) => workspace.clone().into_any_element(),
            None => v_flex()
                .gap_2()
                .items_center()
                .child(div().text_lg().child("No review open"))
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child("Create a review to start browsing a diff (Ctrl+N)."),
                )
                .into_any_element(),
        };

        // While the theme picker is open the shell node carries an extra
        // identifier, flipping which key bindings apply (see `init`) — same
        // mechanism `workspace.rs` uses for its own overlays.
        let mut key_context = KEY_CONTEXT.to_string();
        if self.theme_picker.is_some() {
            key_context.push(' ');
            key_context.push_str(THEME_PICKER_CONTEXT);
        }

        v_flex()
            .size_full()
            .relative()
            .track_focus(&self.focus_handle)
            .key_context(key_context.as_str())
            .on_action(cx.listener(Self::on_new_review))
            .on_action(cx.listener(Self::on_refresh_badges))
            .on_action(cx.listener(Self::on_open_theme_picker))
            .on_action(cx.listener(Self::on_theme_picker_next))
            .on_action(cx.listener(Self::on_theme_picker_prev))
            .on_action(cx.listener(Self::on_theme_picker_close))
            .on_action(cx.listener(Self::on_theme_picker_choose))
            .child(
                TitleBar::new().child(
                    h_flex()
                        .gap_2()
                        .child(div().font_semibold().child("dv"))
                        .when_some(active_title, |el, title| {
                            el.child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("— {title}")),
                            )
                        }),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h(px(0.))
                    .items_start()
                    // Sidebar: the review navigator.
                    .child(
                        v_flex()
                            .h_full()
                            .w(px(280.))
                            .flex_none()
                            .border_r_1()
                            .border_color(theme.border)
                            .bg(theme.sidebar)
                            .child(
                                div().p_2().w_full().child(
                                    Button::new("new-review")
                                        .primary()
                                        .w_full()
                                        .label("New Review")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.on_new_review(&NewReview, window, cx)
                                        })),
                                ),
                            )
                            .child(
                                h_flex()
                                    .px_2()
                                    .py_1()
                                    .items_center()
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("RECENT"),
                                    )
                                    .child(
                                        // Manual badge refresh (docs/phase-3-github.md
                                        // deliverable 3) — same `refresh_all_badges`
                                        // the app-open pass uses, but never
                                        // WSL-skipped, since this is an explicit ask.
                                        Button::new("refresh-badges")
                                            .ghost()
                                            .xsmall()
                                            .label("\u{27f3}")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.on_refresh_badges(&RefreshBadges, window, cx)
                                            })),
                                    ),
                            )
                            .child(
                                uniform_list(
                                    "recent-list",
                                    recent_count,
                                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                        range
                                            .map(|i| {
                                                this.render_recent_row(i, cx).into_any_element()
                                            })
                                            .collect::<Vec<_>>()
                                    }),
                                )
                                .flex_1()
                                .px_1(),
                            ),
                    )
                    .child(
                        div()
                            .h_full()
                            .flex_1()
                            .min_w(px(0.))
                            // The active review fills this area itself; the
                            // empty-state message is centered by the wrapper.
                            .when(!has_active, |el| el.flex().items_center().justify_center())
                            .child(main),
                    ),
            )
            .children(self.render_theme_picker(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::{ChecksSummary, PrBadge, PrState, ReviewBadge, merge_local_badge};

    fn badge(open: usize, pr: Option<PrBadge>) -> ReviewBadge {
        ReviewBadge {
            open,
            submitted: false,
            pr,
        }
    }

    fn pr_badge() -> PrBadge {
        PrBadge {
            state: PrState::Open,
            is_draft: false,
            decision: None,
            checks: ChecksSummary::Passing,
        }
    }

    // --- merge_local_badge (review finding P3-3) ---------------------------

    #[test]
    fn merge_local_badge_fetch_remote_true_takes_fresh_local_verbatim() {
        let existing = Some(badge(1, Some(pr_badge())));
        let fresh_local = badge(3, None); // local pass never sets `pr` itself
        let merged = merge_local_badge(existing, fresh_local, true);
        assert_eq!(
            merged.open, 3,
            "open count must come from the fresh recompute"
        );
        assert!(
            merged.pr.is_none(),
            "fetch_remote=true means the caller is about to (re)fetch pr itself; \
             merge must not paper over that with the old value"
        );
    }

    #[test]
    fn merge_local_badge_fetch_remote_false_preserves_existing_pr() {
        let existing = Some(badge(1, Some(pr_badge())));
        let fresh_local = badge(2, None); // local recompute, no network run
        let merged = merge_local_badge(existing, fresh_local, false);
        assert_eq!(merged.open, 2, "local fields still refresh");
        assert!(
            merged.pr.is_some(),
            "fetch_remote=false must carry over the existing pr badge, not clobber it with None \
             (review finding P3-3 — this used to flicker the PR cluster off on every comment save)"
        );
    }

    #[test]
    fn merge_local_badge_fetch_remote_false_with_no_prior_pr_stays_none() {
        let merged = merge_local_badge(None, badge(1, None), false);
        assert!(
            merged.pr.is_none(),
            "nothing to preserve when there was no existing badge at all"
        );
    }
}
