//! The application shell: the review-navigator sidebar plus the main area
//! that hosts the active review. dv is review-centric — the sidebar is a
//! library of reviews across repos (see docs/ui-design.md).

use std::path::PathBuf;

use dv_core::provision::{ComponentId, ConsentAction, ConsistencyReport, consistency_check};
use dv_core::{
    ChecksSummary, DiffSource, GithubClient, PrState, RemoteRef, RepoLocation, RepoSlug,
    ReviewDecision,
};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::kbd::Kbd;
use gpui_component::tag::Tag;
use gpui_component::tooltip::Tooltip;
use gpui_component::{
    ActiveTheme, Selectable as _, Sizable as _, StyledExt, TitleBar, h_flex, v_flex,
};

use std::collections::{HashMap, HashSet, VecDeque};

use crate::onboarding::{OnboardingPage, RowState};
use crate::recent::RecentStore;
use crate::settings::{
    CONTEXT_LINES_MAX, CONTEXT_LINES_MIN, DEFAULT_SIDEBAR_WIDTH, MONO_FONT_SIZE_MAX,
    MONO_FONT_SIZE_MIN, SIDEBAR_WIDTH_MAX, SIDEBAR_WIDTH_MIN, Settings, SidebarFilters,
    SidebarGrouping, ViewModeSetting,
};
// Only consumed by `Self::set_summary_width` (automation-only, see below).
#[cfg(feature = "automation")]
use crate::settings::{SUMMARY_WIDTH_MAX, SUMMARY_WIDTH_MIN};
use crate::setup::SetupState;
use crate::themes;
use crate::workspace::{ReviewChanged, SummaryWidthChanged, Workspace};
// Only consumed by `Self::automation_state`'s "badges"/"index" dump.
#[cfg(feature = "automation")]
use crate::workspace::{checks_word, pr_state_word, review_decision_word, source_label};

actions!(
    shell,
    [
        NewReview,
        RefreshBadges,
        ToggleSidebar,
        OpenThemePicker,
        ThemePickerNext,
        ThemePickerPrev,
        ThemePickerClose,
        ThemePickerChoose,
        OpenSettings,
        SettingsClose,
        OpenOnboarding,
        OnboardingClose,
        ToggleArchiveReview,
        DeleteReviewPrompt,
        DeleteReviewConfirm,
        DeleteReviewCancel,
        // ctrl-k / cmd-k command palette (docs/backlog.md "one fuzzy
        // surface over commands AND destinations") — a fourth shell-level
        // overlay, same family as the theme picker/settings panel/
        // onboarding page above.
        OpenCommandPalette,
        CommandPaletteNext,
        CommandPalettePrev,
        CommandPaletteClose,
        CommandPaletteChoose
    ]
);

const KEY_CONTEXT: &str = "AppShell";
/// Stamped onto the shell's key context while the theme picker is open
/// (same mechanism `workspace.rs` uses for its palette/PR-picker overlays).
const THEME_PICKER_CONTEXT: &str = "ThemePickerOpen";
/// Same mechanism, for the settings panel (`ctrl-,`).
const SETTINGS_PANEL_CONTEXT: &str = "SettingsPanelOpen";
/// Same mechanism, for the onboarding page (S8e) — no keybinding opens it
/// (first-run/drift auto-open it, plus the sidebar's "Setup" button and
/// `--automation`'s `OpenOnboarding` action), but `escape` still needs a
/// context to bind against while it's up.
const ONBOARDING_CONTEXT: &str = "OnboardingOpen";
/// Same mechanism, for the delete-review confirmation modal (opened from a
/// review card's context menu — no keybinding opens it, but escape/enter
/// need a context to bind against while it's up).
const DELETE_CONFIRM_CONTEXT: &str = "DeleteConfirmOpen";
/// Same mechanism, for the ctrl-k/cmd-k command palette.
const COMMAND_PALETTE_CONTEXT: &str = "CommandPaletteOpen";

/// Fixed height, in px, of every sidebar row — both a review card
/// ([`AppShell::render_review_card`]) and a group header
/// ([`AppShell::render_sidebar_header`]). `uniform_list` measures exactly
/// one row (index 0 of whatever's currently visible) and applies that
/// height to every row's scroll math (see gpui's `elements/uniform_list.rs`
/// doc comment); once grouping (S6d) can put a `Header` at index 0, a
/// content-sized header that doesn't exactly match a content-sized card's
/// natural height would desync scrolling for the rest of the list. Fixing
/// both to the same explicit height sidesteps that outright rather than
/// trying to keep two different elements' natural sizes in lockstep.
const SIDEBAR_ROW_HEIGHT: f32 = 46.;

// Font-size/context-lines clamp bounds (`MONO_FONT_SIZE_MIN`/`_MAX`,
// `CONTEXT_LINES_MIN`/`_MAX`) live in `settings.rs` now — shared with
// `Settings::load`'s own re-clamp of a hand-edited settings.json, see its
// doc comment.

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
    let pr_number = latest.remote.as_ref().map(|r| r.pr);
    Some((
        ReviewBadge {
            open,
            submitted,
            pr: None,
            pr_number,
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
        // Only carry the cached `pr` forward when it's still for the same
        // PR `fresh_local` just determined is latest at this location —
        // otherwise a newer, different review becoming latest (a fresh
        // draft, or switching which PR is linked) would keep showing the
        // *previous* review's PR cluster under a `fetch_remote: false`
        // call, which never re-fetches to correct it (review finding P2's
        // root cause, generalized to the badge cache itself).
        pr: existing
            .filter(|b| b.pr_number == fresh_local.pr_number)
            .and_then(|b| b.pr),
        ..fresh_local
    }
}

/// Should the workspace's live conflict probe be stamped onto the index
/// entry for `review_source`? Yes exactly when the workspace is displaying
/// that review's KIND of diff (`std::mem::discriminant`), not when the two
/// sources are structurally equal — a reopened PR workspace shows a *fresh*
/// `DiffSource::Range` (new head/merge-base oids) while `review.source`
/// stays frozen at draft creation, so exact equality never re-converges and
/// hydration's older-head answer would win forever, letting the sidebar
/// badge contradict the open workspace's own header (review finding P2-1).
/// Same-kind with moved oids is still the same diff *kind*, and the live
/// probe is the fresher authority on it. The guard's motivating case keeps
/// working: a bare launch adopts a range review while the workspace shows
/// the working-tree diff — that `ls-files -u` probe is about a different
/// kind of diff entirely and must NOT stamp over hydration's persisted-
/// live-base range finding. Pure so the three-case matrix is unit-testable
/// (matches `merge_local_badge`'s pattern).
fn should_stamp_conflict(review_source: &DiffSource, ws_source: &DiffSource) -> bool {
    std::mem::discriminant(review_source) == std::mem::discriminant(ws_source)
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

/// `state` word for an [`dv_core::IndexEntry`]'s automation dump —
/// deliberately coarser than `Workspace::automation_state`'s own
/// `review.state` (`"draft"` / `"submitted:<verdict>"`, verdict included):
/// the index's automation surface only needs draft-vs-submitted for
/// docs/phase-6-review-navigator.md's S6b verification, and the verdict is
/// already available on `Workspace::automation_state` once that specific
/// review is the one open.
#[cfg(feature = "automation")]
fn index_state_word(state: &dv_core::ReviewState) -> &'static str {
    match state {
        dv_core::ReviewState::Draft => "draft",
        dv_core::ReviewState::Submitted { .. } => "submitted",
    }
}

/// `health` word for an [`dv_core::IndexEntry`]'s automation dump.
#[cfg(feature = "automation")]
fn index_health_word(health: dv_core::EntryHealth) -> &'static str {
    match health {
        dv_core::EntryHealth::Ok => "ok",
        dv_core::EntryHealth::RepoUnavailable => "repo_unavailable",
        dv_core::EntryHealth::Missing => "missing",
    }
}

/// `id` word for `Self::automation_state`'s `onboarding.rows[].id` — snake
/// case, matching this file's other `*_word` helpers' convention.
#[cfg(feature = "automation")]
fn component_id_word(id: ComponentId) -> &'static str {
    match id {
        ComponentId::GhCli => "gh_cli",
        ComponentId::DvOnPath => "dv_on_path",
        ComponentId::DvHost => "dv_host",
        ComponentId::DvCli => "dv_cli",
        ComponentId::NodeVtsls => "node_vtsls",
    }
}

/// `state` word for `Self::automation_state`'s `onboarding.rows[].state`.
#[cfg(feature = "automation")]
fn onboarding_row_state_word(state: &RowState) -> &'static str {
    match state {
        RowState::Checking => "checking",
        RowState::Ok(_) => "ok",
        RowState::Missing(_) => "missing",
        RowState::Consent(..) => "needs_consent",
        RowState::Installing => "installing",
        RowState::Failed(_) => "failed",
        RowState::Skipped(_) => "skipped",
    }
}

/// The human-readable detail string carried by every `RowState` variant
/// except `Checking`/`Installing` (which have none yet) — `Self::
/// automation_state`'s `onboarding.rows[].detail`.
#[cfg(feature = "automation")]
fn onboarding_row_detail(state: &RowState) -> Option<&str> {
    match state {
        RowState::Checking | RowState::Installing => None,
        RowState::Ok(s) | RowState::Missing(s) | RowState::Failed(s) | RowState::Skipped(s) => {
            Some(s.as_str())
        }
        RowState::Consent(_, detail) => Some(detail.as_str()),
    }
}

/// `sidebar_grouping` word for `Self::automation_state`'s `settings` dump.
#[cfg(feature = "automation")]
fn grouping_word(grouping: SidebarGrouping) -> &'static str {
    match grouping {
        SidebarGrouping::None => "none",
        SidebarGrouping::Repo => "repo",
        SidebarGrouping::Status => "status",
        SidebarGrouping::Pr => "pr",
    }
}

/// One row of [`AppShell::visible_sidebar_items`]: either a group header
/// (grouping is on) or a review card, referencing its entry by index into
/// [`dv_core::ReviewIndex::entries`] rather than cloning the whole
/// [`dv_core::IndexEntry`] — the list is rebuilt on every render, and an
/// index stays valid for exactly as long as `self.index` itself doesn't
/// mutate mid-render (true here: nothing between computing this list and
/// consuming it touches `self.index`).
///
/// `Header` carries both the group `key` and the display `label` — they can
/// legitimately diverge (`Pr` grouping keys on the full host/owner/repo/pr
/// slug but labels on the host-stripped display string), and
/// [`AppShell::render_sidebar_header`] must build its gpui element id from
/// the `key`, not the `label`: two distinct groups can share an identical
/// label (two GitHub Enterprise hosts with the same owner/repo/pr), and a
/// `uniform_list` with two siblings at the same element id is a duplicate
/// stateful-id hazard.
#[derive(Debug, Clone, PartialEq)]
enum SidebarItem {
    Header {
        key: SharedString,
        label: SharedString,
    },
    Review(usize),
}

/// Whether `entry` currently passes `settings.sidebar_filters`
/// (docs/phase-6-review-navigator.md deliverable 4) — an AND of two
/// independent axes:
/// - **PR status**: a local-only review (`entry.remote.is_none()`) passes
///   iff `filters.unlinked` — a distinct bucket, not folded into any of the
///   four PR-state bools (a local review isn't "no PR status", it's a
///   different axis entirely). A PR-linked review whose `pr_status` hasn't
///   hydrated yet (the network fetch is still in flight, or simply hasn't
///   run this session) **passes every PR-status filter** rather than being
///   hidden while its status is merely unknown — hiding-while-loading would
///   read as the review having vanished, exactly the incident this phase
///   exists to fix.
/// - **Review status**: the review's own `state`/`verdict` against the
///   `review_*` bools — draft counts as its own bucket (deliverable 4's
///   flagged addition), not folded into any submitted verdict.
/// - **Health**: an entry whose repo has gone `RepoUnavailable`/`Missing`
///   (S6a's `apply_hydration`) always passes, regardless of the PR/review
///   axes above. Its last-known state/verdict is preserved rather than
///   cleared, so an unrelated filter (e.g. hiding approved reviews) could
///   otherwise mask the row entirely — taking `render_review_card`'s
///   unavailable-repo glyph, the only on-screen signal of the problem, out
///   of view with it (P3 finding).
fn entry_passes_filters(entry: &dv_core::IndexEntry, filters: &SidebarFilters) -> bool {
    // Archived wins over everything, including the unavailable-repo
    // bypass below — the user explicitly tucked this review away, and an
    // unplugged drive shouldn't drag it back into view.
    if entry.archived && !filters.archived {
        return false;
    }
    if entry.health != dv_core::EntryHealth::Ok {
        return true;
    }
    let pr_ok = match &entry.remote {
        None => filters.unlinked,
        Some(_) => match &entry.pr_status {
            None => true,
            // `is_draft` only overrides the state-based bucket while the
            // PR is still open — GitHub requires marking a PR ready
            // before it can merge, but allows closing a still-draft PR
            // without ever marking it ready, so a *closed* draft is
            // representable. Letting `is_draft` win unconditionally would
            // bucket a closed draft under `pr_draft` forever, so toggling
            // `pr_closed` off/on could never hide/show it — bucket by the
            // terminal state instead once the PR isn't open anymore.
            Some(pr) => match pr.state {
                PrState::Open if pr.is_draft => filters.pr_draft,
                PrState::Open => filters.pr_open,
                PrState::Merged => filters.pr_merged,
                PrState::Closed => filters.pr_closed,
            },
        },
    };
    let review_ok = match &entry.state {
        dv_core::ReviewState::Draft => filters.review_draft,
        dv_core::ReviewState::Submitted { verdict, .. } => match verdict {
            dv_core::Verdict::Comment => filters.review_comment,
            dv_core::Verdict::Approve => filters.review_approved,
            dv_core::Verdict::RequestChanges => filters.review_changes,
        },
    };
    pr_ok && review_ok
}

/// Canonical repo identity for `SidebarGrouping::Repo` — every review
/// sharing a repo lands under exactly one header, regardless of how many
/// different PRs it's linked to or whether it's linked to one at all.
/// Deliberately ignores `remote` entirely (live-verified bug in an earlier
/// pass of this slice: passing `entry.remote` through to
/// [`dv_core::repo_label`] made a PR-linked review key off
/// `owner/repo` while a plain local review at the exact same
/// `RepoLocation` keyed off the folder name — same repo, two different
/// strings, so a repo with both a PR-linked review and a local-only one
/// split into two headers).
///
/// Keys on the **full** [`RepoLocation::display_name`], not on
/// `repo_label`'s basename-only display text. An earlier version of this
/// function keyed on `repo_label(location, None)` (`repo_short_name`
/// discards everything but the final path segment) specifically to absorb
/// drive-letter/parent-dir spelling differences between entries for the
/// SAME repo (binding orchestrator note: a location's separator/case
/// spelling can vary between entries for the same repo, and
/// `RepoLocation`'s `Eq` is spelling-sensitive on normal components, so
/// grouping on the raw value would split one repo into two groups). But a
/// basename is also just what an entirely *different* repo can happen to
/// be named — two unrelated local repos sharing a final path segment
/// (`D:\work\api` and `D:\clients\api`) collapsed into a single "api"
/// header with both repos' reviews mixed underneath (confirmed P3
/// finding). Keying on the full path fixes the over-merge while keeping
/// the anti-false-split property via the same normalization, applied to
/// the whole string instead of just the last segment.
///
/// Separator style is unified to `\` and, for `Local`, the result is
/// lowercased: Windows/macOS filesystems are case-insensitive and
/// case-preserving, so the exact same repo can be recorded across two
/// `RepoLocation::Local` entries differing only in case (a stale
/// `recent.json` seed vs. a freshly-typed or dialog-picked path, say).
/// `Wsl` locations keep their POSIX `path` byte-for-byte (no separator
/// rewrite, no folding — ext4 paths are genuinely case-sensitive, so
/// folding there would wrongly merge distinct repos), but the `distro`
/// segment IS lowercased: WSL distro registration is itself
/// case-insensitive at the OS level, so the same distro can show up
/// spelled differently across two entries (a `recent.json` seed vs. a
/// `\\wsl.localhost\<distro>\...` path picked from Explorer, or a
/// hand-typed `--wsl` arg) — left unfolded, that splits one repo into two
/// groups exactly like the Local case this function already guards
/// against. Only the *key* is normalized — the header label
/// (`visible_sidebar_items` tracks it separately, still derived from
/// `repo_label`'s basename for compactness) keeps its own display
/// spelling, so nothing on screen reads artificially lowercased or
/// full-path-verbose.
fn repo_group_key(location: &RepoLocation) -> String {
    match location {
        RepoLocation::Local(_) => location.display_name().replace('/', "\\").to_lowercase(),
        RepoLocation::Wsl { distro, path } => format!("{}:{}", distro.to_lowercase(), path),
    }
}

/// Canonical PR identity for `SidebarGrouping::Pr` — keys on the PR's full
/// `host/owner/repo#<pr>` slug (`RemoteRef::slug` is `host/owner/repo`),
/// never the host-stripped display string `dv_core::repo_label` produces.
/// Two different GitHub Enterprise hosts can coincidentally share an
/// identical owner/repo name and PR number (a multi-host GHE + github.com
/// setup); keying on the label alone would merge their reviews into one
/// group even though they're unrelated repos. The header text itself
/// still uses the host-stripped label (`visible_sidebar_items` tracks key
/// and label separately) — that's unchanged and can still render
/// identically for two such colliding repos, but each now gets its own
/// header with only its own reviews under it.
///
/// The slug itself is lowercased before keying: [`RepoSlug::parse_remote_url`]
/// only lowercases the host, so `remote.slug` preserves whatever owner/repo
/// casing was in the origin URL at the moment each review was created — but
/// GitHub's owner/repo path segments are themselves case-insensitive, so the
/// exact same PR can be recorded under two differently-cased slugs across
/// two reviews (e.g. one created while `origin` was `KyleKZ/difftest`,
/// another after the remote was normalized to `kylekz/difftest`). Left
/// unfolded, that splits one PR into two headers — the same failure mode
/// `repo_group_key` above is deliberately hardened against. Case-folding the
/// key only (the header label keeps its original casing) mirrors the
/// case-insensitive slug comparisons already used elsewhere (`workspace.rs`,
/// `main.rs`).
fn pr_group_key(location: &RepoLocation, remote: Option<&RemoteRef>) -> String {
    match remote {
        Some(remote) => format!("{}#{}", remote.slug.to_lowercase(), remote.pr),
        // No PR: falls back to the same per-repo bucket `Repo` grouping
        // would give it, including that bucket's case-folding.
        None => repo_group_key(location),
    }
}

/// `SidebarGrouping::Status`'s group-header word — one bucket per
/// review-status axis, the same four buckets `SidebarFilters`'s `review_*`
/// fields filter on (Draft / Comment / Approved / Changes Requested),
/// independent of any PR-status axis. Automation asserts these exact
/// strings (docs/phase-6-review-navigator.md S6d verification) — treat
/// them as a stable contract once shipped.
fn status_group_label(state: &dv_core::ReviewState) -> &'static str {
    match state {
        dv_core::ReviewState::Draft => "Draft",
        dv_core::ReviewState::Submitted {
            verdict: dv_core::Verdict::Comment,
            ..
        } => "Comment",
        dv_core::ReviewState::Submitted {
            verdict: dv_core::Verdict::Approve,
            ..
        } => "Approved",
        dv_core::ReviewState::Submitted {
            verdict: dv_core::Verdict::RequestChanges,
            ..
        } => "Changes Requested",
    }
}

/// Static table backing the filter popover's PR-status rows
/// ([`AppShell::render_sidebar_filter_popover`]) and the `"sidebar_filters"`
/// automation mirror: `(row id, label, getter, setter)`. A table rather
/// than nine hand-written rows/match arms — `get`/`set` are plain field
/// accessors (not closures), so this stays a `const` despite being built
/// from function "pointers".
type FilterAccessor = (
    &'static str,
    &'static str,
    fn(&SidebarFilters) -> bool,
    fn(&mut SidebarFilters, bool),
);
const PR_FILTER_ROWS: &[FilterAccessor] = &[
    (
        "sidebar-filter-pr-draft",
        "PR: draft",
        |f| f.pr_draft,
        |f, v| f.pr_draft = v,
    ),
    (
        "sidebar-filter-pr-open",
        "PR: open",
        |f| f.pr_open,
        |f, v| f.pr_open = v,
    ),
    (
        "sidebar-filter-pr-merged",
        "PR: merged",
        |f| f.pr_merged,
        |f, v| f.pr_merged = v,
    ),
    (
        "sidebar-filter-pr-closed",
        "PR: closed",
        |f| f.pr_closed,
        |f, v| f.pr_closed = v,
    ),
    (
        "sidebar-filter-unlinked",
        "Unlinked (local only)",
        |f| f.unlinked,
        |f, v| f.unlinked = v,
    ),
];
/// Same shape as [`PR_FILTER_ROWS`], for the review-status axis.
const REVIEW_FILTER_ROWS: &[FilterAccessor] = &[
    (
        "sidebar-filter-review-draft",
        "Review: draft",
        |f| f.review_draft,
        |f, v| f.review_draft = v,
    ),
    (
        "sidebar-filter-review-comment",
        "Review: comment",
        |f| f.review_comment,
        |f, v| f.review_comment = v,
    ),
    (
        "sidebar-filter-review-approved",
        "Review: approved",
        |f| f.review_approved,
        |f, v| f.review_approved = v,
    ),
    (
        "sidebar-filter-review-changes",
        "Review: changes requested",
        |f| f.review_changes,
        |f, v| f.review_changes = v,
    ),
    (
        "sidebar-filter-archived",
        "Archived",
        |f| f.archived,
        |f, v| f.archived = v,
    ),
];

/// Compact relative time for a review card's line-1 age (docs/phase-6-
/// review-navigator.md deliverable 2's sketch: "2h", "3d") — measured from
/// `updated_ms` (the review's own last activity, not when the user last
/// opened it in the sidebar; that's `last_opened_ms`, which only drives
/// sort order). No finer than a day beyond the first month, and no finer
/// than a month beyond the first year — a review this stale doesn't need
/// second-guessing to the hour.
pub(crate) fn relative_age(updated_ms: u64) -> String {
    let now = dv_core::review::now_ms();
    let secs = now.saturating_sub(updated_ms) / 1000;
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else if secs < 86_400 * 30 {
        format!("{}d", secs / 86_400)
    } else if secs < 86_400 * 365 {
        format!("{}mo", secs / (86_400 * 30))
    } else {
        format!("{}y", secs / (86_400 * 365))
    }
}

/// Absolute local timestamp for a review card's hover tooltip — the exact
/// moment `relative_age`'s "2h"/"3d" names (docs/backlog.md's Phase-6 S6c
/// deferral). That entry assumed gpui-component's `.tooltip()` was the only
/// option and gated `pub(crate)`; checked against the real pinned gpui
/// source (`~/.cargo/git/checkouts/zed-*/*/crates/gpui/src/elements/div.rs`)
/// instead of guessing, `.tooltip()` turns out to be a plain public
/// `StatefulInteractiveElement` method (needs `.id(...)`, nothing else) —
/// gpui-component's OWN `gpui_component::tooltip::Tooltip::new(text).build
/// (window, cx)` builds the `AnyView` it wants, also plain `pub`. No new
/// tooltip idiom needed. A malformed/out-of-range `ms` (shouldn't happen —
/// every caller sources it from a review's own `updated_ms`) falls back to
/// a label rather than panicking.
pub(crate) fn absolute_timestamp(ms: u64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(i64::try_from(ms).unwrap_or(i64::MAX)) {
        chrono::LocalResult::Single(dt) => dt.format("%b %-d, %Y, %-I:%M %p").to_string(),
        _ => "unknown time".to_string(),
    }
}

/// The one pill recipe every state indicator in the app renders through —
/// PR state, file status, review-decision and submitted-review markers
/// alike — so a color and a label always render identically: `Tag::custom
/// (color @ 0.15, color, color @ 0.4).small()`, the one badge/pill recipe.
/// `Tag`'s `Custom` variant uses `color` verbatim for the background (see
/// gpui-component's `TagVariant::bg`), so the caller passes the
/// *already-tinted* background, not the base hue.
pub(crate) fn state_pill(color: Hsla, label: impl Into<SharedString>) -> Tag {
    Tag::custom(color.opacity(0.15), color, color.opacity(0.4))
        .small()
        .child(label.into())
}

/// [`state_pill`] carrying an icon instead of text — for marks whose obvious
/// glyph the UI font doesn't reliably cover (U+2713 rendered as tofu inside
/// the small Tag; R1e visual review, P3). Same recipe, an `Icon` child.
pub(crate) fn state_pill_icon(color: Hsla, icon: gpui_component::IconName) -> Tag {
    Tag::custom(color.opacity(0.15), color, color.opacity(0.4))
        .small()
        .child(gpui_component::Icon::new(icon).xsmall())
}

/// PR state pill label + color (draft/open/merged/closed), shared by the
/// sidebar's PR-status cluster ([`render_pr_glyphs`]) and the workspace's
/// title-bar-anatomy header (`Workspace::render_header`, R1c) — both used to
/// carry their own copy of this exact match, and a review finding (R1b) flagged
/// the duplication risk before a third call site (R1c) made it worth fixing.
/// Color mapping: open=success,
/// merged=`accent_alt` (the deliberate "purple link" hue, shared with the
/// renamed-file pill in `Workspace::render_file_row`), closed=danger,
/// draft=muted — draft takes precedence over `state` when both are true,
/// matching a real GitHub PR (a draft is always reported as `OPEN`).
pub(crate) fn pr_state_pill(
    is_draft: bool,
    state: PrState,
    muted: Hsla,
    success: Hsla,
    danger: Hsla,
    accent_alt: Hsla,
) -> (&'static str, Hsla) {
    if is_draft {
        ("draft", muted)
    } else {
        match state {
            PrState::Open => ("open", success),
            PrState::Merged => ("merged", accent_alt),
            PrState::Closed => ("closed", danger),
        }
    }
}

/// Sidebar PR-status pill cluster: state (draft/open/merged/closed),
/// review-decision marker, CI marker. Colors follow the shared badge/pill
/// mapping (see [`pr_state_pill`] for the state pill itself).
/// The decision and CI markers keep their own compact glyphs (✓/±/▪) rather
/// than spelling out full words — unlike the title bar (R1c), which spells
/// `approved`/`changes requested` out in full, this cluster can carry three
/// pills at once in a two-line sidebar card, so content stays short by
/// design (the slice's own flagged risk: pill weight overpowering a dense
/// row).
#[allow(clippy::too_many_arguments)]
fn render_pr_glyphs(
    is_draft: bool,
    state: PrState,
    decision: Option<ReviewDecision>,
    checks: ChecksSummary,
    muted: Hsla,
    success: Hsla,
    danger: Hsla,
    warning: Hsla,
    accent_alt: Hsla,
) -> impl IntoElement {
    let (state_label, state_color) =
        pr_state_pill(is_draft, state, muted, success, danger, accent_alt);
    // Approved renders an `Icon` check, not U+2713 text — the UI font
    // tofu'd the glyph at Tag size (R1e visual review, P3). "±" stays text:
    // Latin-1, universally covered.
    let decision_pill = match decision {
        Some(ReviewDecision::Approved) => Some((None, success)),
        Some(ReviewDecision::ChangesRequested) => Some((Some("\u{b1}"), danger)),
        Some(ReviewDecision::ReviewRequired) | None => None,
    };
    let ci_color = match checks {
        ChecksSummary::Passing => Some(success),
        ChecksSummary::Failing => Some(danger),
        ChecksSummary::Pending => Some(warning),
        ChecksSummary::None => None,
    };
    h_flex()
        .id("pr-badge")
        .flex_none()
        .gap_1()
        .items_center()
        .child(state_pill(state_color, state_label))
        .children(decision_pill.map(|(glyph, color)| match glyph {
            Some(glyph) => state_pill(color, glyph),
            None => state_pill_icon(color, gpui_component::IconName::Check),
        }))
        // Small square (▪), not a dot — kept from the old glyph cluster so
        // the CI marker never reads as a second, indistinguishable copy of
        // the state pill's own color (review finding P3-2, from the retired
        // per-repo `render_recent_row`; this is that same cluster).
        .children(ci_color.map(|color| state_pill(color, "\u{25aa}")))
}

pub fn init(cx: &mut App) {
    let shell = Some(KEY_CONTEXT);
    let theme_picker = Some("AppShell && ThemePickerOpen");
    let settings_panel = Some("AppShell && SettingsPanelOpen");
    let onboarding = Some("AppShell && OnboardingOpen");
    cx.bind_keys([
        KeyBinding::new("cmd-n", NewReview, shell),
        KeyBinding::new("ctrl-n", NewReview, shell),
        // cmd- twins added S8i (docs/phase-8-lsp-and-polish.md §macOS) —
        // mirror the existing cmd-n/ctrl-n pair above. Additive only: the
        // ctrl- bindings are unchanged, so Windows/Linux behavior doesn't
        // move.
        KeyBinding::new("cmd-shift-t", OpenThemePicker, shell),
        KeyBinding::new("ctrl-shift-t", OpenThemePicker, shell),
        KeyBinding::new("cmd-,", OpenSettings, shell),
        KeyBinding::new("ctrl-,", OpenSettings, shell),
        // Sidebar hide/show (R2) — the editor-world
        // ctrl-b convention; cmd- twin per the S8i macOS pairing above.
        KeyBinding::new("cmd-b", ToggleSidebar, shell),
        KeyBinding::new("ctrl-b", ToggleSidebar, shell),
        // ctrl-k / cmd-k command palette — same cmd-/ctrl- pairing every
        // other shell-level binding above already uses.
        KeyBinding::new("cmd-k", OpenCommandPalette, shell),
        KeyBinding::new("ctrl-k", OpenCommandPalette, shell),
    ]);
    cx.bind_keys([
        KeyBinding::new("down", ThemePickerNext, theme_picker),
        KeyBinding::new("up", ThemePickerPrev, theme_picker),
        KeyBinding::new("escape", ThemePickerClose, theme_picker),
        KeyBinding::new("enter", ThemePickerChoose, theme_picker),
    ]);
    cx.bind_keys([KeyBinding::new("escape", SettingsClose, settings_panel)]);
    cx.bind_keys([KeyBinding::new("escape", OnboardingClose, onboarding)]);
    let delete_confirm = Some("AppShell && DeleteConfirmOpen");
    cx.bind_keys([
        KeyBinding::new("escape", DeleteReviewCancel, delete_confirm),
        KeyBinding::new("enter", DeleteReviewConfirm, delete_confirm),
    ]);
    let command_palette = Some("AppShell && CommandPaletteOpen");
    cx.bind_keys([
        KeyBinding::new("down", CommandPaletteNext, command_palette),
        KeyBinding::new("up", CommandPalettePrev, command_palette),
        KeyBinding::new("escape", CommandPaletteClose, command_palette),
        KeyBinding::new("enter", CommandPaletteChoose, command_palette),
    ]);
}

/// Maximum number of recently-active workspaces the [`WorkspaceCache`] LRU
/// keeps alive at once (Phase 7 D1). Also caps concurrent parked
/// store/worktree watchers — each cached entry's `_watcher`/
/// `_worktree_watcher` stays subscribed while parked (see
/// `AppShell::stash_active`), so this is effectively the app's concurrent
/// WSL-host-watch ceiling too, well within the host's per-distro cap.
const MAX_CACHED_WORKSPACES: usize = 6;
/// Maximum total estimated rendered-diff bytes (`Workspace::
/// estimated_diff_bytes`) across every cached (parked) workspace — a
/// handful of huge diffs must not blow the memory budget just because
/// they fit under the count cap above.
const MAX_CACHE_BYTES: usize = 128 * 1024 * 1024;

/// Retains recently-active-but-not-shown workspaces so switching back to
/// one paints instantly (Phase 7 deliverable 1) instead of rebuilding from
/// zero via `Workspace::new`. Strict LRU with a DUAL cap: never more than
/// `max_entries` entries AND never more than `max_bytes` of estimated
/// rendered-diff bytes — see [`Self::evict_to_budget`]. Eviction drops the
/// entry, and with it the LAST strong `Entity<Workspace>` ref for a parked
/// workspace, tearing down its store/worktree watchers via `Drop`
/// (cross-cutting risk A — `AppShell::active` is the only other place a
/// strong ref to a live `Workspace` lives, so between the two there is
/// never a workspace with zero or two owners). Keyed by review id, not
/// `RepoLocation` — sidesteps the case-canonicalization hazard the Phase-6
/// review index already learned to avoid (cross-cutting risk E; see
/// `AppShell::stash_active`'s doc comment).
struct WorkspaceCache {
    entries: HashMap<String, CachedWorkspace>,
    /// Front = most-recently used, back = evict next.
    lru: VecDeque<String>,
    max_entries: usize,
    max_bytes: usize,
}

/// One parked workspace kept alive by the [`WorkspaceCache`].
struct CachedWorkspace {
    entity: Entity<Workspace>,
    /// Keeps this PARKED workspace's `ReviewChanged` feeding badges + the
    /// review index while it isn't active — the single-slot
    /// `AppShell::_ws_subscription`/`_ws_summary_subscription` only ever
    /// track the ACTIVE workspace (cross-cutting risk B: `Subscription` is
    /// a single RAII guard, not a registry, so each parked entry needs its
    /// own). See `AppShell::on_cached_review_changed`.
    _sub: Subscription,
    /// Re-enforces the LRU byte budget when a diff computation dispatched
    /// before parking lands its rows on this PARKED entity (capstone P3-8:
    /// stage-2 recolors — and stage-1 rows — arriving after `stash_active`
    /// grew the cache past what its `evict_to_budget` accounted, with no
    /// re-enforcement until the next switch). See
    /// `AppShell::on_cached_diff_bytes_changed`.
    _bytes_sub: Subscription,
}

impl WorkspaceCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            lru: VecDeque::new(),
            max_entries,
            max_bytes,
        }
    }

    /// Remove and return `key`'s cached entry, if any — the reactivation
    /// path (`AppShell::open_review`'s cache-hit fast path). Drops it out
    /// of LRU order too; the caller either reinstalls it as active right
    /// away or lets it fall out of the cache entirely.
    fn take(&mut self, key: &str) -> Option<CachedWorkspace> {
        let entry = self.entries.remove(key)?;
        self.lru.retain(|k| k != key);
        Some(entry)
    }

    /// Park `entry` under `key` as most-recently-used.
    fn insert(&mut self, key: String, entry: CachedWorkspace) {
        self.lru.retain(|k| k != &key);
        self.lru.push_front(key.clone());
        self.entries.insert(key, entry);
    }

    /// Move the entry stored under `old_key` to `new_key` in place —
    /// same entity, corrected identity (cross-cutting risk E; see
    /// `AppShell::on_cached_review_changed`, the only caller). LRU
    /// position is preserved rather than bumped to MRU: this corrects a
    /// stale mapping, it isn't a fresh access.
    ///
    /// A PARKED entity's own store watcher can drift it onto a review id
    /// that's ALSO already parked here (two drafts of the same repo, both
    /// cached, one gets externally deleted and the other's watcher
    /// re-resolves to the id the sibling already occupies — review
    /// finding). Without dropping that pre-existing occupant first, the
    /// plain `entries.insert` below would silently overwrite (and leak —
    /// its `_sub`/entity torn down outside `evict_to_budget`'s accounting)
    /// the sibling, while the `lru` rewrite loop leaves TWO occurrences of
    /// `new_key` against a single `entries` slot, permanently desyncing
    /// `lru.len()` from `entries.len()`. `drop_stale` is the same teardown
    /// `AppShell::install_active`'s active-side collision already uses for
    /// exactly this "resolves to an id already parked" case.
    fn rekey(&mut self, old_key: String, new_key: String) {
        let Some(entry) = self.entries.remove(&old_key) else {
            return;
        };
        if new_key != old_key {
            self.drop_stale(&new_key);
        }
        for k in self.lru.iter_mut() {
            if *k == old_key {
                *k = new_key.clone();
            }
        }
        self.entries.insert(new_key, entry);
    }

    /// Drop `key`'s cached entry, if any, without returning it — used when
    /// the active workspace resolves to a review id that's ALSO (stale-)
    /// parked here: a non-pinned open (folder picker / `dv pr`) skips the
    /// pinned-only cache-hit fast path in `AppShell::open_review`, so it
    /// can build a brand-new live `Workspace` for a review that's already
    /// sitting cached from an earlier switch-away (P3 review finding). Two
    /// live entities would otherwise both watch the same review id until
    /// the next unrelated switch-away silently overwrote one (cross-cutting
    /// risk A: one strong-ref owner per review) — this closes that window
    /// as soon as the duplication is detected, from
    /// `AppShell::install_active`'s `ReviewChanged` closure.
    fn drop_stale(&mut self, key: &str) {
        if self.entries.remove(key).is_some() {
            self.lru.retain(|k| k != key);
        }
    }

    /// Estimated total bytes across every cached entry's rendered diffs
    /// (cross-cutting risk F — `Workspace::estimated_diff_bytes` already
    /// counts both unified and split rows per entry).
    fn total_bytes(&self, cx: &App) -> usize {
        self.entries
            .values()
            .map(|e| e.entity.read(cx).estimated_diff_bytes())
            .sum()
    }

    /// Every parked entity, for fanning a global settings change (theme,
    /// context lines, font size) out to workspaces that aren't currently
    /// active. Without this, a setting changed while a workspace sits
    /// parked never reaches it — its `RenderedDiff` cache keeps whatever
    /// colors/hunk-structure it was baked with, and its `context_lines`/
    /// `font_size` fields stay frozen — so reactivating it later paints
    /// stale content under the *new* global theme (review finding: visibly
    /// wrong, potentially low-contrast, until an unrelated file-select
    /// happens to recompute it). Cloned rather than borrowed: the caller
    /// needs to call back into each entity with the very `cx` this method
    /// would otherwise have to borrow from `self` to iterate.
    fn cached_entities(&self) -> Vec<Entity<Workspace>> {
        self.entries.values().map(|e| e.entity.clone()).collect()
    }

    /// Evict least-recently-used entries until both the count and byte
    /// budgets are satisfied. Each `entries.remove` here is the LAST strong
    /// `Entity<Workspace>` drop for that parked workspace (cross-cutting
    /// risk A) — its watchers tear down right here, which is correct: an
    /// evicted workspace shouldn't keep an OS/host watch running for a
    /// review nobody can instantly return to anymore.
    ///
    /// `total_bytes` is computed once up front rather than re-summed on
    /// every loop condition check — this runs off `AppShell::stash_active`
    /// on essentially every switch (the sub-50ms path this slice exists
    /// for), and re-walking every row of every cached `RenderedDiff` just
    /// to confirm the byte budget is satisfied — even when nothing needs
    /// evicting — was an avoidable full-cache traversal (P3 review
    /// finding). Decremented locally as entries are popped instead.
    fn evict_to_budget(&mut self, cx: &App) {
        let mut bytes = self.total_bytes(cx);
        while self.lru.len() > self.max_entries || bytes > self.max_bytes {
            let Some(key) = self.lru.pop_back() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                bytes = bytes.saturating_sub(entry.entity.read(cx).estimated_diff_bytes());
            }
        }
    }
}

/// State of the delete-review confirmation modal (see
/// [`AppShell::delete_confirm`]).
struct DeleteConfirm {
    review_id: String,
    /// The store delete is running on the background executor; the
    /// modal's Delete button is disabled meanwhile.
    in_flight: bool,
    error: Option<String>,
}

pub struct AppShell {
    focus_handle: FocusHandle,
    recent: RecentStore,
    active: Option<Entity<Workspace>>,
    /// The active review's id, for sidebar-row highlighting and
    /// [`Self::open_review_row`]'s pin (docs/phase-6-review-navigator.md
    /// S6c). Identity, not position: once the sidebar renders from
    /// [`Self::index`] instead of the recency list, a row's on-screen index
    /// is no longer stable (grouping/filtering in S6d can reorder or hide
    /// rows entirely) — the review's own id is the only thing worth
    /// tracking. Set the instant an explicit pin is known (a row click,
    /// `{"cmd":"select_review"}`); for an open with no pin (a brand-new
    /// repo, `dv pr <n>`'s `pending_pr`), it's `None` until the workspace's
    /// own `ReviewChanged` reports which review actually landed (see
    /// `_ws_subscription`, below).
    selected_review_id: Option<String>,
    /// The review a card context menu was last opened over: stashed by the
    /// card's right-mouse-down (which fires alongside the `ContextMenuExt`
    /// machinery), read by the menu items' unit actions
    /// (`ToggleArchiveReview`/`DeleteReviewPrompt`). Identity by id, same
    /// rationale as [`Self::selected_review_id`].
    menu_review: Option<String>,
    /// The delete-review confirmation modal, when open (context menu →
    /// "Delete review…"). The modal owns the whole delete flow: it stays
    /// up while the store delete runs (`in_flight`) and shows a failure
    /// (e.g. a WSL distro that stopped) instead of pretending the review
    /// is gone.
    delete_confirm: Option<DeleteConfirm>,
    /// True under `--automation`: blocks the native folder picker, which
    /// would wedge the foreground executor (and thus the whole automation
    /// channel) until a human dismissed it.
    automation: bool,
    /// Per-repo review badge (latest review's open-comment count /
    /// submitted flag), refreshed off-thread. Keyed by location, not list
    /// index — the recent list shifts when new entries insert at the top
    /// (review finding: index keys wore the wrong rows' badges).
    badges: HashMap<RepoLocation, ReviewBadge>,
    /// Cross-repo review index (docs/phase-6-review-navigator.md
    /// deliverable 1): every review dv has ever hydrated, across every
    /// repo, cached at `<data_dir>/dv/review_index.json`. Loaded
    /// synchronously in [`Self::new`] so the sidebar's *eventual*
    /// review-centric render (S6c) can paint instantly from the cached
    /// file before a single `ReviewStore::list()` call has run; kept
    /// fresh off-thread by [`Self::hydrate_index`]/
    /// [`Self::hydrate_index_location`] and by the `ReviewChanged`
    /// subscription in [`Self::open_review`]. This slice (S6b) only
    /// populates and exposes it via [`Self::automation_state`] — sidebar
    /// rendering itself is unchanged until S6c.
    index: dv_core::ReviewIndex,
    /// Per-location monotonic sequence, guarding [`Self::hydrate_index_location`]
    /// against out-of-order completion: `cx.background_executor()` is a real
    /// multi-threaded pool, so two overlapping hydration passes for the same
    /// location (e.g. `open_review`'s own call racing the broader
    /// `hydrate_index` walk, or two `ReviewChanged` events in quick
    /// succession) are not guaranteed to *complete* in the order they were
    /// dispatched. `apply_hydration` does a full-outcome replace, so an
    /// earlier-dispatched read that completes last would silently overwrite
    /// fresher data with a stale snapshot. Bumped and captured at dispatch
    /// time; a completion only applies if its captured value still matches
    /// (mirrors `Workspace::source_epoch`'s discard-if-superseded pattern).
    index_hydration_gens: HashMap<RepoLocation, u64>,
    /// True while a sidebar-handle drag is in progress (set by the first
    /// drag-move frame). `on_mouse_up_out` fires on ANY left release outside
    /// the handle's hitbox — without this gate every click in the window
    /// would rewrite settings.json.
    sidebar_dragging: bool,
    /// True while the sidebar's filter popover (docs/phase-6-review-
    /// navigator.md deliverable 4) is open. Pure mouse — every row is a
    /// click target, nothing here binds a key — so unlike the theme
    /// picker/settings panel overlays, opening this never needs to
    /// `window.focus` anything onto the shell (cross-cutting risk E: no
    /// key bindings means no dispatch-path ambiguity to sidestep).
    filter_popover_open: bool,
    /// The "open anything" quick-open input pinned above the sidebar list
    /// (R2): a PR URL / `owner/repo#123` / `#123` /
    /// filesystem path, parsed by `dv_core::parse_open_target` on Enter
    /// (see [`Self::quick_open_submit`]).
    quick_open: Entity<InputState>,
    /// Keeps the quick-open input's Enter/Change subscription alive.
    _quick_open_subscription: Subscription,
    /// Inline error under the quick-open input. Cleared by the next edit
    /// (`InputEvent::Change`) or by clicking the error line itself.
    quick_open_error: Option<SharedString>,
    /// Keeps the active workspace's ReviewChanged subscription alive.
    _ws_subscription: Option<Subscription>,
    /// Keeps the active workspace's `SummaryWidthChanged` subscription
    /// alive — the summary panel's own drag handle lives inside
    /// `Workspace`'s render tree and live-updates its width directly, but
    /// `Settings` (and thus persistence) lives here, so a drag-release
    /// emits this event rather than reaching back into the shell directly
    /// (see `render_sidebar_resize_handle`'s doc comment for the mirror-image
    /// sidebar case, which needs no such round trip since it's shell-owned
    /// end to end).
    _ws_summary_subscription: Option<Subscription>,
    /// Persisted app-wide settings. Loaded once at startup; updated and
    /// re-saved on every theme-picker / settings-panel change (and by
    /// `--automation`'s `set_setting`).
    settings: Settings,
    /// The theme-picker overlay (`ctrl-shift-t`), when open. Unlike the
    /// PR picker there's nothing to load — the registry is a static list —
    /// so this is just a cursor into `themes::names()`.
    theme_picker: Option<ThemePicker>,
    /// The ctrl-k/cmd-k command palette, when open (docs/backlog.md "one
    /// fuzzy surface over commands AND destinations"). A fifth shell-level
    /// overlay in the same mutual-exclusion family as `theme_picker`/
    /// `settings_panel`/`onboarding`/`delete_confirm` above.
    command_palette: Option<CommandPalette>,
    /// The settings panel (`ctrl-,`), when open.
    settings_panel: Option<SettingsPanel>,
    /// Keeps the window's OS-appearance observer alive — re-resolves the
    /// active theme on every live OS light/dark flip while
    /// `follow_os_appearance` is on (see `Self::resolve_follow_os`).
    /// Startup's own resolution happens in `main.rs` (via `cx.window_appearance()`,
    /// before any window/shell exists), *not* through this subscription —
    /// `Window::observe_window_appearance`'s registration never invokes the
    /// callback itself (see its construction site in `Self::new`), so
    /// startup needed its own resolution anyway.
    _appearance_subscription: Option<Subscription>,
    /// Wall-clock of the most recent [`Self::open_review`]'s synchronous
    /// body (dispatch to the `self.active` assignment) — Phase 7 D4
    /// instrumentation (mirrors `Workspace::last_diff_ms`). A warm switch
    /// (S7-3) does all its content work synchronously, so a small value
    /// here is the "no flash" signal; a cold switch defers to an async
    /// load, so this alone doesn't mean the workspace has finished loading
    /// (check `workspace.settled`/`workspace.status` for that).
    last_switch_ms: Option<u64>,
    /// Whether the most recent [`Self::open_review`] reactivated a parked
    /// [`WorkspaceCache`] entry (`true`) or built a fresh `Workspace` from
    /// zero (`false`) — Phase 7 D1/D4: the signal `last_switch_ms` alone
    /// can't give, since a cache MISS can still finish its synchronous body
    /// quickly (the async load just hasn't landed yet). `None` only before
    /// the first ever `open_review` call.
    last_switch_cache_hit: Option<bool>,
    /// LRU of recently-active workspaces, kept alive so re-selecting one
    /// paints instantly (Phase 7 deliverable 1). See [`WorkspaceCache`]'s
    /// doc comment for the eviction policy.
    workspace_cache: WorkspaceCache,
    /// The onboarding overlay (S8e), when open — shown on true first run
    /// (`SetupState::is_first_run`) and auto-surfaced later whenever a
    /// consistency check finds something needing a human (see
    /// [`Self::apply_consistency_report`]).
    onboarding: Option<OnboardingPage>,
    /// Monotonic guard against out-of-order [`consistency_check`]
    /// completions — the same reasoning as `index_hydration_gens` (a launch
    /// check and an `open_review` check for the same distro can overlap on
    /// `cx.background_executor()`'s real thread pool with no ordering
    /// guarantee). Bumped and captured at dispatch time in
    /// [`Self::spawn_consistency_check`]; a completion only applies if its
    /// captured value still matches.
    onboarding_check_gen: u64,
    /// Session-scoped "the user has already seen and closed a drift-
    /// surfaced onboarding page" latch (S8e review, P2). Without this, every
    /// subsequent per-launch/`open_review` consistency check that still
    /// finds drift (a declined vtsls consent, an unauthenticated `gh`, ...)
    /// re-opens the page in [`Self::apply_consistency_report`]'s
    /// already-closed branch, even for re-selecting the already-active
    /// review — the user can never permanently decline. Set only by
    /// [`Self::close_onboarding_page`] when the page it's closing actually
    /// showed a row that needed a human (`OnboardingPage::
    /// needs_human_fingerprint` returning `Some` — S8e review, P2: closing
    /// a drift-free page, e.g. the first-run welcome page with no WSL
    /// distro live, must NOT suppress a later, genuinely different drift),
    /// and cleared only by an explicit manual reopen
    /// ([`Self::open_onboarding_page`]), so a deliberate look re-arms the
    /// auto-surface for that fresh session of checks. This latch alone is
    /// SESSION-only — `crate::setup::SetupState::dismissed_drift_fingerprints`
    /// is its cross-launch counterpart (phase-8 capstone review, P3; a SET
    /// of fingerprints rather than one slot, capstone integration review,
    /// P3, since a multi-distro machine can have more than one
    /// simultaneously-valid dismissed shape), consulted by
    /// [`Self::apply_consistency_report`] alongside this field rather than
    /// replacing it.
    drift_page_dismissed: bool,
    /// Titles ([`OnboardingPage::installing`]'s merge key) with a
    /// consent-triggered `install_vtsls` genuinely in flight right now,
    /// persisted here at the SHELL level rather than only on the
    /// [`OnboardingPage`] itself (S8e review, P3). The page is rebuilt from
    /// scratch on every open/reopen ([`Self::open_onboarding_page`]), so a
    /// page-only marker forgets an install that outlives a close/reopen
    /// (up to 180s) — the reopened page's fresh check re-detects the
    /// half-written `npm install` as `NeedsConsent` and a second click would
    /// start a second concurrent install into the same distro. Every new
    /// page is seeded from this set ([`OnboardingPage::seed_installing`]);
    /// entries are added in [`Self::on_onboarding_consent_install`] and
    /// removed once that install's own background task completes (success
    /// or failure).
    onboarding_installing: HashSet<String>,
    /// Dispatch-time timestamp of the last [`Self::spawn_consistency_check`]
    /// call that included a given key (phase-8 capstone review, P3) — a
    /// distro name for its per-distro rows, or [`GH_COOLDOWN_KEY`] for the
    /// unconditional `gh` row every check includes. Consulted only by
    /// [`Self::spawn_consistency_check_throttled`] (the passive/high-
    /// frequency callers: launch and `Self::open_review`'s per-repo-open
    /// trigger) to collapse a redundant re-check within
    /// [`CONSISTENCY_COOLDOWN`] — without this, rapidly triaging many WSL
    /// reviews via the Phase-7 ~2ms cached switch fired one full check
    /// (a `gh auth status` network round trip + several `wsl.exe` spawns)
    /// per switch, with no dedup. A manual page (re)open
    /// ([`Self::open_onboarding_page`]) and the post-install reverify
    /// ([`Self::on_onboarding_consent_install`]) both call
    /// [`Self::spawn_consistency_check`] directly, bypassing the cooldown
    /// check (both must always see a fresh result) — but still stamp this
    /// map via that shared fn, so a passive check right after either
    /// doesn't immediately re-hit the same ground.
    consistency_checked_at: HashMap<String, std::time::Instant>,
}

/// Sentinel key for `gh`'s entry in [`AppShell::consistency_checked_at`] —
/// `gh` isn't distro-scoped (it always runs host-side, unconditionally, per
/// `consistency_check`'s own module doc), so it needs a slot outside the
/// real distro names that key every other entry in that map.
const GH_COOLDOWN_KEY: &str = "\0gh";

/// How long [`AppShell::spawn_consistency_check_throttled`] treats a prior
/// check as still good enough to skip a passive re-check. Generous enough
/// to collapse a rapid multi-review triage session into one real check per
/// distro, short enough that genuine drift (installing vtsls, `gh auth
/// login`/`logout`) still surfaces well within a sitting.
const CONSISTENCY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// The theme picker overlay, while open.
struct ThemePicker {
    /// Cursor into `themes::names()`.
    selected: usize,
}

/// The ctrl-k/cmd-k command palette overlay, while open. Unlike the theme
/// picker/jump-to-file palette (a static list / one workspace's files),
/// this one's candidate set spans commands + reviews + files + PRs and can
/// change out from under the query (a background PR-list revalidation, a
/// live `ReviewChanged`) — so `items` is the already-ranked snapshot for
/// the CURRENT `input` text, rebuilt on every `InputEvent::Change`
/// (`AppShell::recompute_command_palette`) rather than re-derived on every
/// render.
struct CommandPalette {
    input: Entity<InputState>,
    _subscription: Subscription,
    /// Ranked, mixed results for `input`'s current text — see
    /// [`crate::command_palette::rank_and_mix`]. Rebuilt wholesale on every
    /// change rather than diffed; the candidate lists involved (a few dozen
    /// commands, a handful of reviews/files/PRs) are far too small for that
    /// to matter.
    items: Vec<PaletteRow>,
    /// Cursor into `items`.
    selected: usize,
}

/// One already-ranked palette row, owned (not borrowed) so it can outlive
/// the `Vec<PaletteCandidate>` scratch list `recompute_command_palette`
/// builds and ranks each keystroke — see that function's doc comment for
/// why lifetime-borrowing `crate::command_palette::rank_and_mix`'s output
/// straight into `CommandPalette::items` doesn't work here (it would tie
/// `CommandPalette` to a borrow of a temporary).
struct PaletteRow {
    kind: crate::command_palette::PaletteKind,
    id: String,
    label: SharedString,
    subtitle: SharedString,
}

impl From<&crate::command_palette::PaletteCandidate> for PaletteRow {
    fn from(c: &crate::command_palette::PaletteCandidate) -> Self {
        Self {
            kind: c.kind,
            id: c.id.clone(),
            label: c.label.clone().into(),
            subtitle: c.subtitle.clone().into(),
        }
    }
}

/// Real, live keybinding text for `action_name` (e.g. `"workspace::
/// ToggleSplit"`), for the command palette's Commands-group subtitle — task
/// requirement: "surface real bindings, not hardcoded strings". Builds the
/// action (same `cx.build_action` automation's own `Cmd::Action` dispatch
/// uses) and looks it up in the app-wide keymap directly
/// ([`gpui::App::key_bindings`]/[`gpui::Keymap::bindings_for_action`]) —
/// deliberately NOT `Window::bindings_for_action` (used by gpui-component's
/// own `Kbd::binding_for_action`), which filters by the CURRENTLY FOCUSED
/// node's context stack: while the palette itself has focus, that stack is
/// "AppShell && CommandPaletteOpen", which would report every workspace-
/// scoped binding (bound to `"Workspace && ..."` predicates, see
/// `workspace::init`) as unbound. The bare keymap has no such blind spot —
/// it matches on the action's TYPE only, regardless of what's focused right
/// now, which is exactly "what key would invoke this, in general".
/// `bindings_for_action`'s own doc comment says the LAST-registered binding
/// should win for display purposes when a namespace multi-binds
/// cmd-and-ctrl twins (S8i's convention throughout `workspace::init`/
/// `Self::init`: cmd- registered first, ctrl- second) — `.last()` here
/// prefers the ctrl- form, the right one to show on this non-mac dev
/// machine. `None` when the action has no binding at all (not every
/// command needs one).
fn keybinding_hint(action_name: &str, cx: &App) -> Option<String> {
    let action = cx.build_action(action_name, None).ok()?;
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    let binding = keymap.bindings_for_action(action.as_ref()).last()?;
    let strokes = binding.keystrokes();
    if strokes.is_empty() {
        return None;
    }
    Some(
        strokes
            .iter()
            .map(|k| Kbd::format(k.as_keystroke()))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Display label for a palette group header — see
/// [`crate::command_palette::PaletteKind`]'s own doc comment for the group
/// order this mirrors.
fn palette_group_label(kind: crate::command_palette::PaletteKind) -> &'static str {
    use crate::command_palette::PaletteKind;
    match kind {
        PaletteKind::Command => "Commands",
        PaletteKind::Review => "Reviews",
        PaletteKind::File => "Files",
        PaletteKind::Pr => "Pull Requests",
    }
}

/// The settings panel overlay, while open. Mouse-first (no arrow-key
/// cursor like the theme/PR pickers — every control is a button/stepper/
/// text-input the user clicks directly), so all this holds is the one text
/// input's live state.
struct SettingsPanel {
    /// "Mono font" free-text field, pre-filled with the current setting.
    mono_font_input: Entity<InputState>,
    _subscription: Subscription,
}

/// Zero-sized `on_drag`/`on_drag_move` payload tag for the sidebar's resize
/// handle (see `AppShell::render_sidebar_resize_handle`). `on_drag_move`
/// only fires for a listener whose type parameter matches the *currently
/// active* drag's payload type (checked via `TypeId`, not per-element) — a
/// process-global `cx.active_drag` slot, not scoped to one handle. Giving
/// the sidebar and the review-summary panel (`workspace::SummaryResizeDrag`)
/// distinct tag types is load-bearing: with a single shared tag, dragging
/// either handle would also fire the other's `on_drag_move` listener,
/// resizing both panels from one drag.
#[derive(Clone)]
struct SidebarResizeDrag;

/// Sidebar badge for one repo's latest review.
#[derive(Debug, Clone, Copy)]
struct ReviewBadge {
    // `open`/`submitted` are only read by `automation_state`'s "badges"
    // dump (the sidebar cards render off `self.index`/`IndexEntry` instead,
    // not off `self.badges` — see `visible_sidebar_items`) — `allow`ed
    // rather than `cfg`'d out under `--no-default-features` since both
    // fields are still unconditionally written by `compute_local_badge`/
    // `merge_local_badge`.
    #[cfg_attr(not(feature = "automation"), allow(dead_code))]
    open: usize,
    #[cfg_attr(not(feature = "automation"), allow(dead_code))]
    submitted: bool,
    /// PR status (docs/phase-3-github.md deliverable 3/5), when the latest
    /// review is linked to one and the network fetch succeeded — `None`
    /// either way renders no PR cluster at all (see [`fetch_pr_badge`]).
    pr: Option<PrBadge>,
    /// The PR number `pr` (once fetched) actually belongs to — i.e. the
    /// *latest* review's `remote.pr` at the time the local fields were last
    /// recomputed. `this.badges` is keyed by location, not review, so with
    /// two PR-linked reviews sharing a location `pr` can go stale relative
    /// to whichever review the caller actually cares about (review
    /// finding P2: a different review's cached PR data was being stamped
    /// onto the wrong `IndexEntry`). Callers that adopt `pr` for a
    /// *specific* review must first check `pr_number == Some(that review's
    /// remote.pr)`.
    pr_number: Option<u64>,
}

/// Sidebar PR-status cluster for one repo's latest review: state glyph,
/// review-decision marker, and CI dot — flows into the review index's
/// [`dv_core::CachedPrStatus`] ([`AppShell::sync_index_pr_status`]) and, from
/// there, into [`render_pr_glyphs`]'s rendering on each review card.
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
        // "Open anything" quick-open (R2). Built
        // before the struct because `InputState::new` needs the `Window`.
        let quick_open = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Open PR URL, owner/repo#123, or path")
        });
        let quick_open_subscription = cx.subscribe_in(
            &quick_open,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.quick_open_submit(window, cx),
                // Any edit invalidates a stale inline error.
                InputEvent::Change => {
                    let had_error = this.quick_open_error.take().is_some();
                    if had_error {
                        cx.notify();
                    }
                }
                _ => {}
            },
        );
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            recent: RecentStore::load(),
            active: None,
            selected_review_id: None,
            menu_review: None,
            delete_confirm: None,
            automation,
            badges: HashMap::new(),
            index: dv_core::ReviewIndex::load(),
            index_hydration_gens: HashMap::new(),
            sidebar_dragging: false,
            filter_popover_open: false,
            quick_open,
            _quick_open_subscription: quick_open_subscription,
            quick_open_error: None,
            _ws_subscription: None,
            _ws_summary_subscription: None,
            settings,
            theme_picker: None,
            command_palette: None,
            settings_panel: None,
            _appearance_subscription: None,
            last_switch_ms: None,
            last_switch_cache_hit: None,
            workspace_cache: WorkspaceCache::new(MAX_CACHED_WORKSPACES, MAX_CACHE_BYTES),
            onboarding: None,
            onboarding_check_gen: 0,
            drift_page_dismissed: false,
            onboarding_installing: HashSet::new(),
            consistency_checked_at: HashMap::new(),
        };
        // Live follow-OS updates (docs/phase-4-settings-and-theming.md
        // deliverable 2): `Window::observe_window_appearance`'s registration
        // only records the callback and activates the subscription (see its
        // definition in `window.rs`) — it never invokes the callback itself,
        // synchronously or otherwise. The callback only ever runs later, off
        // a genuine platform appearance-change event (on Windows, a
        // `WM_SETTINGCHANGE`/`ImmersiveColorSet` message, itself deduped
        // against the platform window's last-seen appearance before it's
        // dispatched — see `gpui_windows`'s `handle_system_theme_changed`).
        // `resolve_follow_os` is idempotent (a no-op when follow-OS is off,
        // and `apply_resolved_theme` no-ops when the resolved name is
        // already the live theme), so it's safe to call unconditionally on
        // every event with no first-call skip.
        let weak = cx.weak_entity();
        this._appearance_subscription =
            Some(window.observe_window_appearance(move |window, cx| {
                weak.update(cx, |this, cx| this.resolve_follow_os(window, cx))
                    .ok();
            }));
        // App-open refresh (docs/phase-3-github.md deliverable 3): skip the
        // network pr_status pass for WSL-located entries here specifically
        // — a cold distro hasn't booted yet, and walking every WSL entry
        // sequentially at startup would stack a wsl.exe boot behind each
        // one. A manual refresh (`RefreshBadges`) or simply opening that
        // review (`refresh_badge`, not WSL-skipped) still fetches it.
        this.refresh_all_badges(true, cx);
        // Index hydration (docs/phase-6-review-navigator.md deliverable 1,
        // S6b): same WSL-liveness gate as the badge walk above
        // (cross-cutting risk B) — a naive "hydrate every known location at
        // launch" would boot every stopped distro the index has ever seen a
        // review in. The gate is unconditional inside `hydrate_index`
        // itself (not just at startup), so this call and the manual
        // `RefreshBadges` one below share the same boot-avoidance.
        this.hydrate_index(cx);
        match seed {
            Some((location, source)) => {
                this.open_review(location, source, pending_pr, None, window, cx)
            }
            // Nothing to focus into, so hold focus on the shell — otherwise
            // the advertised Ctrl+N binding (in the shell's key context) has
            // no focused node on its dispatch path and never fires.
            None => window.focus(&this.focus_handle, cx),
        }
        // Onboarding spine (S8e). True first run always shows the page —
        // EXCEPT under `--automation`: cross-cutting risk, a script's
        // `wait_ready` would wedge behind a page nothing in the script ever
        // dismisses (the orchestrator notes call this out explicitly).
        // Every other launch runs the identical check silently in the
        // background; it only resurfaces the page itself if something
        // actually needs a human (`Self::apply_consistency_report`) — dv's
        // own bits (`dv-host`/`dv-cli`) repair themselves silently as a side
        // effect of this same call, no page required.
        //
        // Deliberately placed AFTER `match seed` (S8e review, P2): a seeded
        // WSL open's `open_review` call above ends by focusing the freshly
        // built workspace (`Self::open_review`'s `window.focus(&handle,
        // cx)`); `open_onboarding_page`'s own `window.focus(&self.
        // focus_handle, cx)` must land LAST so the shell — not the dimmed
        // workspace underneath — actually holds focus when the "Welcome to
        // dv" modal is the first thing on screen. Otherwise the workspace's
        // own deeper `escape` binding (`ClearSelection`) outranks the
        // shell's `OnboardingClose` once focus is on that deeper node, and
        // only the mouse-only Close button/backdrop can dismiss the modal.
        if SetupState::load().is_first_run() && !automation {
            this.open_onboarding_page(window, cx);
        } else {
            let distros_allowed = this.live_wsl_distros();
            this.spawn_consistency_check_throttled(distros_allowed, cx);
        }
        this
    }

    /// Open a review: reactivate a cached workspace if one exists for
    /// `pinned_review_id` (Phase 7 D1 — [`Self::install_active`]'s doc
    /// comment covers the shared subscription-wiring; this cache-hit branch
    /// only does the reactivation-specific bookkeeping), or else spin up a
    /// fresh `Workspace` for `location`/`source` from zero and focus it so
    /// keyboard nav is live. `pending_pr` is only ever `Some` on the very
    /// first review a freshly launched `dv pr <number|url>` opens.
    /// `pinned_review_id`, when set, is an explicit user pick (a sidebar row
    /// click via [`Self::open_review_row`], or `{"cmd":"select_review"}`)
    /// that `Workspace::pick_review` must honor over its own auto-selection
    /// — including a SUBMITTED review, opened read-only
    /// (docs/phase-6-review-navigator.md's headline incident fix). The two
    /// are mutually exclusive by construction: every caller passes at most
    /// one — and only a `pinned_review_id` open can ever be a cache hit
    /// (doc deviation 5: a brand-new repo / `pending_pr` open has no key to
    /// look up yet, so it keeps today's loading state, though it IS stashed
    /// on exit by its resolved review id so a later switch-back hits).
    fn open_review(
        &mut self,
        location: RepoLocation,
        source: DiffSource,
        pending_pr: Option<u64>,
        pinned_review_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Phase 7 D4: wall time of this whole synchronous body, down to the
        // `self.active` assignment below (mirrors `Workspace::last_diff_ms`).
        // Measured on the UI thread with no async hop, so there's no
        // epoch/supersede concern the way the async-completion timings need.
        let t0 = std::time::Instant::now();
        // Persist an absolute path: a relative one (`dv .`) would resolve
        // against whatever cwd the app is next launched from.
        let location = absolutize(location);
        // Captured BEFORE the assignment below overwrites it — the
        // self-reselect guard a few lines down needs to know whether
        // `pinned_review_id` was *already* the active review's id, not
        // whether it's about to be (those are trivially equal after the
        // overwrite either way). Requires an actual pin (`pinned_review_id.
        // is_some()`): `selected_review_id` is `None` for any workspace
        // with no review loaded yet (a brand-new/unreviewed repo), and
        // every non-pinned open (New Review picker, `automation_open`, a
        // bare launch) also passes `pinned_review_id = None` — without this
        // guard, `None == None` would misfire as "already active" and
        // silently drop the open of a completely different unreviewed repo
        // (P1 review finding).
        let already_active = self.active.is_some()
            && pinned_review_id.is_some()
            && self.selected_review_id == pinned_review_id;
        // A pin is known to be the selected review right away; anything
        // else (a brand-new repo, `pending_pr`, a plain re-open) doesn't
        // know which review will actually land until the workspace's own
        // `ReviewChanged` reports it (see `Self::install_active`, below).
        // The cache-hit branch below overwrites this with the same value
        // once the reactivation actually lands.
        self.selected_review_id = pinned_review_id.clone();
        // Review-open refresh (docs/phase-3-github.md deliverable 3): not
        // WSL-skipped — opening this specific review already implies its
        // distro (if any) is live, so there's no cold-boot backlog hazard
        // the way there is walking every recent entry at startup. Full
        // refresh (local + network) — see `refresh_badge`'s doc comment.
        // Runs on a cache hit too: a parked workspace's own `ReviewChanged`
        // handler (`Self::on_cached_review_changed`) never fetches remotely
        // (cross-cutting risk D — no background sweep of the whole cache),
        // so its badge can be as stale as the last time it was active.
        self.refresh_badge(location.clone(), true, cx);
        // Fold this location's fresh review set into the index right away
        // too — don't wait for the broader `hydrate_index` walk, which may
        // already have run and skipped this location (a brand-new repo
        // isn't in the index yet at the time it runs) or simply not have
        // gotten to it yet. Not WSL-gated, same "opening it implies it's
        // live" reasoning as `refresh_badge`'s `fetch_remote: true` above.
        self.hydrate_index_location(location.clone(), cx);

        // Re-selecting the review that's already active (e.g. clicking its
        // own already-highlighted sidebar row again) is a content no-op —
        // the active entity's own live watchers already keep it current, so
        // there's nothing to reload. The active workspace is never itself
        // present in `workspace_cache` (only PARKED ones are — see that
        // struct's doc comment), so without this guard the lookup below
        // would always miss and fall into the cache-miss path: that would
        // stash the still-live active entity into the LRU under its own key
        // (spuriously counting against — and potentially evicting an
        // unrelated entry from — the eviction budget) AND build a brand-new
        // duplicate `Workspace` for the same review, leaving two
        // independently-watching live entities for one review until the
        // next distinct switch-away silently drops the stale one (review
        // finding: re-clicking the active row could later resurrect that
        // stale duplicate instead of the one actually being used).
        if already_active {
            if let Some(ws) = &self.active {
                let handle = ws.focus_handle(cx);
                window.focus(&handle, cx);
            }
            self.last_switch_cache_hit = Some(true);
            self.last_switch_ms = Some(t0.elapsed().as_millis() as u64);
            cx.notify();
            return;
        }

        // Onboarding spine (S8e): the "user is opening this WSL repo right
        // now" boot-storm exception the provision module's own doc calls
        // out — this distro is live by implication, so it's always safe to
        // check regardless of `has_running_host` (unlike the launch-time
        // walk in `Self::new`/`Self::live_wsl_distros`, which must gate on
        // it). Silently re-provisions `dv-host`/`dv-cli` on drift and
        // refreshes/auto-surfaces the onboarding page for a `node`/`vtsls`
        // consent row exactly like the launch-time check does. Placed AFTER
        // the `already_active` early-return above (S8e review, P2): re-
        // selecting the review that's already open is a pure no-op and
        // shouldn't spawn a fresh WSL round trip every time. Throttled
        // (phase-8 capstone review, P3): without `Self::
        // spawn_consistency_check_throttled`'s cooldown, rapidly triaging
        // many WSL reviews via the Phase-7 ~2ms cached switch fired one
        // full check — a `gh auth status` network round trip plus several
        // `wsl.exe` spawns — on every single switch.
        if let RepoLocation::Wsl { distro, .. } = &location {
            self.spawn_consistency_check_throttled(vec![distro.clone()], cx);
        }

        // Phase 7 D1 cache-hit fast path: reactivate a parked workspace
        // instead of rebuilding from zero. Keyed by review id (cross-cutting
        // risk E) — `pinned_review_id` IS that id for every caller that can
        // possibly hit (see this method's doc comment).
        if let Some(key) = pinned_review_id.as_ref()
            && let Some(cached) = self.workspace_cache.take(key)
        {
            self.stash_active(cx);
            let ws = cached.entity;
            let handle = ws.focus_handle(cx);
            // S7-0's fix depends on this: a reactivated entity keeps its
            // ORIGINAL `FocusHandle` (minted once in its `Workspace::new`),
            // so the PR picker's `Workspace && PrPickerOpen` Enter binding
            // (and every other workspace-scoped binding) only dispatches
            // once this call actually lands focus back on it.
            window.focus(&handle, cx);
            // No `pending_pr` on a reactivation — mutually exclusive with
            // `pinned_review_id` by construction (this method's doc
            // comment), and there is nothing left to "await" for an entity
            // that already has a review loaded.
            self.install_active(ws, None, cx);
            self.selected_review_id = Some(key.clone());
            // Phase 7 D1b (S7-4): kick a one-shot background revalidation
            // of the entry just reactivated — corrects any drift no parked
            // watcher covered while it sat in the cache (notably a Local
            // `WorkingTree` edit; the review-store watcher only observes
            // `.git/dv`, never the working tree). Dispatched AFTER
            // `install_active` so `self.active` is already the reactivated
            // entity, and it's the only entry ever touched here — never a
            // sweep of the whole `workspace_cache` (cross-cutting risk D).
            self.revalidate_active(cx);
            // Reactivating an already-loaded entity emits no `ReviewChanged`
            // (nothing about it changed) — `install_active`'s closure is the
            // ONLY place that stamps `last_opened_ms` (via
            // `self.index.upsert(entry, true)`), and it only runs off a
            // future event, which a plain reactivation never produces. Stamp
            // recency explicitly here, mirroring that closure, so a warm
            // switch-back counts as a real user pick for the sidebar's
            // recency order the same way a cold open does (review finding:
            // without this, re-selecting a parked review left it stuck at
            // its old sidebar position forever, both in-session and across
            // restarts via the persisted index).
            if let Some(entry) = self.index.get(key).cloned() {
                self.index.upsert(entry, true);
                // Re-dispatch rather than relying on the hydration already
                // dispatched above (line 984): that one was dispatched
                // *before* this stamp landed and would apply a pre-bump
                // snapshot over it (same race `install_active`'s closure
                // documents for its own upsert-then-hydrate pair) —
                // `hydrate_index_location` bumps its own generation, so this
                // call supersedes that stale one.
                self.hydrate_index_location(location.clone(), cx);
            }
            self.last_switch_cache_hit = Some(true);
            self.last_switch_ms = Some(t0.elapsed().as_millis() as u64);
            cx.notify();
            return;
        }

        // Cache miss (or no key to look up at all) — stash whatever was
        // active before building the replacement, same as the cache-hit
        // branch above.
        self.stash_active(cx);

        let view_mode_default = self.settings.view_mode_default;
        let context_lines = self.settings.context_lines;
        let font_size = self.settings.mono_font_size;
        let summary_width = self.settings.summary_width;
        // Captured before `cx.new`'s closure, whose own `cx` parameter
        // shadows this one — `self`/this outer `cx` are the only handles on
        // `AppShell` itself available to hand to the new `Workspace`.
        let shell = cx.weak_entity();
        let workspace = cx.new(|cx| {
            Workspace::new(
                location,
                source,
                pending_pr,
                pinned_review_id,
                view_mode_default,
                context_lines,
                font_size,
                summary_width,
                shell,
                window,
                cx,
            )
        });
        let handle = workspace.focus_handle(cx);
        window.focus(&handle, cx);
        self.install_active(workspace, pending_pr, cx);
        self.last_switch_cache_hit = Some(false);
        self.last_switch_ms = Some(t0.elapsed().as_millis() as u64);
        cx.notify();
    }

    /// Stash whatever workspace is currently active into the [`WorkspaceCache`]
    /// LRU (Phase 7 D1) instead of letting it drop, then evict down to
    /// budget. Keyed by the OUTGOING workspace's CURRENT review id,
    /// recomputed here rather than reused from whatever it was
    /// pinned/loaded with (cross-cutting risk E) — a workspace can switch to
    /// a *different* review mid-life (the in-app PR picker, a watcher-driven
    /// reload), and stashing under a stale id would make a later
    /// switch-back miss the cache. A workspace with no review loaded yet
    /// (still mid-load, or its store came up empty) has nothing worth
    /// caching under and is simply dropped, same as before this cache
    /// existed. Every [`Self::open_review`] call site — cache hit, cache
    /// miss, and a bare app-launch/`New Review` open with `self.active`
    /// already `None` (a no-op here) — routes through this so eviction and
    /// the parked `ReviewChanged` wiring are the single choke point.
    fn stash_active(&mut self, cx: &mut Context<Self>) {
        let Some(ws) = self.active.take() else {
            return;
        };
        // These two are single-slot and track only the ACTIVE workspace
        // (cross-cutting risk B) — clear them before the entity moves into
        // the cache so a parked workspace's `ReviewChanged` never runs the
        // active-only closure (which stamps `selected_review_id`/recency —
        // parked != selected) after this point. `install_active` (for
        // whatever becomes active next, if anything) installs its own fresh
        // pair.
        self._ws_subscription = None;
        self._ws_summary_subscription = None;
        // Parking is not closing — but a live vtsls child must not outlive
        // the switch-away just because the LRU keeps this entity around
        // for instant reactivation (P3 finding: `Workspace::park_lsp_session`'s
        // doc comment).
        ws.update(cx, |ws, cx| ws.park_lsp_session(cx));
        let key = ws.read(cx).review().map(|r| r.id.clone());
        if let Some(key) = key {
            let sub = cx.subscribe(&ws, Self::on_cached_review_changed);
            let bytes_sub = cx.subscribe(&ws, Self::on_cached_diff_bytes_changed);
            self.workspace_cache.insert(
                key,
                CachedWorkspace {
                    entity: ws,
                    _sub: sub,
                    _bytes_sub: bytes_sub,
                },
            );
            self.workspace_cache.evict_to_budget(cx);
        }
        // else: mid-load or empty-store — nothing worth caching; `ws` drops
        // here, tearing down its watchers same as it always has.
    }

    /// Install `ws` as the active workspace: wires the active-only
    /// `ReviewChanged`/`SummaryWidthChanged` subscriptions and assigns
    /// `self.active`. The `ReviewChanged` closure is moved here VERBATIM
    /// from before this slice's refactor (Phase 7 S7-3) — every documented
    /// invariant (`transient_pr_wait`, `stamped_review_id`, `pending_pr`'s
    /// two-event handling) is preserved unchanged; only its home moved.
    /// Callers are responsible for focusing `ws`'s handle and for
    /// `Self::stash_active`-ing whatever was active before this call — a
    /// cache-hit reactivation and a brand-new `Workspace::new` both need
    /// those, but in a different order relative to `ws`'s own construction,
    /// so neither belongs inside this shared tail.
    fn install_active(
        &mut self,
        workspace: Entity<Workspace>,
        pending_pr: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        // Tracks the review id the *last* recency stamp was for — `None`
        // until the first stamp. Every `ReviewChanged` on the same
        // workspace whose review id is unchanged from the last stamp (a
        // comment, reply, resolve, submit, or an external CLI/other-window
        // edit picked up by the store watcher) is a metadata refresh, not
        // the user re-selecting the review, and must not reorder the
        // sidebar under them. But a workspace can also switch to a
        // *different* review without a fresh `open_review` call — the
        // in-app PR picker (`Workspace::on_pr_picker_choose`) calls
        // `open_pr` directly on this same entity — and that IS an explicit
        // user pick, so it must still bump `last_opened_ms` (review
        // finding: a plain one-shot bool stamped once and never reset left
        // every later picker-driven switch on an already-open workspace
        // un-stamped, so the sidebar's recency order went stale after the
        // first event).
        let mut stamped_review_id: Option<String> = None;
        // True until the one transient pre-PR `ReviewChanged` of a `dv pr <n>`
        // launch has been skipped. Cleared after that skip so a `pending_pr`
        // that never resolves (bad/failed PR number) doesn't suppress recency
        // stamping for the rest of the workspace's life (capstone P3).
        let mut transient_pr_wait = true;
        // Keep this entry's badge live while the review is being worked on.
        self._ws_subscription = Some(cx.subscribe(
            &workspace,
            move |this: &mut Self, ws, _: &ReviewChanged, cx| {
                let location = ws.read(cx).location().clone();
                // Local-only, no network — see `refresh_badge`'s doc
                // comment (review finding P3-3).
                this.refresh_badge(location.clone(), false, cx);
                // Keep this review's cached index entry current on every
                // change too (comment/reply/resolve, submit, or an
                // external CLI/other-window edit picked up by the store
                // watcher).
                let entry = {
                    let ws = ws.read(cx);
                    // Docs/backlog.md "Merge-conflict indicator...": the
                    // live workspace's probe is authoritative for the diff
                    // it's SHOWING — but the adopted review isn't always
                    // that diff (a plain launch adopts the repo's latest
                    // draft whatever its source — `pick_review`), and a
                    // working-tree probe stamped onto a range review's
                    // card would erase `hydrate_location`'s persisted-
                    // live-base finding (docs/backlog.md "Stored-but-
                    // never-reopened PR range reviews...") with a clean
                    // answer about a different diff. Stamp only when the
                    // workspace shows this review's kind of diff (see
                    // `should_stamp_conflict` — kind match, not exact
                    // equality, so a reopened PR's fresh range still
                    // stamps); `None` lets `upsert`'s carry-forward
                    // keep whatever hydration last found (see
                    // `Workspace::source`'s doc comment).
                    let source_conflict = ws.conflict().cloned();
                    let ws_source = ws.source().clone();
                    ws.review().map(|review| {
                        let conflict = should_stamp_conflict(&review.source, &ws_source)
                            .then_some(source_conflict)
                            .flatten();
                        // Only a PR-linked review carries a `pr_status` at
                        // all (review finding P2-3) — `this.badges` is
                        // keyed by *location*, not review, and tracks
                        // whichever review `compute_local_badge` picked as
                        // "latest"; stamping its `pr` onto a *different*,
                        // local-only review sharing that location would
                        // paint someone else's PR state onto it. Worse, when
                        // the location has *two* PR-linked reviews (this one
                        // open, a different one currently "latest"),
                        // `this.badges[location].pr` can hold data fetched
                        // for the *other* review's PR (review finding P2) —
                        // so only adopt it when the badge's `pr_number`
                        // actually matches this review's own linked PR. When
                        // the review is PR-linked but the badge's network
                        // pass hasn't landed yet, or belongs to a different
                        // PR, fall back to whatever `pr_status` is already
                        // cached in the index rather than wiping it with
                        // `None` or stamping the wrong PR's data onto it
                        // (review finding P2-2).
                        let pr_status = review.remote.as_ref().and_then(|remote| {
                            this.badges
                                .get(&location)
                                .filter(|b| b.pr_number == Some(remote.pr))
                                .and_then(|b| b.pr)
                                .map(|pr| dv_core::CachedPrStatus {
                                    state: pr.state,
                                    is_draft: pr.is_draft,
                                    decision: pr.decision,
                                    checks: pr.checks,
                                })
                                .or_else(|| {
                                    this.index.get(&review.id).and_then(|e| e.pr_status.clone())
                                })
                        });
                        let mut entry = dv_core::IndexEntry::from_review(&location, review);
                        entry.pr_status = pr_status;
                        entry.conflict = conflict.clone();
                        (review.id.clone(), entry)
                    })
                };
                if let Some((review_id, entry)) = entry {
                    // This workspace is the active one — whatever review it
                    // shows right now is, by definition, the sidebar's
                    // selected review, however it got there (an explicit
                    // row pin, `pick_review`'s auto-selection, or a watcher
                    // swap all funnel through here).
                    this.selected_review_id = Some(review_id.clone());
                    // See `WorkspaceCache::drop_stale`'s doc comment (P3
                    // review finding): this workspace is now the one true
                    // owner of `review_id` — if an earlier switch-away left
                    // a stale duplicate parked under the same id (reachable
                    // only via a non-pinned open, which skips the cache-hit
                    // fast path), drop it now rather than letting it linger
                    // as a second live watcher until the next unrelated
                    // switch-away happens to overwrite it.
                    this.workspace_cache.drop_stale(&review_id);
                    // A `pending_pr` launch (`dv pr <n>`) produces TWO
                    // `ReviewChanged` events for one `open_review` call: the
                    // initial load's own `pick_review` fallback (no PR
                    // context yet — typically an unrelated newest draft),
                    // immediately followed by `open_pr`'s own load of the
                    // actual PR-linked review. The first event is transient,
                    // not the user's selection, so it must not consume the
                    // first recency stamp (review finding: doing so bumped
                    // the unrelated draft's `last_opened_ms` and left the
                    // just-launched PR review's recency untouched, sorting
                    // the wrong review to the top of the sidebar at the
                    // next launch — `ReviewIndex::load` is the only place
                    // recency ordering applies since `apply_hydration`
                    // went order-preserving). Wait for the event whose review is
                    // actually linked to `pending_pr` before stamping; a
                    // plain open (no `pending_pr`) or an already-pinned open
                    // has no such transient step, so its first event is
                    // definitive as before. Once a review HAS been stamped,
                    // any later event for a *different* review id (the
                    // PR-picker case above) is by construction an explicit
                    // switch too, so it re-stamps unconditionally; only a
                    // later event for the *same* review id (an edit) skips
                    // stamping.
                    let opened = match &stamped_review_id {
                        None => {
                            if pending_pr.is_none() {
                                true
                            } else if entry
                                .remote
                                .as_ref()
                                .is_some_and(|r| Some(r.pr) == pending_pr)
                            {
                                // The awaited PR-linked review landed.
                                true
                            } else {
                                // Skip only the one transient pre-PR fallback
                                // event (`mem::take` clears the flag on that
                                // first skip). If `open_pr` never delivers a
                                // matching event (failed/typo'd PR), later
                                // events stamp rather than freezing recency for
                                // the whole session (capstone P3).
                                !std::mem::take(&mut transient_pr_wait)
                            }
                        }
                        Some(prev) => *prev != review_id,
                    };
                    if opened {
                        stamped_review_id = Some(review_id.clone());
                    }
                    // Re-dispatch a full location hydration on every live
                    // metadata write too, not just when `open_review`
                    // itself dispatches one — an in-flight hydration
                    // dispatched *before* this upsert (e.g. `open_review`'s
                    // own location hydrate, reading a pre-edit snapshot)
                    // must be treated as stale once fresher data has landed
                    // here, or its completion clobbers this upsert's
                    // `open_comments` with the older snapshot. Merely
                    // bumping the generation to invalidate that stale
                    // hydration (without redispatching) fixed the clobber
                    // but introduced a worse regression: the invalidated
                    // hydration was often the *only* thing that would ever
                    // apply this location's full review set, so a
                    // brand-new repo with sibling reviews lost every
                    // sibling but the active one until the next manual
                    // refresh (review finding — the S6c acceptance
                    // assertion that `state.sidebar` lists every review in
                    // a multi-review repo). `hydrate_index_location` bumps
                    // the generation itself (superseding the stale
                    // dispatch) and reads a guaranteed post-upsert
                    // snapshot, so its completion both drops the clobber
                    // risk and restores every sibling; `apply_hydration`
                    // carries `last_opened_ms`/`pr_status` forward by
                    // review id, so this upsert's own stamp survives the
                    // re-hydration.
                    this.index.upsert(entry, opened);
                    this.hydrate_index_location(location.clone(), cx);
                } else {
                    // The active workspace's review store is now fully
                    // empty (e.g. the only review, currently open, was
                    // deleted externally via CLI or another window) —
                    // `pick_review` has nothing left to fall back to and
                    // `ws.review()` is `None`. `self._ws_subscription`
                    // holds only the *current* workspace's subscription
                    // (assigning a new one drops and unsubscribes the
                    // old), so this closure only ever runs for the active
                    // workspace and it's always correct to clear
                    // `selected_review_id` here (review finding: leaving it
                    // pointed at the now-deleted id kept the stale row
                    // highlighted in the sidebar for the rest of the
                    // session with no path back to `None`).
                    this.selected_review_id = None;
                }
            },
        ));
        // Summary-panel drag-release persistence (Phase 4 deliverable 5) —
        // see `_ws_summary_subscription`'s doc comment.
        self._ws_summary_subscription = Some(cx.subscribe(
            &workspace,
            move |this: &mut Self, _, event: &SummaryWidthChanged, _cx| {
                this.settings.summary_width = event.0;
                this.settings.save();
            },
        ));
        self.active = Some(workspace);
    }

    /// Kicks a one-shot Phase 7 D1b revalidation ([`Workspace::revalidate`])
    /// of `self.active` — dispatched only from [`Self::open_review`]'s
    /// cache-hit branch, right after [`Self::install_active`] has landed
    /// the reactivated entity as `self.active`. This is the S7-4 completion
    /// of the no-op `install_active`/cache-hit stub S7-3 deliberately left
    /// in place rather than land prematurely. Never call this for a brand-
    /// new (cache-miss) `Workspace::new` — its own initial load already does
    /// the equivalent work from a clean slate, so revalidating it again
    /// would be redundant. Also never sweep `self.workspace_cache` here —
    /// this only ever touches the single entity that was just reactivated
    /// (cross-cutting risk D: a background pass over every parked entry
    /// could boot a stopped WSL distro just for sitting in the cache).
    fn revalidate_active(&mut self, cx: &mut Context<Self>) {
        if let Some(ws) = &self.active {
            ws.update(cx, |ws, cx| ws.revalidate(cx));
        }
    }

    /// Lighter `ReviewChanged` handler for a PARKED [`WorkspaceCache`] entry
    /// (cross-cutting risk B) — keeps its badge and review-index entry
    /// current while it isn't shown (the Phase-2 store watcher / an agent
    /// CLI edit still streams in via `Workspace::_watcher`, which stays
    /// subscribed for as long as the entity is cached; see
    /// `Self::stash_active`), but deliberately does NOT stamp recency the
    /// way [`Self::install_active`]'s active-only closure does:
    /// `selected_review_id` and the sidebar's recency order are the ACTIVE
    /// workspace's concern only — a parked workspace, by definition, isn't
    /// what the sidebar currently has selected.
    fn on_cached_review_changed(
        &mut self,
        ws: Entity<Workspace>,
        _: &ReviewChanged,
        cx: &mut Context<Self>,
    ) {
        let location = ws.read(cx).location().clone();
        // Cross-cutting risk E again: a PARKED entity's own review can
        // drift out from under the key it was stashed under — an unpinned
        // entity's `pick_review` fallback re-floats to a different draft
        // when a sibling draft appears (or its pinned review is deleted),
        // and this handler is exactly the signal that a drift happened.
        // Re-key the cache entry so a later reactivation-by-id
        // (`Self::open_review`'s `workspace_cache.take(key)`) finds this
        // entity under the review it's ACTUALLY showing now, instead of
        // silently reactivating it under a stale id while the sidebar
        // still highlights the id it was originally parked under (P2
        // review finding). LRU position is preserved — this is a same-
        // entity identity fixup, not a new access.
        if let Some(new_id) = ws.read(cx).review().map(|r| r.id.clone()) {
            let old_key = self
                .workspace_cache
                .entries
                .iter()
                .find(|(_, cached)| cached.entity.entity_id() == ws.entity_id())
                .map(|(k, _)| k.clone());
            if let Some(old_key) = old_key
                && old_key != new_id
            {
                self.workspace_cache.rekey(old_key, new_id);
            }
        }
        // Local-only, no network (cross-cutting risk D — no background
        // sweep over the whole cache; see `refresh_badge`'s doc comment for
        // why `fetch_remote: false` is the right choice for a live-update
        // subscription regardless of active/parked).
        self.refresh_badge(location.clone(), false, cx);
        let entry = {
            let ws = ws.read(cx);
            // Same reasoning as `Self::install_active`'s active closure —
            // a parked workspace's live conflict probe is just as much a
            // real answer as an active one's, and the same source-KIND
            // guard applies (don't stamp a probe about a different kind of
            // diff over hydration's persisted-live-base finding; see
            // `should_stamp_conflict`).
            let source_conflict = ws.conflict().cloned();
            let ws_source = ws.source().clone();
            ws.review().map(|review| {
                let conflict = should_stamp_conflict(&review.source, &ws_source)
                    .then_some(source_conflict)
                    .flatten();
                // Same pr_status carry-forward as `Self::install_active`'s
                // active closure — see its doc comment for why this can't
                // just adopt `this.badges[location].pr` unconditionally.
                let pr_status = review.remote.as_ref().and_then(|remote| {
                    self.badges
                        .get(&location)
                        .filter(|b| b.pr_number == Some(remote.pr))
                        .and_then(|b| b.pr)
                        .map(|pr| dv_core::CachedPrStatus {
                            state: pr.state,
                            is_draft: pr.is_draft,
                            decision: pr.decision,
                            checks: pr.checks,
                        })
                        .or_else(|| self.index.get(&review.id).and_then(|e| e.pr_status.clone()))
                });
                let mut entry = dv_core::IndexEntry::from_review(&location, review);
                entry.pr_status = pr_status;
                entry.conflict = conflict.clone();
                entry
            })
        };
        if let Some(entry) = entry {
            // `opened: false` — reactivation (which stamps `true`) happens
            // through `Self::open_review`'s cache-hit branch instead, once
            // this entry is actually selected again, not on every live edit
            // while it merely sits parked.
            self.index.upsert(entry, false);
            self.hydrate_index_location(location, cx);
        }
        // No `else` clearing `selected_review_id` here (unlike the active
        // closure) — a parked entry's review store going empty says nothing
        // about what the sidebar currently has selected.
    }

    /// A diff computation dispatched before parking just landed rows on a
    /// PARKED entity (`Workspace::DiffBytesChanged`, subscribed per cached
    /// entry — capstone P3-8): the LRU's real byte total may now exceed
    /// what `stash_active`'s `evict_to_budget` accounted at park time, so
    /// re-enforce the budget. Cheap and rare: at most one stage-1 + one
    /// stage-2 completion per in-flight request can straggle in per park,
    /// and `evict_to_budget` is a walk over at most six entries.
    fn on_cached_diff_bytes_changed(
        &mut self,
        _ws: Entity<Workspace>,
        _: &crate::workspace::DiffBytesChanged,
        cx: &mut Context<Self>,
    ) {
        self.workspace_cache.evict_to_budget(cx);
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
    fn refresh_badge(
        &mut self,
        location: RepoLocation,
        fetch_remote: bool,
        cx: &mut Context<Self>,
    ) {
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
            let pr_number = remote.pr;
            let slug = remote.slug.clone();
            let pr = cx
                .background_executor()
                .spawn(async move { fetch_pr_badge(&remote) })
                .await;
            if let Some(pr) = pr {
                this.update(cx, |this, cx| {
                    if let Some(badge) = this.badges.get_mut(&key)
                        && badge.pr_number == Some(pr_number)
                    {
                        badge.pr = Some(pr);
                    }
                    // Flow the fresh fetch into the index too (review
                    // finding P3-1) — a review that's merely browsed (no
                    // comment/reply/resolve) never fires `ReviewChanged`,
                    // so without this its `IndexEntry.pr_status` would sit
                    // at `None` for the whole session even though this
                    // exact fetch just landed the real status.
                    this.sync_index_pr_status(&slug, pr_number, pr);
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
    ///
    /// Locations come from [`Self::known_locations`] (index UNION recent),
    /// not `self.recent.entries()` alone (review finding: since S6c stopped
    /// writing `recent.json` on every open, that list is frozen at launch —
    /// scoping this walk to it silently stopped covering any review opened
    /// afterward, which is the *only* bulk path that fetches `pr_status` for
    /// the index-driven sidebar).
    fn refresh_all_badges(&mut self, startup: bool, cx: &mut Context<Self>) {
        let locations: Vec<_> = self.known_locations();
        cx.spawn(async move |this, cx| {
            let local = cx
                .background_executor()
                .spawn(async move {
                    locations
                        .into_iter()
                        // Boot-avoidance (plan §4): `compute_local_badge`
                        // opens a `ReviewStore`, which for a WSL location
                        // with no live host shells out `wsl.exe cat/ls` —
                        // booting a stopped distro as a side effect of
                        // merely painting a sidebar badge. `has_running_host`
                        // never spawns/installs/boots anything; it only
                        // reports a distro that already has a connection
                        // open from some earlier, explicit repo-open. This
                        // skip covers BOTH the startup walk and a manual
                        // `RefreshBadges` — sequentially booting every WSL
                        // entry's distro on an explicit-but-still-surprising
                        // manual refresh would be just as nasty as doing it
                        // silently at launch.
                        .filter(|loc| match loc {
                            RepoLocation::Wsl { distro, .. } => {
                                dv_core::remote::manager::has_running_host(distro)
                            }
                            RepoLocation::Local(_) => true,
                        })
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
                        // included. Still seed the recomputed badge's
                        // `pr` from whatever this entry already showed
                        // (review finding P2, proven live: the dot
                        // flickers off on every refresh) via the same
                        // `merge_local_badge` carry-over the
                        // `ReviewChanged`-triggered path already uses —
                        // otherwise every entry's badge blanks to
                        // `pr: None` the instant this pass lands, and
                        // only comes back if (and whenever) its own
                        // network fetch below succeeds. The fetch is
                        // still queued unconditionally here (unlike the
                        // `fetch_remote: false` caller of
                        // `merge_local_badge`), so a success still
                        // overwrites with fresh data; a failure just
                        // leaves the carried-over value in place (stale
                        // beats blank).
                        let existing = this.badges.get(&loc).copied();
                        this.badges
                            .insert(loc.clone(), merge_local_badge(existing, badge, false));
                        if let Some(remote) = remote {
                            to_fetch.push((loc, remote));
                        }
                    }
                }
                cx.notify();
            })
            .ok();

            for (loc, remote) in to_fetch {
                let pr_number = remote.pr;
                let slug = remote.slug.clone();
                let pr = cx
                    .background_executor()
                    .spawn(async move { fetch_pr_badge(&remote) })
                    .await;
                if let Some(pr) = pr {
                    this.update(cx, |this, cx| {
                        if let Some(badge) = this.badges.get_mut(&loc)
                            && badge.pr_number == Some(pr_number)
                        {
                            badge.pr = Some(pr);
                        }
                        // Same index sync as `refresh_badge` (review
                        // finding P3-1) — a manual `RefreshBadges` (or the
                        // startup walk) must land in the index too, not
                        // just `self.badges`.
                        this.sync_index_pr_status(&slug, pr_number, pr);
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    /// Propagate a freshly fetched PR status into every index entry linked
    /// to `remote_slug`/`pr_number` (review finding P3-1).
    /// `refresh_badge`/`refresh_all_badges`'s network completions previously
    /// only patched `self.badges` — a review that's merely browsed (no
    /// comment/reply/resolve to trigger the `ReviewChanged` subscription
    /// that otherwise updates the index) could sit with
    /// `IndexEntry.pr_status: None` for the whole session even though the
    /// badge cache already had the answer.
    ///
    /// Matches by `(remote.slug, remote.pr)`, **not** `RepoLocation`
    /// (review finding P2-2): `self.badges`/`self.recent` are keyed by the
    /// open-time absolutized path, while the index's entries carry
    /// whatever `Workspace::location()` normalized to (git's
    /// `rev-parse --show-toplevel`, via `hydrate_location`/`from_review`) —
    /// the two can differ textually (a subdirectory open, or a
    /// case-divergent path on Windows) even for the same repo, so a
    /// `RepoLocation` equality filter here would silently match nothing
    /// and leave `pr_status` stuck at `None` forever. `slug`+`pr` identify
    /// the linked PR itself, independent of how the caller's `RepoLocation`
    /// happens to be spelled. Still scoped to `pr_number` (matching the
    /// `ReviewChanged` closure's own reasoning, review finding P2): more
    /// than one review can be linked to the same repo, but this fetch's
    /// status belongs to exactly the one PR it was fetched for.
    fn sync_index_pr_status(&mut self, remote_slug: &str, pr_number: u64, pr: PrBadge) {
        let status = dv_core::CachedPrStatus {
            state: pr.state,
            is_draft: pr.is_draft,
            decision: pr.decision,
            checks: pr.checks,
        };
        let updates: Vec<dv_core::IndexEntry> = self
            .index
            .entries()
            .iter()
            .filter(|e| {
                // Case-fold the slug the same way `pr_group_key` does — two
                // reviews of one PR can carry case-divergent slugs (origin URL
                // casing drift), and an exact match would leave the sibling's
                // sidebar PR glyph stale (capstone P3).
                e.remote
                    .as_ref()
                    .is_some_and(|r| r.pr == pr_number && r.slug.eq_ignore_ascii_case(remote_slug))
            })
            .cloned()
            .map(|mut e| {
                e.pr_status = Some(status.clone());
                e
            })
            .collect();
        for entry in updates {
            // `opened: false` — this is a metadata refresh, not the user
            // re-selecting the review (same reasoning as the
            // `ReviewChanged` closure's own `upsert` call).
            self.index.upsert(entry, false);
        }
    }

    /// One location's worth of index refresh, off-thread: re-lists that
    /// repo's `.git/dv` store ([`dv_core::hydrate_location`]) and folds the
    /// result into `self.index` via
    /// [`dv_core::ReviewIndex::apply_hydration`]. **Not** idempotent under
    /// reordering: `apply_hydration` does a full-outcome replace for the
    /// location, so two overlapping calls (e.g. `open_review`'s own hydrate
    /// racing this pass's walk, or two `ReviewChanged` events in quick
    /// succession) that read *different* disk snapshots must still apply in
    /// dispatch order — and `cx.background_executor()` is a real
    /// multi-threaded pool, so completion order isn't guaranteed to match
    /// dispatch order. `index_hydration_gens` guards exactly this
    /// (cross-cutting risk C, same discard-if-superseded shape as
    /// `Workspace::source_epoch`): bump-and-capture before spawning, only
    /// apply on completion if this location's generation hasn't moved on.
    /// Never itself decides whether it's safe to hydrate a WSL location —
    /// see call sites.
    fn hydrate_index_location(&mut self, location: RepoLocation, cx: &mut Context<Self>) {
        let slot = self
            .index_hydration_gens
            .entry(location.clone())
            .or_insert(0);
        *slot += 1;
        let dispatched_gen = *slot;
        cx.spawn(async move |this, cx| {
            let loc = location.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move { dv_core::hydrate_location(&loc) })
                .await;
            this.update(cx, |this, cx| {
                // A newer hydration for this same location has been
                // dispatched since — this outcome is stale, discard it
                // rather than let it clobber whatever the newer pass
                // applies (or already applied).
                if this.index_hydration_gens.get(&location) != Some(&dispatched_gen) {
                    return;
                }
                this.index.apply_hydration(&location, outcome);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Every location the app currently knows a review for — the index's
    /// own entries UNION `recent.json`'s seed, deduped by location
    /// (cross-cutting risk G: a user with only a pre-Phase-6 `recent.json`
    /// must not lose their sidebar on upgrade to a review-centric one).
    /// Shared by [`Self::hydrate_index`] (which pass gets the local-store
    /// walk) and [`Self::refresh_all_badges`] (which pass gets the network
    /// PR-status fetch): since S6c stopped `RecentStore::touch`ing on every
    /// open (`recent.rs`'s "read-only as of S6c"), `self.recent.entries()`
    /// alone is a snapshot frozen at launch — a location opened afterward
    /// only ever exists in `self.index`, and a badges walk scoped to
    /// `recent` alone would never fetch PR/CI status for it (review
    /// finding, `refresh_all_badges` regression: the manual "Refresh
    /// badges" action and the startup badge walk both silently stopped
    /// covering every review the sidebar itself now shows).
    fn known_locations(&self) -> Vec<RepoLocation> {
        // Dedup the index's own entries by location first (review finding
        // P3-4) — a location with N cached reviews (N `IndexEntry`s, since
        // the index is keyed by `review_id` not location) must still only
        // appear once, matching this function's own "deduped by location"
        // doc promise above.
        let mut locations: Vec<RepoLocation> = Vec::new();
        for entry in self.index.entries() {
            if !locations.contains(&entry.location) {
                locations.push(entry.location.clone());
            }
        }
        for entry in self.recent.entries() {
            if !locations.contains(&entry.location) {
                locations.push(entry.location.clone());
            }
        }
        locations
    }

    /// Walk every location the app currently knows a review for (see
    /// [`Self::known_locations`]) and refresh each one off-thread via
    /// [`Self::hydrate_index_location`]. The WSL-liveness skip mirrors
    /// `refresh_all_badges`'s boot-avoidance filter exactly (cross-cutting
    /// risk B) and, like that filter, applies unconditionally regardless of
    /// caller: `hydrate_index_location` opens a `ReviewStore`, which for a
    /// WSL location with no live host boots a stopped distro (review
    /// finding P1/P2 — a startup-only guard here left the manual
    /// `RefreshBadges` path booting every stopped distro the index has ever
    /// seen a review in, exactly the storm this policy exists to prevent).
    fn hydrate_index(&mut self, cx: &mut Context<Self>) {
        for location in self.known_locations() {
            if let RepoLocation::Wsl { distro, .. } = &location
                && !dv_core::remote::manager::has_running_host(distro)
            {
                continue;
            }
            self.hydrate_index_location(location, cx);
        }
    }

    fn on_refresh_badges(&mut self, _: &RefreshBadges, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_all_badges(false, cx);
        // Hydrate the cross-repo index too (review finding P3-2) — without
        // this, `RefreshBadges` only ever refreshed `self.badges`, leaving
        // a review added via CLI to a repo that isn't currently open
        // invisible in the index until the app restarts or that repo is
        // explicitly reopened. Unlike `refresh_all_badges`, whose WSL skip
        // only gates the local store-open (the network `pr_status` fetch is
        // still deliberately unconditional on manual refresh),
        // `hydrate_index_location` itself opens a `ReviewStore` for the
        // local pass, so its WSL-liveness skip stays unconditional here too
        // — a stopped distro must not get booted just because the user
        // asked for a badge refresh (review finding P1/P2, cross-cutting
        // risk B).
        self.hydrate_index(cx);
        // Also refresh the active workspace's PR header band, if it has
        // one open (review finding P3-b) — `RefreshBadges` is already the
        // one manual "go sync with GitHub" gesture in the UI, so the
        // header shouldn't need a second, undiscoverable way to unstick
        // itself. `Workspace::refresh_pr` no-ops when no PR is open.
        //
        // Same reasoning extends to the read-only GitHub thread pull
        // (docs/phase-6-review-navigator.md deliverable 6, acceptance: "a
        // thread resolved on github.com shows resolved in dv after a
        // manual refresh, without restarting dv") — `refresh_remote_threads`
        // no-ops with no PR linked.
        if let Some(ws) = &self.active {
            ws.update(cx, |ws, cx| {
                ws.refresh_pr(cx);
                ws.refresh_remote_threads(cx);
            });
        }
    }

    // ---- Theme picker ---------------------------------------------------

    fn on_open_theme_picker(
        &mut self,
        _: &OpenThemePicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.theme_picker.is_some()
            || self.settings_panel.is_some()
            || self.onboarding.is_some()
            || self.command_palette.is_some()
        {
            return;
        }
        // Decline while the active workspace's own PR picker is open (review
        // finding P2: `on_open_pr_picker`'s guard against these two shell
        // overlays only ran in that one direction — opening ctrl-shift-t
        // over an already-open PR picker was still possible, painting the
        // theme picker directly on top of it and, on close, leaving the PR
        // picker's own bindings live again only after an extra, unexplained
        // `escape`). Mirrors `on_open_pr_picker`'s `overlay_open` check.
        if self
            .active
            .as_ref()
            .is_some_and(|ws| ws.read(cx).pr_picker_open())
        {
            return;
        }
        // The sidebar filter popover is a third shell-level overlay that
        // doesn't go through this guard (it's mouse-only, so it never moves
        // focus and this action can fire while it's open) — close it so its
        // full-window backdrop can't end up painted on top of the picker
        // (review finding: the two overlays stacking swallows the picker's
        // first click).
        self.close_filter_popover(cx);
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

    // ---- Review-card context menu (archive / delete) -------------------

    /// Context-menu "Archive"/"Unarchive": flips the index's app-managed
    /// flag for the review the menu was opened over ([`Self::menu_review`]).
    /// Nothing in the repo's store changes — with the Archived filter off
    /// (the default) the row just disappears from the sidebar.
    fn on_toggle_archive_review(
        &mut self,
        _: &ToggleArchiveReview,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.menu_review.take() else {
            return;
        };
        let Some(archived) = self.index.get(&id).map(|e| e.archived) else {
            return;
        };
        self.index.set_archived(&id, !archived);
        cx.notify();
    }

    /// Context-menu "Delete review…": open the confirmation modal. The
    /// actual store delete only runs on [`Self::on_delete_review_confirm`].
    fn on_delete_review_prompt(
        &mut self,
        _: &DeleteReviewPrompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.menu_review.take() else {
            return;
        };
        // Same mutual-exclusion posture as the other shell overlays (see
        // `on_open_theme_picker`) — mostly unreachable while one is up
        // (they occlude the sidebar), kept for safety. The PR-picker check
        // is NOT redundant though (post-hoc review P3): it's a workspace-
        // pane overlay, so the sidebar stays right-clickable while it's
        // open and this modal would stack on top of it.
        if self.theme_picker.is_some()
            || self.settings_panel.is_some()
            || self.onboarding.is_some()
            || self.command_palette.is_some()
        {
            return;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|ws| ws.read(cx).pr_picker_open())
        {
            return;
        }
        self.delete_confirm = Some(DeleteConfirm {
            review_id: id,
            in_flight: false,
            error: None,
        });
        // Focus the shell so the modal's escape/enter bindings (context
        // "AppShell && DeleteConfirmOpen") sit on the dispatch path — same
        // reasoning as `on_open_theme_picker`.
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Close the confirmation modal, restoring focus the same way
    /// [`Self::close_theme_picker`] does. Safe to call mid-flight: the
    /// delete keeps running and its completion still folds the removal
    /// into the index (the deletion has happened on disk either way).
    fn close_delete_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.delete_confirm.take().is_some() {
            match &self.active {
                Some(ws) => window.focus(&ws.focus_handle(cx), cx),
                None => window.focus(&self.focus_handle, cx),
            }
            cx.notify();
        }
    }

    fn on_delete_review_cancel(
        &mut self,
        _: &DeleteReviewCancel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_delete_confirm(window, cx);
    }

    /// Run the delete: `ReviewStore::delete` on the background executor
    /// (it shells out — WSL locations route through the host), then fold
    /// the removal into the index. The modal stays up while in flight and
    /// shows the error on failure (a stopped distro, a vanished path)
    /// rather than pretending the review is gone.
    fn on_delete_review_confirm(
        &mut self,
        _: &DeleteReviewConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(confirm) = &mut self.delete_confirm else {
            return;
        };
        if confirm.in_flight {
            return;
        }
        let id = confirm.review_id.clone();
        let Some(location) = self.index.get(&id).map(|e| e.location.clone()) else {
            // Vanished from the index meanwhile (deleted externally) —
            // nothing left to delete; close with the standard focus rescue.
            self.close_delete_confirm(window, cx);
            return;
        };
        confirm.in_flight = true;
        confirm.error = None;
        cx.notify();

        let store_location = location.clone();
        let store_id = id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { dv_core::ReviewStore::open(store_location).delete(&store_id) })
                .await;
            this.update_in(cx, |this, window, cx| {
                // Only the modal instance that launched this delete may be
                // touched (post-hoc review P2): the user can escape a
                // slow in-flight delete and open the modal for a DIFFERENT
                // review meanwhile — closing that one here (or writing this
                // delete's error/in_flight into it, re-arming its Delete
                // button mid-flight) would corrupt the newer flow.
                let own_modal = this
                    .delete_confirm
                    .as_ref()
                    .is_some_and(|confirm| confirm.review_id == id);
                match result {
                    Ok(()) => {
                        this.index.remove(&id);
                        // Drop any parked workspace for the deleted review
                        // (its watchers included). If the review is the
                        // ACTIVE workspace's, its own store watcher fires
                        // next and falls back to another review or the
                        // empty state (the existing external-CLI-delete
                        // path in `_ws_subscription`); clear the stale pin
                        // rather than waiting on that event.
                        this.workspace_cache.drop_stale(&id);
                        if this.selected_review_id.as_deref() == Some(id.as_str()) {
                            this.selected_review_id = None;
                        }
                        // Proper focus rescue on close (the R2 lesson:
                        // never leave focus parked on a node that's about
                        // to stop mattering).
                        if own_modal {
                            this.close_delete_confirm(window, cx);
                        }
                        this.hydrate_index_location(location, cx);
                    }
                    Err(err) => {
                        // If not `own_modal` (dismissed mid-flight, or a
                        // different review's modal is up now) there's
                        // nothing to report into — the review simply stays
                        // in the sidebar.
                        if own_modal && let Some(confirm) = &mut this.delete_confirm {
                            confirm.in_flight = false;
                            confirm.error = Some(format!("{err:#}"));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn on_theme_picker_next(
        &mut self,
        _: &ThemePickerNext,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(picker) = &mut self.theme_picker {
            let len = themes::names().count();
            if len > 0 {
                picker.selected = (picker.selected + 1).min(len - 1);
                cx.notify();
            }
        }
    }

    fn on_theme_picker_prev(
        &mut self,
        _: &ThemePickerPrev,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    /// Explicit theme pick (`name` is always one of `themes::names()`):
    /// persist it as the manual `theme` and turn `follow_os_appearance`
    /// off — explicit beats automatic (docs/phase-4-settings-and-theming.md
    /// deliverable 2). Applies live, then closes the picker (a no-op if it
    /// wasn't open — the settings panel's Theme row calls this too, with no
    /// picker involved). Shared by the keyboard path
    /// ([`Self::on_theme_picker_choose`]), the picker row's mouse click, and
    /// the settings panel.
    fn choose_theme(&mut self, name: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.theme = name.to_string();
        self.settings.follow_os_appearance = false;
        self.settings.save();
        self.apply_resolved_theme(name, window, cx);
        self.close_theme_picker(window, cx);
    }

    /// Apply `name` live iff it isn't already the active theme (comparing
    /// against the *live* global theme, not `self.settings.theme` — while
    /// follow-OS is on, those two can legitimately differ). Deliberately
    /// takes no position on `self.settings.theme`/`follow_os_appearance`:
    /// the caller ([`Self::choose_theme`] for an explicit pick,
    /// [`Self::resolve_follow_os`] for an automatic one) owns that.
    fn apply_resolved_theme(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if *cx.theme().theme_name() == *name {
            return;
        }
        themes::apply_theme(name, &self.settings.mono_font, Some(window), cx);
        // UI chrome picks up the new theme for free (reads `cx.theme()`
        // fresh every render), but the active workspace's diff pane cached
        // its syntax highlighting with the *old* theme's concrete colors
        // baked in — see `Workspace::on_theme_changed`. `eager: true`: the
        // user is looking at this one right now, so recompute unconditionally
        // regardless of WSL host reachability (review finding — gating this
        // on `has_running_host` left the visible diff pane silently
        // blank/stale whenever the distro had no *host* connection, even
        // though `request_diff`'s own per-command `wsl.exe` fallback would
        // have worked fine).
        if let Some(ws) = &self.active {
            ws.update(cx, |ws, cx| ws.on_theme_changed(true, cx));
        }
        // Every PARKED workspace's diff pane has the exact same stale-bake
        // problem — it just isn't on screen right now (review finding: a
        // theme change made while workspace B is active left A's cached
        // rows painted in the OLD theme when A was later reactivated,
        // since only `self.active` ever got `on_theme_changed`). Safe to
        // fan out unconditionally to every cached entry regardless of
        // location/host state: `eager: false` makes
        // `Workspace::invalidate_diff_cache` gate the actual git-touching
        // recompute on the repo being reachable (cross-cutting risk D — a
        // stopped WSL distro must never get booted just to repaint a
        // workspace nobody is looking at), so this loop only ever does
        // cheap in-memory cache invalidation for an unreachable parked
        // entry — and leaves the existing (stale) cache alone rather than
        // clearing it with nothing to replace it when unreachable.
        for ws in self.workspace_cache.cached_entities() {
            ws.update(cx, |ws, cx| ws.on_theme_changed(false, cx));
        }
    }

    // ---- ctrl-k / cmd-k command palette ---------------------------------
    //
    // docs/backlog.md: "one fuzzy surface over commands AND destinations:
    // the action registry, reviews via the Phase-6 global index, files in
    // the current diff, PRs." Same shell-level-overlay shape as the theme
    // picker just above (open/close/next/prev/choose, a key-context toggle,
    // mutual exclusion with the other overlays) but with a live text input
    // like `workspace::Palette` (jump-to-file) — the ranking/mixing itself
    // is pure logic in `crate::command_palette`, unit-tested there headless;
    // everything here is just wiring live app state into it.

    fn on_open_command_palette(
        &mut self,
        _: &OpenCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Same mutual-exclusion posture as `on_open_theme_picker` — plus
        // `self.command_palette.is_some()` itself, since (unlike the theme
        // picker) there's no reason to special-case "already open" with a
        // refocus: the input already has focus, so ctrl-k firing again a
        // second time can only mean the keymap somehow dispatched it twice.
        if self.command_palette.is_some()
            || self.theme_picker.is_some()
            || self.settings_panel.is_some()
            || self.onboarding.is_some()
            || self.delete_confirm.is_some()
        {
            return;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|ws| ws.read(cx).pr_picker_open())
        {
            return;
        }
        self.close_filter_popover(cx);

        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Type a command, or search reviews/files/PRs\u{2026}")
        });
        let items = self.compute_command_palette_items("", cx);
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    let query = input.read(cx).value().to_string();
                    this.recompute_command_palette(&query, cx);
                }
                InputEvent::PressEnter { .. } => this.command_palette_choose(window, cx),
                InputEvent::Blur => this.close_command_palette(window, cx),
                InputEvent::Focus => {}
            },
        );
        // Focus the INPUT itself (not `self.focus_handle`) — same reasoning
        // as `workspace::Workspace::on_jump_to_file`: up/down/escape/enter
        // are bound to "AppShell && CommandPaletteOpen", a context stamped
        // on this shell's ROOT node (see `render`'s `key_context` builder),
        // which is an ancestor of the input regardless of which specific
        // node has literal focus — so those bindings still resolve fine
        // while the input holds focus and receives the actual typing.
        input.update(cx, |input, cx| input.focus(window, cx));
        self.command_palette = Some(CommandPalette {
            input,
            _subscription: subscription,
            items,
            selected: 0,
        });
        cx.notify();
    }

    /// Closes the palette, restoring focus the same way
    /// [`Self::close_theme_picker`] does (Phase 7 D0 fixed a real regression
    /// in exactly this spot for the PR picker: focusing the wrong target
    /// left a binding's context off the dispatch path) — the active
    /// workspace's own handle if one exists, else the shell's.
    fn close_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_palette.take().is_some() {
            match &self.active {
                Some(ws) => window.focus(&ws.focus_handle(cx), cx),
                None => window.focus(&self.focus_handle, cx),
            }
            cx.notify();
        }
    }

    fn on_command_palette_close(
        &mut self,
        _: &CommandPaletteClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_command_palette(window, cx);
    }

    fn on_command_palette_next(
        &mut self,
        _: &CommandPaletteNext,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(palette) = &mut self.command_palette
            && !palette.items.is_empty()
        {
            palette.selected = (palette.selected + 1).min(palette.items.len() - 1);
            cx.notify();
        }
    }

    fn on_command_palette_prev(
        &mut self,
        _: &CommandPalettePrev,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(palette) = &mut self.command_palette {
            palette.selected = palette.selected.saturating_sub(1);
            cx.notify();
        }
    }

    fn on_command_palette_choose(
        &mut self,
        _: &CommandPaletteChoose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command_palette_choose(window, cx);
    }

    /// Shared by the keyboard path ([`Self::on_command_palette_choose`]) and
    /// a row's own mouse click ([`Self::render_command_palette`]).
    fn command_palette_choose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(palette) = &self.command_palette else {
            return;
        };
        let Some(row) = palette.items.get(palette.selected) else {
            return;
        };
        let kind = row.kind;
        let id = row.id.clone();
        self.activate_palette_row(kind, id, window, cx);
    }

    /// Runs whatever a palette row's `kind`/`id` mean. Closes the palette
    /// FIRST in every case (task requirement: "palette closes, then the
    /// action runs") — load-bearing, not just cosmetic, for the Command
    /// case: this palette is now one of `on_open_theme_picker`'s (etc.)
    /// mutual-exclusion checks, so dispatching e.g. `OpenThemePicker` while
    /// `self.command_palette` were still `Some` would silently no-op
    /// against its own guard.
    fn activate_palette_row(
        &mut self,
        kind: crate::command_palette::PaletteKind,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::command_palette::PaletteKind;
        self.close_command_palette(window, cx);
        match kind {
            PaletteKind::Command => {
                // Same dispatch primitives `--automation`'s `Cmd::Action`
                // handler uses (`automation/mod.rs`) — build the action from
                // its registered name, then dispatch it exactly like a
                // keystroke or a menu click would (`window.dispatch_action`
                // bubbles through the same action-handler chain regardless
                // of trigger). A build failure here (stale id from a query
                // typed before some app-state change removed the action) is
                // a silent no-op, matching this codebase's general posture
                // for a stale-click race (see e.g. `open_review_row`'s own
                // doc comment).
                if let Ok(action) = cx.build_action(&id, None) {
                    window.dispatch_action(action, cx);
                }
            }
            PaletteKind::Review => {
                // Same path a sidebar card click takes.
                self.open_review_row(&id, window, cx);
            }
            PaletteKind::File => {
                // Same path a file-list row click takes
                // (`Workspace::select_file`).
                if let (Some(ws), Ok(index)) = (&self.active, id.parse::<usize>()) {
                    ws.update(cx, |ws, cx| ws.select_file(index, window, cx));
                }
            }
            PaletteKind::Pr => {
                // Same path the PR picker's own row click takes
                // (`Workspace::open_pr`).
                if let (Some(ws), Ok(number)) = (&self.active, id.parse::<u64>()) {
                    ws.update(cx, |ws, cx| ws.open_pr(number, window, cx));
                }
            }
        }
    }

    /// Rebuilds `self.command_palette.items` for `query` — the input's
    /// `InputEvent::Change` handler installed in
    /// [`Self::on_open_command_palette`]. Resets the cursor to the top:
    /// keeping a numeric selection index stable across a result-set reshape
    /// would as often land on an unrelated row as the intended one.
    fn recompute_command_palette(&mut self, query: &str, cx: &mut Context<Self>) {
        let items = self.compute_command_palette_items(query, cx);
        if let Some(palette) = &mut self.command_palette {
            palette.items = items;
            palette.selected = 0;
        }
        cx.notify();
    }

    /// Gathers fresh candidates from every live source and ranks/mixes them
    /// for `query` via [`crate::command_palette::rank_and_mix`]. Cheap
    /// enough to call on every keystroke: a few dozen commands, the review
    /// index (already in memory), the active workspace's file list
    /// (likewise), and the PR-picker's own in-memory cache (never a fresh
    /// `gh` fetch — task's own scope note: opening/typing in the palette
    /// must never trigger one).
    fn compute_command_palette_items(
        &self,
        query: &str,
        cx: &mut Context<Self>,
    ) -> Vec<PaletteRow> {
        let candidates = self.command_palette_candidates(cx);
        crate::command_palette::rank_and_mix(query, &candidates)
            .into_iter()
            .map(PaletteRow::from)
            .collect()
    }

    /// Builds the full, unranked candidate list — every group's entire
    /// membership, in whatever order makes for a sensible empty-query
    /// default (see [`crate::command_palette::rank_and_mix`]'s doc comment
    /// on why that only matters for the empty-query cap).
    fn command_palette_candidates(
        &self,
        cx: &mut Context<Self>,
    ) -> Vec<crate::command_palette::PaletteCandidate> {
        use crate::command_palette::{
            PaletteCandidate, PaletteKind, command_priority, humanize_action_name,
            is_palette_command,
        };

        // Commands: `cx.all_action_names()` is THE SAME registry
        // `--automation`'s `actions` command enumerates — see
        // `is_palette_command`'s doc comment for the narrow allow/deny
        // filter on top of it. Collected to an owned `Vec` first (rather
        // than `.iter().filter().map()` straight off the borrowed slice) to
        // sidestep the borrowed-slice's double-reference item type
        // entirely — clearer than fighting deref coercion for a list this
        // small.
        let action_names: Vec<&'static str> = cx.all_action_names().to_vec();
        let mut commands: Vec<PaletteCandidate> = Vec::new();
        for name in action_names {
            if !is_palette_command(name) {
                continue;
            }
            let short = name.rsplit_once("::").map_or(name, |(_, s)| s);
            let label = humanize_action_name(short);
            let subtitle = keybinding_hint(name, cx).unwrap_or_default();
            commands.push(PaletteCandidate {
                kind: PaletteKind::Command,
                id: name.to_string(),
                search_text: label.clone(),
                label,
                subtitle,
            });
        }
        // Curated priority first (see `command_priority`'s doc comment),
        // alphabetical among the rest — only affects the empty-query
        // default cap; a real query re-ranks by fuzzy score regardless.
        commands.sort_by(|a, b| {
            let sa = a.id.rsplit_once("::").map_or(a.id.as_str(), |(_, s)| s);
            let sb = b.id.rsplit_once("::").map_or(b.id.as_str(), |(_, s)| s);
            command_priority(sa)
                .cmp(&command_priority(sb))
                .then_with(|| a.label.cmp(&b.label))
        });

        // Reviews: the Phase-6 global index, same source the sidebar
        // renders from. `self.index.entries()` is already recency-sorted
        // (`ReviewIndex::load`'s own doc comment), which is exactly the
        // order a "recent reviews" empty-query default wants. Condensed to
        // one line per the task's own spec ("repo/PR + age") via
        // `repo_label`/`relative_age` — the same two helpers
        // `render_review_card` uses for its own line 1 — with the title
        // folded into `search_text` only, so typing a review's title still
        // finds it even though the condensed label omits it.
        let reviews: Vec<PaletteCandidate> = self
            .index
            .entries()
            .iter()
            .map(|entry| {
                let repo = dv_core::repo_label(&entry.location, entry.remote.as_ref());
                let age = relative_age(entry.updated_ms);
                PaletteCandidate {
                    kind: PaletteKind::Review,
                    id: entry.review_id.clone(),
                    label: format!("{repo} \u{b7} {age}"),
                    subtitle: entry.title.clone(),
                    search_text: format!("{repo} {}", entry.title),
                }
            })
            .collect();

        // Files/PRs: only the ACTIVE workspace has either — no active
        // review means both groups are simply empty (`rank_and_mix`
        // already omits an empty group's header).
        let mut files: Vec<PaletteCandidate> = Vec::new();
        let mut prs: Vec<PaletteCandidate> = Vec::new();
        if let Some(ws) = &self.active {
            let ws = ws.read(cx);
            files = ws
                .files()
                .iter()
                .enumerate()
                .map(|(index, file)| PaletteCandidate {
                    kind: PaletteKind::File,
                    id: index.to_string(),
                    label: file.path.clone(),
                    subtitle: format!("{:?}", file.status),
                    search_text: file.path.clone(),
                })
                .collect();
            // Task's own scope note: reuse the ctrl-g picker's cache ONLY —
            // never trigger a `gh pr list` fetch just because the palette
            // opened. `pr_list_cache()` is a plain read, never a fetch.
            prs = ws
                .pr_list_cache()
                .into_iter()
                .flatten()
                .map(|pr| PaletteCandidate {
                    kind: PaletteKind::Pr,
                    id: pr.number.to_string(),
                    label: format!("#{} {}", pr.number, pr.title),
                    subtitle: format!("by {}", pr.author),
                    search_text: format!("{} {} {}", pr.number, pr.title, pr.author),
                })
                .collect();
        }

        commands
            .into_iter()
            .chain(reviews)
            .chain(files)
            .chain(prs)
            .collect()
    }

    /// The command palette overlay, when open — same shared modal recipe as
    /// `render_theme_picker`/`workspace::render_palette`
    /// (R2 item 6), with grouped
    /// rows: a quiet 11px header (`render_sidebar_header`'s own treatment)
    /// whenever the ranked list's `kind` changes from the previous row —
    /// cheap because `rank_and_mix` already emits items pre-grouped
    /// contiguously in Commands/Reviews/Files/PRs order, so this only ever
    /// needs to compare adjacent entries, never re-sort.
    fn render_command_palette(&self, cx: &mut Context<Self>) -> Option<Div> {
        const VISIBLE: usize = 16;
        let palette = self.command_palette.as_ref()?;
        let theme = cx.theme();
        let dv = themes::dv_theme(cx);
        let surface_active = dv.surface_active;
        let seam = dv.modal_border;
        let backdrop = dv.backdrop;
        let panel_bg = theme.sidebar;
        let fg = theme.foreground;
        let muted = theme.muted_foreground;
        let chip_bg = theme.tokens.muted;
        let text_secondary = dv.text_secondary;

        let first = palette.selected.saturating_sub(VISIBLE - 1);
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut prev_kind: Option<crate::command_palette::PaletteKind> = None;
        for (i, row) in palette.items.iter().enumerate().skip(first).take(VISIBLE) {
            if prev_kind != Some(row.kind) {
                rows.push(
                    div()
                        .px_2()
                        .pt_1()
                        .text_size(px(11.))
                        .text_color(text_secondary)
                        .child(palette_group_label(row.kind))
                        .into_any_element(),
                );
                prev_kind = Some(row.kind);
            }
            let selected = i == palette.selected;
            let label = row.label.clone();
            let subtitle = row.subtitle.clone();
            // Commands' subtitle is a real keybinding hint (`keybinding_hint`)
            // — chip-styled like the footer's own `Kbd` legend
            // rather than plain text, so it reads
            // as "this is a shortcut" and not as a review's title/a file's
            // status. Other kinds' subtitles (age/status/author) stay plain
            // muted text — they're prose, not a keycap.
            let is_command_hint = row.kind == crate::command_palette::PaletteKind::Command;
            rows.push(
                h_flex()
                    .id(("command-palette-row", i))
                    .h(px(30.))
                    .mx_1()
                    .gap_2()
                    .px_2()
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |el| el.bg(surface_active))
                    .hover(|el| el.bg(surface_active.opacity(0.5)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            if let Some(palette) = &mut this.command_palette {
                                palette.selected = i;
                            }
                            this.command_palette_choose(window, cx);
                        }),
                    )
                    .child(div().flex_1().min_w(px(0.)).truncate().child(label))
                    .when(!subtitle.is_empty() && is_command_hint, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_color(muted)
                                .bg(chip_bg)
                                .py_0p5()
                                .px_1()
                                .rounded(theme.radius.half())
                                .text_size(px(11.))
                                .child(subtitle.clone()),
                        )
                    })
                    .when(!subtitle.is_empty() && !is_command_hint, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_color(muted)
                                .truncate()
                                .child(subtitle),
                        )
                    })
                    .into_any_element(),
            );
        }

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(backdrop)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.close_command_palette(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(48.))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            v_flex()
                                .w(px(560.))
                                .max_w_full()
                                .max_h(px(480.))
                                .overflow_hidden()
                                .p_2()
                                .gap_2()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .bg(panel_bg)
                                .text_color(fg)
                                .text_size(px(13.))
                                .border_1()
                                .border_color(seam)
                                .rounded_lg()
                                .shadow_lg()
                                .child(
                                    div()
                                        .px_2()
                                        .pt_1()
                                        .text_size(px(11.))
                                        .text_color(muted)
                                        .child(
                                            "Go to anything \u{b7} > commands, # PRs \u{b7} \
                                             enter to open, esc to close",
                                        ),
                                )
                                .child(gpui_component::input::Input::new(&palette.input).small())
                                .child(v_flex().w_full().children(rows).when(
                                    palette.items.is_empty(),
                                    |el| {
                                        el.child(
                                            div()
                                                .px_2()
                                                .py_1()
                                                .text_color(muted)
                                                .child("no matches"),
                                        )
                                    },
                                )),
                        ),
                ),
        )
    }

    /// Re-resolve the active theme from `follow_os_appearance`'s light/dark
    /// pair against the window's live OS appearance — called by the
    /// `observe_window_appearance` subscription set up in [`Self::new`] on
    /// every genuine OS light/dark change while follow-OS is on. A no-op
    /// when follow-OS is off (the observer stays registered regardless, so
    /// turning follow-OS back on doesn't need to re-register anything).
    fn resolve_follow_os(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings.follow_os_appearance {
            return;
        }
        let os_is_dark = matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        );
        let name = self.settings.effective_theme(os_is_dark).to_string();
        self.apply_resolved_theme(&name, window, cx);
    }

    /// Explicit row selection (a sidebar card click, or
    /// `{"cmd":"select_review"}`): pin `review_id` and reopen it — including
    /// a SUBMITTED review, read-only (docs/phase-6-review-navigator.md's
    /// headline incident fix: a review only reachable before via the CLI or
    /// `pick_review`'s auto-selection is now a click away). An id with no
    /// matching index entry (a stale click racing a hydration that dropped
    /// it) is a silent no-op here — [`Self::automation_select_review`]
    /// checks first and surfaces that case as an error instead.
    fn open_review_row(&mut self, review_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.index.get(review_id) else {
            return;
        };
        let location = entry.location.clone();
        let source = entry.source.clone();
        self.open_review(
            location,
            source,
            None,
            Some(review_id.to_string()),
            window,
            cx,
        );
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
            self.open_review(location, DiffSource::WorkingTree, None, None, window, cx);
        }
    }

    // ---- "Open anything" quick-open (R2) -----------------------------------

    /// Enter in the quick-open input. Resolution is deliberately cheap and
    /// offline: an `owner/repo#N` (or PR URL) target is looked up against
    /// the index's existing PR linkage only — never a `git remote` call per
    /// known repo, which would spawn git across every location (and worse,
    /// touch WSL distros) on every Enter. A repo dv has never seen a PR for
    /// simply isn't found: the error says to open the repo folder first
    /// (after which ctrl-g's PR picker — or this field again — works).
    pub(crate) fn quick_open_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.quick_open.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let target = match dv_core::parse_open_target(&text) {
            Ok(target) => target,
            Err(message) => {
                self.set_quick_open_error(message, cx);
                return;
            }
        };
        match target {
            dv_core::OpenTarget::Path(path) => {
                let Ok(location) = RepoLocation::from_path_arg(&path) else {
                    self.set_quick_open_error(format!("not a repo path: {path}"), cx);
                    return;
                };
                // Catch the typo here, where the inline error can say so —
                // `open_review` on a nonexistent local path would open a
                // workspace that fails with a less-attributable git error.
                // WSL locations skip the check (existence would need a
                // distro round trip); their failure surfaces in-workspace,
                // same as the CLI's `--wsl` flag.
                if let RepoLocation::Local(p) = &location
                    && !p.is_dir()
                {
                    self.set_quick_open_error(format!("no such directory: {path}"), cx);
                    return;
                }
                self.clear_quick_open(window, cx);
                self.open_review(location, DiffSource::WorkingTree, None, None, window, cx);
            }
            dv_core::OpenTarget::PrNumber(number) => {
                let Some(ws) = self.active.clone() else {
                    self.set_quick_open_error(
                        format!("no active repo for #{number} — use owner/repo#{number} or open a repo first"),
                        cx,
                    );
                    return;
                };
                // `open_pr` refuses SILENTLY when a save/submit/PR-load is
                // in flight — fine for the picker (it stays open,
                // retriable), fatal here where the input was about to be
                // cleared: the typed target would vanish with no action
                // and no error (R2 review, P3). Check first.
                if ws.read(cx).pr_open_busy() {
                    self.set_quick_open_error(
                        "busy — a comment save or PR load is in flight; try again in a moment",
                        cx,
                    );
                    return;
                }
                self.clear_quick_open(window, cx);
                // The active workspace's own PR-open path (same one the
                // ctrl-g picker's Enter uses) — validates in-flight
                // comment saves before switching, surfaces fetch errors.
                ws.update(cx, |ws, cx| ws.open_pr(number, window, cx));
            }
            dv_core::OpenTarget::Pr {
                host,
                owner,
                repo,
                number,
            } => {
                let slug = format!("{host}/{owner}/{repo}");
                // Case-insensitive: origin-URL casing drifts for the same
                // GitHub repo — the same precedent as `load_pr`'s adoption
                // scan and `sync_index_pr_status` (both eq_ignore_ascii_case
                // their slugs; R2 review, P3).
                let location = self
                    .index
                    .entries()
                    .iter()
                    .find(|e| {
                        e.remote
                            .as_ref()
                            .is_some_and(|r| r.slug.eq_ignore_ascii_case(&slug))
                    })
                    .map(|e| e.location.clone());
                let Some(location) = location else {
                    self.set_quick_open_error(
                        format!(
                            "no known local clone of {owner}/{repo} — open the repo folder first"
                        ),
                        cx,
                    );
                    return;
                };
                // "Same repo" by slug identity, NOT `RepoLocation` equality:
                // the workspace's location is rev-parse-normalized while
                // the index entry keeps whatever spelling hydration saw —
                // the exact textual-divergence trap `refresh_badge`'s
                // lookup documents. A false miss here wouldn't just be
                // slow: it would stash the active workspace and cold-build
                // a DUPLICATE workspace for the repo that's already open
                // (R2 review, P3). A slug-less active review (plain local,
                // no PR linkage) falls back to location equality — its
                // repo can still be the target's host clone.
                let same_repo = self.active.is_some() && {
                    let active_slug = self
                        .selected_review_id
                        .as_deref()
                        .and_then(|id| self.index.get(id))
                        .and_then(|e| e.remote.as_ref().map(|r| r.slug.clone()));
                    match active_slug {
                        Some(active) => active.eq_ignore_ascii_case(&slug),
                        None => self
                            .active
                            .as_ref()
                            .is_some_and(|ws| ws.read(cx).location() == &location),
                    }
                };
                if same_repo {
                    let Some(ws) = self.active.clone() else {
                        return;
                    };
                    // Same silent-refusal guard as the PrNumber arm above.
                    if ws.read(cx).pr_open_busy() {
                        self.set_quick_open_error(
                            "busy — a comment save or PR load is in flight; try again in a moment",
                            cx,
                        );
                        return;
                    }
                    self.clear_quick_open(window, cx);
                    ws.update(cx, |ws, cx| ws.open_pr(number, window, cx));
                } else {
                    self.clear_quick_open(window, cx);
                    // A different (or no) active repo: the `dv pr <n>`
                    // launch shape — fresh workspace for that location with
                    // the PR fetch pending (`open_review`'s cache-miss
                    // branch; no pin, so the LRU fast path can't
                    // mis-reactivate some other review).
                    self.open_review(
                        location,
                        DiffSource::WorkingTree,
                        Some(number),
                        None,
                        window,
                        cx,
                    );
                }
            }
        }
    }

    /// `{"cmd":"quick_open","text":"..."}`: stuff the input and submit —
    /// the scripted stand-in for click-focus + per-key typing + Enter
    /// (coordinate clicks are the one flaky automation primitive; see
    /// CLAUDE.md's stale-frame caveat). Everything from the parse on is
    /// the real [`Self::quick_open_submit`] path.
    #[cfg(feature = "automation")]
    pub(crate) fn automation_quick_open(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.quick_open.update(cx, |input, cx| {
            input.set_value(text.to_string(), window, cx)
        });
        self.quick_open_submit(window, cx);
    }

    fn set_quick_open_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.quick_open_error = Some(message.into());
        cx.notify();
    }

    /// Successful submit: blank the input (ready for the next target) and
    /// drop any stale error.
    fn clear_quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.quick_open
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.quick_open_error = None;
        cx.notify();
    }

    /// Which focus handle currently owns window focus, as a stable label
    /// for `--automation` assertions (Phase 7 D0: the PR-picker Enter
    /// regression was invisible because focus location wasn't assertable).
    /// `"workspace"` means the active `Workspace`'s own handle holds focus
    /// (so its `Workspace && PrPickerOpen` Enter binding is on the
    /// dispatch path); `"shell"` means `AppShell`'s handle (theme picker /
    /// settings panel); `"none"` otherwise.
    #[cfg(feature = "automation")]
    pub(crate) fn focus_label(&self, window: &Window, cx: &App) -> &'static str {
        if let Some(ws) = &self.active
            && ws.focus_handle(cx).is_focused(window)
        {
            return "workspace";
        }
        if self.focus_handle.is_focused(window) {
            return "shell";
        }
        "none"
    }

    /// Whether a shell-level overlay that must not be stacked under the PR
    /// picker is currently open. `Workspace::on_open_pr_picker` consults
    /// this directly (review finding P3: `shell_focus_handle.is_focused()`
    /// was a false proxy for "an overlay is open" — ordinary sidebar-chrome
    /// clicks with no overlay open also bubble focus onto the shell handle,
    /// since none of that chrome is itself focusable, which silently
    /// declined a legitimate mouse-triggered PR-picker open). Only the
    /// theme picker is a genuine stacking hazard (non-modal, so a click can
    /// reach the "PRs · ctrl-g" hint button behind it); the settings panel
    /// is a modal `inset_0().occlude()` overlay that click can never reach
    /// in the first place, but it's included anyway for symmetry with
    /// `on_open_theme_picker`'s own decline guard.
    pub(crate) fn overlay_open(&self) -> bool {
        self.theme_picker.is_some()
            || self.settings_panel.is_some()
            || self.onboarding.is_some()
            || self.command_palette.is_some()
    }

    /// Closes the sidebar filter popover if open, notifying on change. The
    /// popover is pure-mouse (no `window.focus` involved, cross-cutting
    /// risk E), but its `inset_0().occlude()` backdrop is mounted last
    /// (bottom of `Render for AppShell`) and will paint/hit-test on top of
    /// whichever overlay opens after it unless that overlay's own opener
    /// closes this first — `on_open_theme_picker`/`on_open_settings` (this
    /// file) and `Workspace::on_open_pr_picker` (workspace.rs, via this
    /// method — `filter_popover_open` itself is private to this module) all
    /// call this on entry (review finding).
    pub(crate) fn close_filter_popover(&mut self, cx: &mut Context<Self>) {
        if self.filter_popover_open {
            self.filter_popover_open = false;
            cx.notify();
        }
    }

    /// Semantic state for `--automation`: the sidebar plus the active
    /// review's own dump (see [`Workspace::automation_state`]).
    #[cfg(feature = "automation")]
    pub(crate) fn automation_state(&self, cx: &App) -> serde_json::Value {
        use serde_json::json;
        let theme = cx.theme();
        json!({
            // Every location the app currently knows a review for, live for
            // the whole session — NOT `self.recent.entries()` (review
            // finding: `recent.json` is a startup-only seed, read-only as
            // of S6c, so scoping this to it silently stopped covering any
            // repo opened after launch, exactly the regression
            // `Self::known_locations`'s doc comment already calls out for
            // `hydrate_index`/`refresh_all_badges`).
            "recent": self.known_locations().iter().map(|l| l.display_name()).collect::<Vec<_>>(),
            "selected_review_id": self.selected_review_id,
            // Phase 7 D4 instrumentation: wall time of the most recent
            // `open_review` (sidebar switch / new-review open), synchronous
            // body only — see the field's doc comment for what a small
            // value here does and doesn't prove.
            "last_switch_ms": self.last_switch_ms,
            // Phase 7 D1 instrumentation: whether that switch reactivated a
            // parked `WorkspaceCache` entry (`true`) or built a fresh
            // `Workspace` from zero (`false`) — see the field's doc comment
            // for why `last_switch_ms` alone can't tell a script this.
            "last_switch_cache_hit": self.last_switch_cache_hit,
            // Phase 7 D1: the workspace LRU's live occupancy, for asserting
            // the dual count/byte budget holds (`entries <= max_entries`,
            // `bytes <= max_bytes`, always) and that eviction actually drops
            // entries — `keys` (review ids, MRU-first) lets a script check
            // *which* entry survived a forced eviction.
            "workspace_cache": json!({
                "entries": self.workspace_cache.entries.len(),
                "bytes": self.workspace_cache.total_bytes(cx),
                "keys": self.workspace_cache.lru.iter().collect::<Vec<_>>(),
                "max_entries": self.workspace_cache.max_entries,
                "max_bytes": self.workspace_cache.max_bytes,
            }),
            // The sidebar's actual rendered row list (docs/phase-6-review-
            // navigator.md deliverables 3/4), in render order — filtered by
            // `settings.sidebar_filters` and, when `sidebar_grouping` isn't
            // `none`, grouped with a header above each run:
            // `{"header": "..."}` / `{"review_id": "..."}`. As of S6c this
            // diverges from `index`'s own order/coverage (headers, and a
            // filtered-out review is present in `index` but absent here),
            // which is why this exists as its own field rather than
            // something scripts derive by re-sorting/filtering `index`
            // themselves. See `Self::visible_sidebar_items`, the single
            // function both this and the real `uniform_list` render from.
            "sidebar": self.visible_sidebar_items().iter().map(|item| match item {
                SidebarItem::Header { label, .. } => json!({"header": label}),
                SidebarItem::Review(idx) => json!({
                    "review_id": self.index.entries()[*idx].review_id,
                }),
            }).collect::<Vec<_>>(),
            "workspace": self.active.as_ref().map(|ws| ws.read(cx).automation_state()),
            // Per-location badge dump (docs/phase-3-github.md deliverable
            // 3/4), same order/coverage as `recent` above — see that
            // field's comment for why this is `known_locations()`, not
            // `self.recent.entries()`.
            "badges": self.known_locations().iter().map(|location| {
                let badge = self.badges.get(location);
                json!({
                    "title": location.display_name(),
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
            // Cross-repo review index (docs/phase-6-review-navigator.md
            // deliverable 1): every review the app has hydrated across
            // every repo it knows about — NOT scoped to `recent` (a
            // location can appear here without ever showing up in
            // `recent.json`, and vice versa until its first hydration
            // completes). This is also the sidebar's actual data source
            // as of S6c (see `"sidebar"`, above, and
            // `Render::render`'s review-card list) — `index` still
            // carries every field (including ones the flat S6c list
            // doesn't render, like `health`) so a script can assert
            // hydration behavior without a screenshot.
            "index": self.index.entries().iter().map(|e| json!({
                "review_id": e.review_id,
                "title": e.title,
                "location": e.location.display_name(),
                "source": source_label(&e.source),
                "state": index_state_word(&e.state),
                "open_comments": e.open_comments,
                "pr": e.pr_status.as_ref().map(|pr| json!({
                    "state": pr_state_word(pr.state),
                    "is_draft": pr.is_draft,
                    "decision": pr.decision.map(review_decision_word),
                    "checks": checks_word(pr.checks),
                })),
                "health": index_health_word(e.health),
                "diffstat": e.diffstat.map(|ds| json!({
                    "additions": ds.additions,
                    "deletions": ds.deletions,
                })),
                // Merge-conflict indicator (docs/backlog.md "Merge-conflict
                // indicator..."): `null` when hydration hasn't reached a
                // verdict yet (old git, WSL hiccup, not-yet-hydrated) —
                // distinct from `{"conflicted": false, "files": []}`, a
                // real "clean" verdict. See `Self::render_review_card`'s
                // `conflict_count` for the sidebar-rendered form of this.
                "conflict": e.conflict.as_ref().map(|c| json!({
                    "conflicted": c.is_conflicted(),
                    "files": c.files,
                })),
                "last_opened_ms": e.last_opened_ms,
                "archived": e.archived,
            })).collect::<Vec<_>>(),
            "delete_confirm_open": self.delete_confirm.is_some(),
            // Which review a card context menu was last opened over — lets
            // a script confirm its right-click landed on the intended card
            // before dispatching ToggleArchiveReview/DeleteReviewPrompt.
            "menu_review": self.menu_review,
            // Theme deliverable: the currently-applied theme's own name
            // (read off the live global `Theme`, not `self.settings`, so
            // this can never lie about what's actually painted), whether
            // the picker overlay is open, and the resolved mono font family
            // so agents can assert JetBrains Mono is really active without
            // eyeballing a screenshot.
            "theme": theme.theme_name().to_string(),
            "theme_picker_open": self.theme_picker.is_some(),
            // ctrl-k command palette: open flag, live query, item count/
            // cursor, and a few top rows (kind/id/label) so a script can
            // assert filtered results without a screenshot.
            "command_palette": self.command_palette.as_ref().map(|p| json!({
                "query": p.input.read(cx).value().to_string(),
                "items": p.items.len(),
                "selected": p.selected,
                "top": p.items.iter().take(5).map(|row| json!({
                    "kind": match row.kind {
                        crate::command_palette::PaletteKind::Command => "command",
                        crate::command_palette::PaletteKind::Review => "review",
                        crate::command_palette::PaletteKind::File => "file",
                        crate::command_palette::PaletteKind::Pr => "pr",
                    },
                    "id": row.id,
                    "label": row.label.to_string(),
                })).collect::<Vec<_>>(),
            })),
            "mono_font": theme.mono_font_family.to_string(),
            // Settings deliverable (docs/phase-4-settings-and-theming.md
            // deliverable 7): the whole persisted struct, plus whether the
            // panel is open. Note this can legitimately disagree with the
            // top-level `theme`/`mono_font` above while follow-OS is on —
            // those two report what's actually painted; `settings.theme` is
            // the last *explicit* pick (see `Settings::effective_theme`).
            "settings": json!({
                "theme": self.settings.theme,
                "follow_os_appearance": self.settings.follow_os_appearance,
                "light_theme": self.settings.light_theme,
                "dark_theme": self.settings.dark_theme,
                "mono_font": self.settings.mono_font,
                "mono_font_size": self.settings.mono_font_size,
                "context_lines": self.settings.context_lines,
                "view_mode_default": match self.settings.view_mode_default {
                    ViewModeSetting::Unified => "unified",
                    ViewModeSetting::Split => "split",
                },
                // Drag-to-resize deliverable (Phase 4 deliverable 5): the
                // sidebar's own live width. The summary panel's mirror-image
                // `summary_width` lives on the *workspace*, not here — see
                // `Workspace::automation_state`, since it's per-workspace
                // render state, not a shell-level layout knob.
                "sidebar_width": self.settings.sidebar_width,
                "sidebar_visible": self.settings.sidebar_visible,
                "summary_width": self.settings.summary_width,
                // Sidebar grouping/filtering (deliverables 3/4).
                "sidebar_grouping": grouping_word(self.settings.sidebar_grouping),
                "sidebar_filters": json!({
                    "pr_draft": self.settings.sidebar_filters.pr_draft,
                    "pr_open": self.settings.sidebar_filters.pr_open,
                    "pr_merged": self.settings.sidebar_filters.pr_merged,
                    "pr_closed": self.settings.sidebar_filters.pr_closed,
                    "unlinked": self.settings.sidebar_filters.unlinked,
                    "review_draft": self.settings.sidebar_filters.review_draft,
                    "review_comment": self.settings.sidebar_filters.review_comment,
                    "review_approved": self.settings.sidebar_filters.review_approved,
                    "review_changes": self.settings.sidebar_filters.review_changes,
                }),
            }),
            "settings_open": self.settings_panel.is_some(),
            // Quick-open (R2): the inline error, or
            // null — the `quick_open` command's primary assert surface.
            "quick_open_error": self.quick_open_error.as_ref().map(|e| e.to_string()),
            // Onboarding spine (S8e): the whole page's row set, so a script
            // can assert first-run behavior and drift-driven re-provisioning
            // without a screenshot (`{"cmd":"action","name":"OpenOnboarding"}`
            // opens it under automation, which never auto-opens it itself —
            // see `AppShell::new`'s launch-time gate).
            "onboarding": self.onboarding.as_ref().map(|page| json!({
                "first_run": page.first_run,
                "running": page.running,
                "rows": page.rows.iter().map(|row| json!({
                    "id": component_id_word(row.id),
                    "title": row.title,
                    "state": onboarding_row_state_word(&row.state),
                    "detail": onboarding_row_detail(&row.state),
                })).collect::<Vec<_>>(),
            })),
        })
    }

    /// True when nothing is loading — a bare shell counts as settled. Also
    /// folds in the onboarding page's own pending work (S8e review, P3):
    /// without this, `wait_ready` returned as soon as the active workspace
    /// settled even while the page's background `consistency_check` (gh up
    /// to 10s, node detect up to 20s) or a consent-triggered install (up to
    /// 180s) was still running, so a script's very next `state` could read
    /// rows still stuck at `checking` nondeterministically — against this
    /// codebase's "`wait_ready` makes one-shot scripts deterministic"
    /// convention. Placeholder rows still paint immediately on open
    /// (`OnboardingPage::placeholder`); this only delays `wait_ready`
    /// returning until they've resolved.
    #[cfg(feature = "automation")]
    pub(crate) fn automation_settled(&self, cx: &App) -> bool {
        let onboarding_settled = self
            .onboarding
            .as_ref()
            .is_none_or(|page| !page.running && page.installing.is_empty());
        onboarding_settled
            && match &self.active {
                Some(ws) => ws.read(cx).automation_settled(),
                None => true,
            }
    }

    /// Select the nth changed file in the active review. Errors (rather
    /// than silently no-oping) so automation responses never claim a
    /// selection that didn't happen.
    #[cfg(feature = "automation")]
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
    #[cfg(feature = "automation")]
    pub(crate) fn automation_open(
        &mut self,
        location: RepoLocation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_review(location, DiffSource::WorkingTree, None, None, window, cx);
    }

    /// `{"cmd":"select_review","review_id":"..."}`: explicit row selection by
    /// review id — the S6c incident-fix entry point for automation, same
    /// pin-and-reopen path a sidebar card click takes
    /// ([`Self::open_review_row`]). Errors (rather than silently no-oping)
    /// on an unknown id, matching [`Self::automation_select_file`]'s
    /// contract: a script's response must never claim a selection that
    /// didn't happen.
    #[cfg(feature = "automation")]
    pub(crate) fn automation_select_review(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        if self.index.get(&id).is_none() {
            anyhow::bail!("unknown review id: {id}");
        }
        self.open_review_row(&id, window, cx);
        Ok(())
    }

    /// `{"cmd":"open_pr","number":N}`: open PR `number` in the active
    /// review's workspace (`Workspace::open_pr`). Errors (rather than
    /// silently no-oping) when there's no active review, matching
    /// `automation_select_file`'s contract.
    #[cfg(feature = "automation")]
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

    /// `{"cmd":"set_setting","key":"...","value":...}`: apply one setting
    /// change through the exact same helper methods the settings panel's
    /// own controls call — so a script exercises the real live-apply +
    /// persist path, not a parallel one. `key` matches `Settings`'s JSON
    /// field names one-to-one.
    #[cfg(feature = "automation")]
    pub(crate) fn automation_set_setting(
        &mut self,
        key: &str,
        value: serde_json::Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        match key {
            "theme" => {
                let requested = value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("theme must be a string"))?;
                let name = themes::names()
                    .find(|n| *n == requested)
                    .ok_or_else(|| anyhow::anyhow!("unknown theme: {requested}"))?;
                self.choose_theme(name, window, cx);
            }
            "follow_os_appearance" => {
                let on = value
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("follow_os_appearance must be a bool"))?;
                self.set_follow_os_appearance(on, window, cx);
            }
            "light_theme" => {
                let name = value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("light_theme must be a string"))?
                    .to_string();
                self.set_light_theme(name, window, cx)?;
            }
            "dark_theme" => {
                let name = value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("dark_theme must be a string"))?
                    .to_string();
                self.set_dark_theme(name, window, cx)?;
            }
            "mono_font" => {
                let family = value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("mono_font must be a string"))?
                    .to_string();
                self.set_mono_font(family, window, cx);
            }
            "mono_font_size" => {
                let size = value
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("mono_font_size must be a number"))?
                    as f32;
                self.set_mono_font_size(size, cx);
            }
            "context_lines" => {
                let n = value.as_u64().ok_or_else(|| {
                    anyhow::anyhow!("context_lines must be a non-negative integer")
                })? as u32;
                self.set_context_lines(n, cx);
            }
            "view_mode_default" => {
                let mode = match value.as_str() {
                    Some("unified") => ViewModeSetting::Unified,
                    Some("split") => ViewModeSetting::Split,
                    _ => anyhow::bail!("view_mode_default must be \"unified\" or \"split\""),
                };
                self.set_view_mode_default(mode, cx);
            }
            "sidebar_width" => {
                let width = value
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("sidebar_width must be a number"))?
                    as f32;
                self.set_sidebar_width(width, cx);
            }
            "summary_width" => {
                let width = value
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("summary_width must be a number"))?
                    as f32;
                self.set_summary_width(width, cx);
            }
            "sidebar_visible" => {
                let visible = value
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("sidebar_visible must be a bool"))?;
                self.set_sidebar_visible(visible, window, cx);
            }
            "sidebar_grouping" => {
                let requested = value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("sidebar_grouping must be a string"))?;
                let grouping = match requested {
                    "none" => SidebarGrouping::None,
                    "repo" => SidebarGrouping::Repo,
                    "status" => SidebarGrouping::Status,
                    "pr" => SidebarGrouping::Pr,
                    other => anyhow::bail!("unknown sidebar_grouping: {other}"),
                };
                self.set_sidebar_grouping(grouping, cx);
            }
            "sidebar_filters" => {
                let filters: SidebarFilters = serde_json::from_value(value)
                    .map_err(|e| anyhow::anyhow!("invalid sidebar_filters: {e}"))?;
                self.set_sidebar_filters(filters, cx);
            }
            other => anyhow::bail!("unknown setting: {other}"),
        }
        Ok(())
    }

    // ---- Settings panel ---------------------------------------------------

    fn on_open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_panel.is_some()
            || self.theme_picker.is_some()
            || self.onboarding.is_some()
            || self.command_palette.is_some()
        {
            return;
        }
        // Decline while the active workspace's own PR picker is open — same
        // reasoning and same review finding (P2) as `on_open_theme_picker`'s
        // matching guard just above; the settings panel's `inset_0().occlude()`
        // modal would otherwise fully cover the PR picker instead of merely
        // overlapping it.
        if self
            .active
            .as_ref()
            .is_some_and(|ws| ws.read(cx).pr_picker_open())
        {
            return;
        }
        // See `on_open_theme_picker`'s matching comment: the filter popover
        // isn't part of this guard's own is_some() checks (mouse-only, no
        // focus move), so it can still be open here — close it so its
        // backdrop doesn't stack on top of the settings panel.
        self.close_filter_popover(cx);
        let input =
            cx.new(|cx| InputState::new(window, cx).default_value(self.settings.mono_font.clone()));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event {
                    let family = input.read(cx).value().to_string();
                    this.set_mono_font(family, window, cx);
                }
            },
        );
        self.settings_panel = Some(SettingsPanel {
            mono_font_input: input,
            _subscription: subscription,
        });
        // Capture focus onto the shell itself while the panel is open — same
        // reasoning as the theme picker's `window.focus` call (its escape
        // binding lives in the shell's own key context, and this sidesteps
        // ambiguity with whatever the active workspace's own bindings are).
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn close_settings_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_panel.take().is_some() {
            match &self.active {
                Some(ws) => window.focus(&ws.focus_handle(cx), cx),
                None => window.focus(&self.focus_handle, cx),
            }
            cx.notify();
        }
    }

    fn on_settings_close(
        &mut self,
        _: &SettingsClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_settings_panel(window, cx);
    }

    // ---- Onboarding (S8e) --------------------------------------------------

    /// Every WSL distro this app currently knows a review for AND that
    /// already has a live host connection — the boot-storm-safe distro list
    /// for a passive/launch-time consistency check (mirrors
    /// `Self::refresh_all_badges`/`Self::hydrate_index`'s own
    /// `has_running_host` filter exactly; see the cross-cutting risk in
    /// `dv_core::provision`'s module doc). `Self::open_review`'s own
    /// per-distro check is the OTHER legitimate trigger and does NOT use
    /// this — opening a WSL repo right now implies that distro is already
    /// live, regardless of what this walk would report for it.
    fn live_wsl_distros(&self) -> Vec<String> {
        let mut distros: Vec<String> = Vec::new();
        for location in self.known_locations() {
            if let RepoLocation::Wsl { distro, .. } = location
                && dv_core::remote::manager::has_running_host(&distro)
                && !distros.contains(&distro)
            {
                distros.push(distro);
            }
        }
        distros
    }

    /// `(ComponentId, title)` placeholders for `distros_allowed`, in the
    /// exact title shape `dv_core::provision::consistency_check` itself
    /// produces (`gh` once, unsuffixed; the other three per distro,
    /// `"<base title> — <distro>"`) — so a freshly opened onboarding page
    /// never visibly reflows into a different row set once the real
    /// `ConsistencyReport` lands, only each row's state changing from
    /// `Checking` to its real answer. When `distros_allowed` is empty (no
    /// WSL distro currently running — the exact first-run-with-no-distro
    /// case), `consistency_check` itself only ever checks `gh` (see its own
    /// module doc: it never boots a distro to check the other three), so
    /// this synthesizes generic, unsuffixed placeholders for `dv-host`/
    /// `dv-cli`/`node`+`vtsls` — [`Self::pad_no_distro_rows`] fills in their
    /// matching `Skipped` state once the report lands — so the page still
    /// shows all four components instead of silently dropping to one (S8e
    /// review, P2).
    fn onboarding_titles(distros_allowed: &[String]) -> Vec<(ComponentId, String)> {
        let mut titles = vec![(ComponentId::GhCli, ComponentId::GhCli.title().to_string())];
        // Mirrors `consistency_check`'s own unconditional, host-side
        // Windows-only row — same order, so placeholders never reflow.
        #[cfg(windows)]
        titles.push((
            ComponentId::DvOnPath,
            ComponentId::DvOnPath.title().to_string(),
        ));
        if distros_allowed.is_empty() {
            for id in [
                ComponentId::DvHost,
                ComponentId::DvCli,
                ComponentId::NodeVtsls,
            ] {
                titles.push((id, id.title().to_string()));
            }
        }
        for distro in distros_allowed {
            for id in [
                ComponentId::DvHost,
                ComponentId::DvCli,
                ComponentId::NodeVtsls,
            ] {
                titles.push((id, format!("{} — {distro}", id.title())));
            }
        }
        titles
    }

    /// Open the onboarding page: placeholder rows immediately (first paint
    /// never waits on a WSL round trip), then dispatch the real check in
    /// the background. `first_run` is read fresh from `SetupState` rather
    /// than threaded through by every caller, so a manually reopened page
    /// (the sidebar button, `{"cmd":"action","name":"OpenOnboarding"}`)
    /// after the marker's already been stamped correctly shows the plain
    /// "Setup status" header, not "Welcome to dv" again.
    ///
    /// The distro list unions three sources, most-specific first:
    ///
    /// - [`Self::live_wsl_distros`] (distros with a live dv-host for a
    ///   known repo);
    /// - every distro that is ACTUALLY running right now, per
    ///   [`dv_core::remote::manager::running_distros`]'s non-booting
    ///   `wsl.exe --list --running` probe — unioned in off the UI thread by
    ///   [`Self::spawn_consistency_check_probing`], so a probed distro's
    ///   rows appear once the check lands rather than in the placeholders.
    ///   Without this, a user with Ubuntu visibly running but no WSL repo
    ///   open yet saw every WSL row claim "no WSL distro running"
    ///   (live-reported on the first production bundle). Only THIS
    ///   explicit-open path gets the union: opening the page is a
    ///   deliberate "show me my environment" action, and checking a
    ///   running distro boots nothing — while the passive
    ///   launch/open_review checks keep their conservative known-repo
    ///   gating so background machinery never provisions into a distro the
    ///   user hasn't connected to dv at all;
    /// - the ACTIVE workspace's own WSL distro: without a running host
    ///   (e.g. a dev build with no sidecar, where Stage-A
    ///   `wsl.exe`-per-command routing carries everything),
    ///   `has_running_host` is false even while a WSL repo is open and its
    ///   git traffic is actively flowing. Opening the page is an explicit
    ///   user action about that exact environment, so the active repo's
    ///   distro is live-by-implication here for the same reason
    ///   `Self::open_review`'s own per-distro check is (the one
    ///   non-`has_running_host` trigger `dv_core::provision`'s boot-storm
    ///   contract blesses).
    fn open_onboarding_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let first_run = SetupState::load().is_first_run();
        let mut distros_allowed = self.live_wsl_distros();
        if let Some(ws) = &self.active
            && let RepoLocation::Wsl { distro, .. } = ws.read(cx).location()
            && !distros_allowed.contains(distro)
        {
            distros_allowed.push(distro.clone());
        }
        let mut page =
            OnboardingPage::placeholder(first_run, Self::onboarding_titles(&distros_allowed));
        // Carry forward any install still running from a PREVIOUS instance
        // of this page (S8e review, P3) — see `Self::onboarding_installing`'s
        // doc comment for why a fresh page can't discover this on its own.
        page.seed_installing(&self.onboarding_installing);
        self.onboarding = Some(page);
        // A deliberate, explicit reopen re-arms the auto-surface latch for
        // whatever drift this fresh look at the page turns up next (S8e
        // review, P2) — only a later CLOSE re-latches it.
        self.drift_page_dismissed = false;
        window.focus(&self.focus_handle, cx);
        self.spawn_consistency_check_probing(distros_allowed, cx);
        cx.notify();
    }

    /// Union `distros_allowed` with every distro that's actually running
    /// right now (per [`dv_core::remote::manager::running_distros`]'s
    /// non-booting probe), then dispatch the real consistency check. The
    /// probe is a blocking `wsl.exe` spawn, so it runs on the background
    /// executor — the onboarding page's placeholder rows have already
    /// painted by the time it fires (its "first paint never waits on a WSL
    /// round trip" contract), and any newly discovered distro's rows land
    /// through the same suffixed-row merge that displaces the generic
    /// "no WSL distro running" placeholders (`OnboardingPage`'s
    /// `drop_generic_placeholder`). Only the explicit page-open path uses
    /// this: the check auto-provisions dv's sidecars into every distro it's
    /// allowed to touch, which is what a user deliberately looking at their
    /// setup status wants — but not something passive launch machinery
    /// should do to a distro that's merely running (see
    /// [`Self::open_onboarding_page`]'s doc).
    fn spawn_consistency_check_probing(
        &mut self,
        distros_allowed: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let unioned = cx
                .background_executor()
                .spawn(async move {
                    let mut distros = distros_allowed;
                    for distro in dv_core::remote::manager::running_distros() {
                        if !distros.iter().any(|d| d.eq_ignore_ascii_case(&distro)) {
                            distros.push(distro);
                        }
                    }
                    distros
                })
                .await;
            this.update(cx, |this, cx| this.spawn_consistency_check(unioned, cx))
                .ok();
        })
        .detach();
    }

    fn close_onboarding_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = self.onboarding.take() {
            // Stamp the marker on CLOSE, not on open or on the check
            // completing — the page must stay closable/skippable mid-check
            // (never-fail-hard contract), and "seen it, dismissed it" is
            // the honest definition of "first run is over" regardless of
            // whether every row ever resolved.
            if page.first_run {
                SetupState::mark_complete(env!("CARGO_PKG_VERSION"));
            }
            // Latch the dismissal for the rest of the session (S8e review,
            // P2) — but ONLY if this page actually showed something needing
            // a human: the user has now explicitly seen and closed that
            // drift report, so the next passive check finding the SAME
            // drift (e.g. a declined vtsls consent, an unauthenticated
            // `gh`) stays silent instead of re-popping the page on every
            // subsequent launch/`open_review`. A page that closed with
            // nothing but `Ok`/`Skipped`/`Checking` rows (e.g. the
            // first-run welcome page with no WSL distro live) never showed
            // any drift to dismiss, so it must NOT suppress a LATER,
            // genuinely different drift from auto-surfacing. Cleared only
            // by a fresh manual reopen (`Self::open_onboarding_page`).
            //
            // Also persist the fingerprint of exactly what was dismissed
            // (phase-8 capstone review, P3): `drift_page_dismissed` alone
            // only lasts the session, so a permanent-by-choice state (no
            // `gh`, a declined vtsls consent) re-popped this same modal on
            // every later launch — `Self::apply_consistency_report`
            // compares each fresh report's own fingerprint against this to
            // stay silent on a repeat of the exact same condition, while
            // still surfacing anything genuinely different.
            if let Some(fingerprint) = page.needs_human_fingerprint() {
                self.drift_page_dismissed = true;
                SetupState::mark_drift_dismissed(fingerprint);
            }
            match &self.active {
                Some(ws) => window.focus(&ws.focus_handle(cx), cx),
                None => window.focus(&self.focus_handle, cx),
            }
            cx.notify();
        }
    }

    fn on_open_onboarding(
        &mut self,
        _: &OpenOnboarding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.onboarding.is_some()
            || self.settings_panel.is_some()
            || self.theme_picker.is_some()
            || self.command_palette.is_some()
        {
            return;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|ws| ws.read(cx).pr_picker_open())
        {
            return;
        }
        self.close_filter_popover(cx);
        self.open_onboarding_page(window, cx);
    }

    fn on_onboarding_close(
        &mut self,
        _: &OnboardingClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_onboarding_page(window, cx);
    }

    /// Passive-trigger front door for [`Self::spawn_consistency_check`]
    /// (phase-8 capstone review, P3): `Self::new`'s launch-time check and
    /// `Self::open_review`'s per-repo-open check both go through here
    /// instead of calling `spawn_consistency_check` directly. Skips the
    /// whole dispatch — `gh` included — when EVERY key it would touch (`gh`
    /// plus every distro in `distros_allowed`) was already checked within
    /// `CONSISTENCY_COOLDOWN`; otherwise runs the full, unfiltered check
    /// exactly as `distros_allowed` was passed in (a partial re-check of
    /// only the stale distros would make an empty `distros_allowed` look
    /// like "no WSL distro running" to `Self::pad_no_distro_rows`, when
    /// really every distro is just still fresh — simpler and safer to
    /// re-check everything together than to thread that distinction
    /// through). A manual page (re)open or the post-install reverify must
    /// always see a fresh result, so both call `spawn_consistency_check`
    /// directly rather than through here.
    fn spawn_consistency_check_throttled(
        &mut self,
        distros_allowed: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let now = std::time::Instant::now();
        let is_fresh = |key: &str| {
            self.consistency_checked_at
                .get(key)
                .is_some_and(|at| now.duration_since(*at) < CONSISTENCY_COOLDOWN)
        };
        let all_fresh = is_fresh(GH_COOLDOWN_KEY) && distros_allowed.iter().all(|d| is_fresh(d));
        if all_fresh {
            return;
        }
        self.spawn_consistency_check(distros_allowed, cx);
    }

    /// Dispatch `dv_core::provision::consistency_check(&distros_allowed)`
    /// off the UI thread (the module's own doc: it can block for seconds —
    /// gh 10s, node detect 20s, an install 180s — so it must NEVER run
    /// inline). `distros_allowed` is the caller's responsibility to have
    /// already gated on `Self::live_wsl_distros` (a passive/launch-time
    /// walk) or on "opening this WSL repo right now" (`Self::open_review`) —
    /// this fn does no gating of its own, matching `consistency_check`'s own
    /// module-doc contract. Stamps [`Self::consistency_checked_at`] for `gh`
    /// and every requested distro at DISPATCH time (not completion) so two
    /// calls issued back-to-back synchronously — e.g. a seeded launch's
    /// `Self::open_review` immediately followed by `Self::new`'s own
    /// unconditional check — see each other's stamp through
    /// [`Self::spawn_consistency_check_throttled`] even before either
    /// finishes.
    fn spawn_consistency_check(&mut self, distros_allowed: Vec<String>, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        self.consistency_checked_at
            .insert(GH_COOLDOWN_KEY.to_string(), now);
        for distro in &distros_allowed {
            self.consistency_checked_at.insert(distro.clone(), now);
        }
        self.onboarding_check_gen += 1;
        let check_gen = self.onboarding_check_gen;
        // Captured before the move below: `consistency_check(&[])` only
        // ever checks `gh` (see its module doc), so an empty allow-list
        // needs the synthesized rows `Self::pad_no_distro_rows` adds below.
        let no_distros_allowed = distros_allowed.is_empty();
        cx.spawn(async move |this, cx| {
            let mut report = cx
                .background_executor()
                .spawn(async move { consistency_check(&distros_allowed) })
                .await;
            if no_distros_allowed {
                Self::pad_no_distro_rows(&mut report);
            }
            this.update(cx, |this, cx| {
                this.apply_consistency_report(check_gen, report, cx)
            })
            .ok();
        })
        .detach();
    }

    /// With no WSL distro currently running, `consistency_check(&[])` only
    /// checks `gh` (it never boots a distro to check the other three — see
    /// the module's own boot-storm-contract doc). Synthesize a generic
    /// `Skipped` row for `dv-host`/`dv-cli`/`node`+`vtsls` so the page still
    /// shows all four components the onboarding spine tracks, matching the
    /// unsuffixed placeholders `Self::onboarding_titles` already produces
    /// for this same case, instead of silently dropping to a single `gh`
    /// row (S8e review, P2 — the acceptance case this fixes is exactly
    /// "delete setup.json, launch with no WSL distro running").
    fn pad_no_distro_rows(report: &mut ConsistencyReport) {
        for id in [
            ComponentId::DvHost,
            ComponentId::DvCli,
            ComponentId::NodeVtsls,
        ] {
            report.components.push(dv_core::provision::ComponentReport {
                id,
                title: id.title().to_string(),
                state: dv_core::provision::ComponentState::Skipped {
                    reason: "no WSL distro running".to_string(),
                },
            });
        }
    }

    /// Apply a completed [`ConsistencyReport`]. `check_gen` against
    /// `onboarding_check_gen` (see that field's doc comment) tells whether
    /// this report was SUPERSEDED by a newer check spawned before it
    /// completed — which happens on every first-run WSL-seeded launch
    /// (`Self::new`'s placeholder check vs. `Self::open_review`'s per-distro
    /// one) and can happen any time a second WSL repo is opened mid-check.
    ///
    /// A superseded report with the page already open is still applied — via
    /// [`OnboardingPage::apply_report_if_unanswered`], an idempotent "first
    /// answer wins" merge that only fills in rows still at
    /// [`RowState::Checking`] — rather than dropped outright: this used to
    /// be a single all-or-nothing guard that discarded the ENTIRE stale
    /// report, permanently orphaning every row only that check would ever
    /// have answered (e.g. the `gh` row and the generic no-distro
    /// placeholders) at "Checking" forever, since the superseding check
    /// covers a different, non-overlapping set of titles (S8e review, P2).
    ///
    /// A superseded report with NO page open is now ALSO given the same
    /// auto-surface consideration as a non-superseded one below, rather than
    /// dropped outright (S8e review, P2 — this was the single biggest gap:
    /// on every non-first-run WSL-seeded launch, `Self::open_review`'s
    /// per-distro check is spawned before `Self::new`'s own empty-distro
    /// launch check, so it is ALWAYS superseded on arrival; if the launch
    /// check finds no drift, the page never opens, and the seeded distro's
    /// real answers — a `NeedsConsent`/`Failed` row the launch check never
    /// even covered — used to be silently discarded here). Its answers are
    /// still real data; the only question left is whether it's still
    /// safe/useful to surface them.
    ///
    /// When the page is open and this IS the current check, it's simply the
    /// next real answer ([`OnboardingPage::apply_report`]). When the page is
    /// closed, dv's own bits have already silently repaired themselves as a
    /// side effect of the check that produced this report
    /// (`check_dv_host`/`check_dv_cli` delegate straight to the install
    /// functions) — this only opens the page if `report.drift` is still true
    /// afterward, i.e. something is left that genuinely needs a human (a
    /// `node`/`vtsls` consent row, or a real `Failed`/`Missing`) — AND only
    /// when the auto-surface itself is safe to do: never under
    /// `--automation` (an unattended script has no one to dismiss a modal
    /// that pops mid-run — S8e review, P2), never after the user already
    /// dismissed one this session (`drift_page_dismissed` — S8e review, P2;
    /// otherwise every subsequent launch/`open_review` check that still
    /// finds the same drift re-pops the page the user just closed), and
    /// never while another shell overlay (settings/theme picker/PR picker)
    /// is up (S8e review, P2 — mirrors `Self::on_open_onboarding`'s own
    /// guards; without this, a page popping mid-PR-picker-search violated
    /// `render`'s "all three are mutually exclusive" invariant and its own
    /// focus-pull yanked focus out of whatever the user was typing into).
    fn apply_consistency_report(
        &mut self,
        check_gen: u64,
        report: ConsistencyReport,
        cx: &mut Context<Self>,
    ) {
        let stale = check_gen != self.onboarding_check_gen;
        match &mut self.onboarding {
            Some(page) if stale => page.apply_report_if_unanswered(report),
            Some(page) => page.apply_report(report),
            None => {
                let overlay_blocking = self.settings_panel.is_some()
                    || self.theme_picker.is_some()
                    || self.command_palette.is_some()
                    || self
                        .active
                        .as_ref()
                        .is_some_and(|ws| ws.read(cx).pr_picker_open());
                // Persisted counterpart to `drift_page_dismissed` (S8e
                // review, P2 — session-only): a report whose needs-human
                // subset fingerprints matches ANY shape the user has
                // previously explicitly dismissed (`Self::
                // close_onboarding_page`) is a REPEAT of a permanent-by-
                // choice condition — no `gh`, a declined vtsls consent —
                // re-surfacing on this brand-new session, not a fresh
                // drift. Membership in a small set, not equality against a
                // single remembered slot (capstone integration review,
                // P3): a multi-distro machine's drift can take more than
                // one simultaneously-valid dismissed shape (e.g. Ubuntu
                // and Debian both missing vtsls, dismissed one repo-open
                // at a time), and a single slot made every launch whose
                // shape didn't happen to match the LAST one dismissed
                // re-pop the modal, clobbering the other shape's
                // dismissal on close. `SetupState::load()` here is a plain
                // JSON read, the same per-check cost every other
                // `SetupState::load()` call site in this file already pays
                // (phase-8 capstone review, P3).
                let already_dismissed_persistently = report
                    .needs_human_fingerprint()
                    .is_some_and(|fp| SetupState::load().is_drift_dismissed(&fp));
                if report.drift
                    && !self.automation
                    && !self.drift_page_dismissed
                    && !already_dismissed_persistently
                    && !overlay_blocking
                {
                    // Mirror `Self::on_open_onboarding`'s own guard: the
                    // sidebar filter popover's full-window backdrop renders
                    // AFTER `render_onboarding` in `render()`, so a still-open
                    // popover paints on top of the just-surfaced modal and
                    // swallows its first click (S8e review, P3) — close it
                    // rather than adding it to `overlay_blocking` above, same
                    // as `on_open_onboarding` does.
                    self.close_filter_popover(cx);
                    let mut page = OnboardingPage::placeholder(false, Vec::new());
                    page.seed_installing(&self.onboarding_installing);
                    page.apply_report(report);
                    self.onboarding = Some(page);
                }
            }
        }
        cx.notify();
    }

    /// The onboarding page's one mutating action: install `@vtsls/
    /// language-server` on the row at `idx`, after the user's explicit
    /// consent click (`dv_core::provision`'s module doc — this is the ONLY
    /// caller of `install_vtsls` anywhere in the app). Keyed by row INDEX
    /// at click time, not [`ComponentId`] (S8e review, P2): two different
    /// live distros can both land on a `NodeVtsls` consent row in the same
    /// render (`render_onboarding_row`'s own doc already calls this out for
    /// the button's element id), and resolving by id alone would always hit
    /// the FIRST matching row — installing into the wrong distro and
    /// flipping every same-id row's state together. Marks only that one row
    /// `Installing` immediately (and its title in-flight, so a concurrent
    /// consistency check can't clobber it — `OnboardingPage::begin_install`'s
    /// doc comment), then re-runs the normal per-distro check once the
    /// install completes (success or failure) so the row's final state comes
    /// from the same real detection path a plain reopen would use, rather
    /// than the install call's own bare `Result`.
    ///
    /// `title` is captured up front and used for BOTH `end_install` and the
    /// failure arm's row lookup instead of `idx` (S8e review, P3): the up-
    /// to-180s install can outlive the page it started on (closed and
    /// reopened with a different, shorter or differently-ordered row set —
    /// e.g. a distro's connection dropping mid-install), so resolving by the
    /// stale index on completion could stamp `Failed` onto a completely
    /// different row, or silently miss it if the reopened page is shorter.
    /// Title is stable (it's the same merge key every other apply path uses)
    /// even across a close/reopen.
    ///
    /// Guards on `self.onboarding_installing` (S8e review, P3), not just the
    /// page-local `installing` set: without this, closing the page mid-
    /// install and reopening it builds a FRESH `OnboardingPage` (an empty
    /// `installing` set, per `Self::open_onboarding_page`) whose own check
    /// re-detects the half-written `npm install` as `NeedsConsent`, and a
    /// second click here would start a SECOND concurrent `npm install -g
    /// @vtsls/language-server` into the same distro (`install_vtsls` itself
    /// takes no per-distro lock). Returns `Err` rather than silently
    /// no-oping on every rejected precondition so both the button's
    /// (discarded) click result and the automation-reachable
    /// `Self::automation_onboarding_consent` wrapper can tell a script
    /// exactly why a consent click didn't do anything.
    fn on_onboarding_consent_install(
        &mut self,
        idx: usize,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let Some(page) = &self.onboarding else {
            anyhow::bail!("onboarding page is not open");
        };
        let Some(row) = page.rows.get(idx) else {
            anyhow::bail!("no onboarding row at index {idx}");
        };
        let RowState::Consent(action, _) = &row.state else {
            anyhow::bail!("row {idx} ({}) is not awaiting consent", row.title);
        };
        let title = row.title.clone();
        if !self.onboarding_installing.insert(title.clone()) {
            anyhow::bail!("{title} already has an install in flight");
        }
        // One background job + the distro list the success-arm reverify
        // should re-check (empty for host-side actions like the PATH
        // registration — `spawn_consistency_check(vec![])` still re-checks
        // every unconditional host-side row, which is exactly the set such
        // an action can have changed).
        type ConsentJob = Box<dyn FnOnce() -> anyhow::Result<()> + Send + 'static>;
        let (job, reverify_distros): (ConsentJob, Vec<String>) = match action.clone() {
            ConsentAction::InstallVtsls { distro, node } => {
                let reverify = vec![distro.clone()];
                (
                    Box::new(move || {
                        dv_core::provision::install_vtsls(&distro, &node).map_err(Into::into)
                    }),
                    reverify,
                )
            }
            ConsentAction::AddDvToPath { dir } => (
                Box::new(move || dv_core::provision::add_dv_to_path(&dir)),
                Vec::new(),
            ),
        };
        if let Some(page) = &mut self.onboarding {
            page.begin_install(idx);
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { job() }).await;
            this.update(cx, |this, cx| {
                // Clear both the page-local and shell-persisted in-flight
                // markers BEFORE branching on the result (S8e review, P2/
                // P3): the success arm's own reverify needs its report free
                // to land on this title, and the failure arm's direct write
                // below must not be treated as a concurrent clobber of
                // itself.
                if let Some(page) = &mut this.onboarding {
                    page.end_install(&title);
                }
                this.onboarding_installing.remove(&title);
                match result {
                    Ok(()) => {
                        // Span `wait_ready` across the reverify, not just
                        // the install itself (phase-8 capstone integration
                        // review, P3): `end_install` above already emptied
                        // `installing`, and `running` was already `false`
                        // from way back when the ORIGINAL check first
                        // populated this row as `Consent` — with neither
                        // flag flipped here, `AppShell::automation_settled`
                        // read "settled" immediately, before this reverify's
                        // own report had any chance to flip the row off
                        // `Installing`. Only when the page is still open
                        // (it may have been closed during the up-to-180s
                        // install) — mirrors `OnboardingPage::placeholder`'s
                        // own `running: true` for a fresh check, and is
                        // cleared the same way: `apply_report`'s trailing
                        // `self.running = false` once this reverify's report
                        // actually lands (or a still-later check's, if this
                        // one gets superseded first).
                        if let Some(page) = &mut this.onboarding {
                            page.running = true;
                        }
                        this.spawn_consistency_check(reverify_distros, cx)
                    }
                    Err(err) => {
                        if let Some(page) = &mut this.onboarding
                            && let Some(row) = page.rows.iter_mut().find(|r| r.title == title)
                        {
                            row.state = RowState::Failed(format!("{err:#}"));
                        }
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
        Ok(())
    }

    /// `{"cmd":"onboarding_consent","row":N}`: the automation-reachable
    /// stand-in for clicking a consent row's "Install" button (S8e review,
    /// P3) — a coordinate `{"cmd":"click"}` on the button isn't
    /// deterministic since its position depends on how many rows precede it,
    /// which varies with live distros and check timing.
    #[cfg(feature = "automation")]
    pub(crate) fn automation_onboarding_consent(
        &mut self,
        row: usize,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        self.on_onboarding_consent_install(row, cx)
    }

    /// "Default view" — applies to the workspace open right now too (not
    /// just future ones); the setting itself only picks what a *freshly
    /// opened* review starts in.
    fn set_view_mode_default(&mut self, mode: ViewModeSetting, cx: &mut Context<Self>) {
        self.settings.view_mode_default = mode;
        self.settings.save();
        if let Some(ws) = &self.active {
            ws.update(cx, |ws, cx| ws.set_view_mode_setting(mode, cx));
        }
        cx.notify();
    }

    /// "Context lines" stepper.
    fn set_context_lines(&mut self, n: u32, cx: &mut Context<Self>) {
        let n = n.clamp(CONTEXT_LINES_MIN, CONTEXT_LINES_MAX);
        self.settings.context_lines = n;
        self.settings.save();
        if let Some(ws) = &self.active {
            ws.update(cx, |ws, cx| ws.set_context_lines(n, true, cx));
        }
        // Same staleness problem as `Self::apply_resolved_theme` (review
        // finding), but sharper here: an unstamped parked workspace's
        // `context_lines` field itself is wrong, not just its baked colors,
        // so even a lazily-recomputed unselected file would use the old
        // value until this runs. Same `eager: false` host-reachability gate
        // on the eager recompute as the theme case — see
        // `Workspace::invalidate_diff_cache`.
        for ws in self.workspace_cache.cached_entities() {
            ws.update(cx, |ws, cx| ws.set_context_lines(n, false, cx));
        }
        cx.notify();
    }

    /// "Mono font" free-text field. Not validated against installed fonts —
    /// an unresolvable family just falls back to a proportional sans
    /// (observed), the same silent fallback an unknown CSS font-family
    /// gets.
    fn set_mono_font(&mut self, family: String, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.mono_font = family;
        self.settings.save();
        themes::set_mono_font(&self.settings.mono_font, window, cx);
        // Keep the panel's own text field in sync when the change didn't
        // originate from it — e.g. automation's `set_setting mono_font`
        // while the panel is open (review finding: the input went stale
        // until the panel was closed and reopened). When the change *did*
        // come from the input's own `InputEvent::Change` (see
        // `on_open_settings`), the box's value already equals the new
        // setting, so the `!=` below skips it there — important, since
        // `InputState::set_value` resets the caret/selection and would
        // otherwise fight the user mid-keystroke.
        if let Some(panel) = &self.settings_panel {
            let input = panel.mono_font_input.clone();
            if input.read(cx).value() != self.settings.mono_font {
                let value = self.settings.mono_font.clone();
                input.update(cx, |input, cx| input.set_value(value, window, cx));
            }
        }
        cx.notify();
    }

    /// "Font size" stepper.
    fn set_mono_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        let size = size.clamp(MONO_FONT_SIZE_MIN, MONO_FONT_SIZE_MAX);
        self.settings.mono_font_size = size;
        self.settings.save();
        if let Some(ws) = &self.active {
            ws.update(cx, |ws, cx| ws.set_font_size(size, cx));
        }
        // Same reasoning as `Self::set_context_lines` — a parked workspace's
        // `font_size` field is frozen otherwise, so reactivating it later
        // paints rows measured for the old size (review finding).
        for ws in self.workspace_cache.cached_entities() {
            ws.update(cx, |ws, cx| ws.set_font_size(size, cx));
        }
        cx.notify();
    }

    /// Sidebar width — `set_setting`'s entry point (the drag handle itself,
    /// [`Self::render_sidebar_resize_handle`], writes `self.settings.sidebar_width`
    /// directly on every drag-move frame and only calls `Settings::save`
    /// on release, so it doesn't route through here — this is for the
    /// discrete, already-final values `set_setting`/a hand-edited settings
    /// panel control would supply).
    #[cfg(feature = "automation")]
    fn set_sidebar_width(&mut self, width: f32, cx: &mut Context<Self>) {
        self.settings.sidebar_width = width.clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX);
        self.settings.save();
        cx.notify();
    }

    fn on_toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let visible = !self.settings.sidebar_visible;
        self.set_sidebar_visible(visible, window, cx);
    }

    /// Sidebar hide/show (ctrl-b, R2). Hidden means
    /// *omitted from the tree* — not width-0 — so `uniform_list` isn't
    /// asked to lay out into a zero box and the drag handle can't resurrect
    /// a "hidden" sidebar by widening it. Also closes the filter popover:
    /// it's an overlay anchored to a sidebar button that no longer exists.
    fn set_sidebar_visible(&mut self, visible: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.sidebar_visible == visible {
            return;
        }
        self.settings.sidebar_visible = visible;
        self.settings.save();
        if !visible {
            self.close_filter_popover(cx);
            // The quick-open input is the sidebar's only focus sink, and
            // gpui does NOT blur a focus whose element unmounts — key
            // dispatch falls back to the window ROOT node, above every
            // context-scoped binding in this app, so hiding the sidebar
            // while the input held focus left the ENTIRE keyboard dead
            // (ctrl-b itself included) until a mouse click (R2 review,
            // P2). Land focus where closing any shell overlay does.
            if self.quick_open.focus_handle(cx).is_focused(window) {
                match &self.active {
                    Some(ws) => {
                        let handle = ws.focus_handle(cx);
                        window.focus(&handle, cx);
                    }
                    None => window.focus(&self.focus_handle, cx),
                }
            }
        }
        cx.notify();
    }

    /// Review-summary panel width — pushes the new width down into the
    /// active workspace (which owns the live render-time value; see
    /// `Workspace::summary_width`) and persists. Mirrors `set_sidebar_width`
    /// above, but for the workspace-owned panel.
    #[cfg(feature = "automation")]
    fn set_summary_width(&mut self, width: f32, cx: &mut Context<Self>) {
        let width = width.clamp(SUMMARY_WIDTH_MIN, SUMMARY_WIDTH_MAX);
        self.settings.summary_width = width;
        self.settings.save();
        if let Some(ws) = &self.active {
            ws.update(cx, |ws, cx| ws.set_summary_width_external(width, cx));
        }
        cx.notify();
    }

    /// One half of the follow-OS light/dark pair. Re-resolves immediately
    /// if follow-OS is already on (picking a new light theme while the OS
    /// is currently in light mode must apply right away, not wait for the
    /// next appearance-changed event).
    ///
    /// Validates `name` against the theme registry, same error style as
    /// `automation_set_setting`'s `"theme"` case — the settings panel's own
    /// segmented row only ever passes a name from `themes::names()`, so this
    /// mainly guards `set_setting`'s JSON input and hand-edited
    /// settings.json values routed through here.
    fn set_light_theme(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let name = themes::names()
            .find(|n| *n == name)
            .ok_or_else(|| anyhow::anyhow!("unknown theme: {name}"))?;
        self.settings.light_theme = name.to_string();
        self.settings.save();
        self.resolve_follow_os(window, cx);
        cx.notify();
        Ok(())
    }

    fn set_dark_theme(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let name = themes::names()
            .find(|n| *n == name)
            .ok_or_else(|| anyhow::anyhow!("unknown theme: {name}"))?;
        self.settings.dark_theme = name.to_string();
        self.settings.save();
        self.resolve_follow_os(window, cx);
        cx.notify();
        Ok(())
    }

    /// "Follow OS appearance" toggle. Turning on re-resolves immediately
    /// from the live OS appearance (`resolve_follow_os`). Turning back off
    /// must restore the persisted explicit `theme` right away too — without
    /// this, the OS-resolved theme stays painted (and highlighted as active
    /// in the panel) even though `settings.theme` now says otherwise, until
    /// the next restart silently repaints it. This matches the `theme`
    /// field's doc: "turning follow-OS back off restores whatever was
    /// picked last".
    fn set_follow_os_appearance(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.follow_os_appearance = on;
        self.settings.save();
        if on {
            self.resolve_follow_os(window, cx);
        } else {
            let name = self.settings.theme.clone();
            self.apply_resolved_theme(&name, window, cx);
        }
        cx.notify();
    }

    /// The sidebar's actual rendered row list, in order: `self.index`'s
    /// entries filtered by `settings.sidebar_filters`
    /// ([`entry_passes_filters`]), then — when `sidebar_grouping` isn't
    /// `SidebarGrouping::None` — partitioned into named runs with a
    /// [`SidebarItem::Header`] above each (deliverables 3/4). Groups appear
    /// in first-encounter order; entries within a group, and ungrouped
    /// entries, keep `self.index.entries()`'s own relative order —
    /// recency-sorted at load, then stable for the whole session
    /// (`apply_hydration` is order-preserving; only genuinely-new reviews
    /// insert at the front). Both [`Render::render`]'s
    /// `uniform_list` and [`Self::automation_state`]'s `"sidebar"` field are
    /// built from this one function, so what a script asserts is exactly
    /// what's on screen.
    fn visible_sidebar_items(&self) -> Vec<SidebarItem> {
        let filters = &self.settings.sidebar_filters;
        let entries = self.index.entries();
        let visible: Vec<usize> = (0..entries.len())
            .filter(|&i| entry_passes_filters(&entries[i], filters))
            .collect();

        if self.settings.sidebar_grouping == SidebarGrouping::None {
            return visible.into_iter().map(SidebarItem::Review).collect();
        }

        // First-encounter order for headers, preserving each group's own
        // internal `entries()` order — a plain `HashMap<String, Vec<_>>`
        // has no ordering of its own, hence the separate `order` list.
        // `key` (the dedup/grouping identity) and `label` (the header
        // text) are tracked separately: a key sometimes needs to
        // disambiguate information the display label deliberately omits
        // (case-folding for `Repo`, the GitHub host for `Pr` — see
        // `repo_group_key`/`pr_group_key`), so two entries can share one
        // group while the header still renders the first-encountered
        // entry's original label text.
        let mut order: Vec<String> = Vec::new();
        let mut labels: HashMap<String, String> = HashMap::new();
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for i in visible {
            let entry = &entries[i];
            let (key, label) = match self.settings.sidebar_grouping {
                SidebarGrouping::None => unreachable!("handled above"),
                SidebarGrouping::Repo => (
                    repo_group_key(&entry.location),
                    dv_core::repo_label(&entry.location, None),
                ),
                SidebarGrouping::Status => {
                    let label = status_group_label(&entry.state).to_string();
                    (label.clone(), label)
                }
                // All rounds of the same PR share one header; a
                // local-only review falls back to the same per-repo
                // header `Repo` grouping would give it —
                // `dv_core::repo_label` already produces exactly that
                // when `remote` is `None`.
                SidebarGrouping::Pr => (
                    pr_group_key(&entry.location, entry.remote.as_ref()),
                    dv_core::repo_label(&entry.location, entry.remote.as_ref()),
                ),
            };
            if !groups.contains_key(&key) {
                order.push(key.clone());
                labels.insert(key.clone(), label);
            }
            groups.entry(key).or_default().push(i);
        }

        let mut items = Vec::with_capacity(entries.len() + order.len());
        for key in order {
            let label = labels.remove(&key).unwrap_or_else(|| key.clone());
            let idxs = groups.remove(&key);
            items.push(SidebarItem::Header {
                key: SharedString::from(key),
                label: SharedString::from(label),
            });
            if let Some(idxs) = idxs {
                items.extend(idxs.into_iter().map(SidebarItem::Review));
            }
        }
        items
    }

    /// Sidebar grouping control (docs/phase-6-review-navigator.md
    /// deliverable 3, [`Self::render_sidebar_grouping_control`]) —
    /// mouse-only, so no `window.focus` re-home is needed (cross-cutting
    /// risk E).
    fn set_sidebar_grouping(&mut self, grouping: SidebarGrouping, cx: &mut Context<Self>) {
        self.settings.sidebar_grouping = grouping;
        self.settings.save();
        cx.notify();
    }

    /// Sidebar filter popover (deliverable 4,
    /// [`Self::render_sidebar_filter_popover`]) — replaces the whole
    /// [`SidebarFilters`] at once; each row click computes its own toggled
    /// copy first (see [`Self::render_filter_row`]), so this stays a single
    /// dumb setter shared by every row and by `automation_set_setting`.
    fn set_sidebar_filters(&mut self, filters: SidebarFilters, cx: &mut Context<Self>) {
        self.settings.sidebar_filters = filters;
        self.settings.save();
        cx.notify();
    }

    /// One two-line review card (docs/phase-6-review-navigator.md
    /// deliverable 2's user sketch, densified by R1e):
    /// an 8px status dot (review/PR state
    /// color — [`pr_state_pill`]'s color for a PR-linked review, `warning`
    /// when the repo went unavailable, `primary` for a plain local review,
    /// matching R1c's "local" title-bar Tag) + `repo_label` + `relative_age`
    /// on line 1, both in `text_secondary` (dv's new second dim-text tier,
    /// R1a); line 2 is the review's derived title, demoted to a
    /// "nested/secondary line" treatment — 11px, `text_secondary`, no longer
    /// bold — left, + a status cluster right (health warning, PR glyphs,
    /// open-comment pill/submitted check) unaffected by that demotion since
    /// those are [`state_pill`] Tags with their own tinted colors regardless
    /// of the row's text color. Row chrome is the shared rounded/hover recipe
    /// (`mx_1 px_2 py_1 rounded_md`, active/hover on `muted.background`
    /// rather than dv's old `accent` selection color). Colors are
    /// snapshotted as owned locals up front — holding `&Theme` across the
    /// card's own `cx.listener` setup below is a borrow-check error
    /// (CLAUDE.md's gpui gotcha; same pattern as `render_summary`/
    /// `render_split_row`).
    fn render_review_card(
        &self,
        entry: &dv_core::IndexEntry,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        // Row hover/active fills use DvTheme's surface_active (NOT
        // muted.background directly — Claude Light defines that identical to
        // sidebar.background, which made selection invisible; R1e review).
        let muted_fg = theme.muted_foreground;
        let primary = theme.primary;
        let success = theme.success;
        let warning = theme.warning;
        let danger = theme.danger;
        let dv = themes::dv_theme(cx);
        let accent_alt = dv.accent_alt;
        let text_secondary = dv.text_secondary;
        let surface_active = dv.surface_active;
        let foreground = theme.foreground;

        let review_id = entry.review_id.clone();
        // A separate clone from `review_id`/`menu_review_id` above — those
        // two are each `move`d whole into their own mouse-down closure, so
        // the age tooltip's `.id(...)` (built well after both closures are
        // constructed) needs its own copy rather than reusing an
        // already-moved binding.
        let review_id_for_age = entry.review_id.clone();
        let menu_review_id = entry.review_id.clone();
        let archived = entry.archived;
        let repo = dv_core::repo_label(&entry.location, entry.remote.as_ref());
        let age = relative_age(entry.updated_ms);
        let diffstat = entry.diffstat;
        // Merge-conflict indicator (docs/backlog.md "Merge-conflict
        // indicator in the review navigator"): `Some(n)` (n > 0) once
        // hydration has determined this review's diff source would
        // conflict — `None` for a clean review OR one hydration hasn't
        // reached a verdict on yet (old git, WSL hiccup, not-yet-hydrated).
        // Deliberately not folded into `unavailable`'s glyph slot below —
        // the two are independent axes (a repo can be unreachable with no
        // conflict info at all, or reachable with a real conflict).
        let conflict_count = entry
            .conflict
            .as_ref()
            .filter(|c| c.is_conflicted())
            .map(|c| c.files.len());
        let title = entry.title.clone();
        let open_comments = entry.open_comments;
        let submitted = matches!(entry.state, dv_core::ReviewState::Submitted { .. });
        let unavailable = entry.health == dv_core::EntryHealth::RepoUnavailable;
        let pr = entry.pr_status.clone();

        // Same state → color mapping the PR-status pill cluster below uses
        // for a PR-linked review ([`pr_state_pill`]); `warning` for a
        // repo-unavailable entry mirrors the existing health-warning glyph
        // on line 2, and `primary` for a plain local review matches R1c's
        // "local" Tag color — every review gets a dot, not just PR ones.
        let dot_color = if unavailable {
            warning
        } else if let Some(pr) = pr.as_ref() {
            pr_state_pill(pr.is_draft, pr.state, muted_fg, success, danger, accent_alt).1
        } else {
            primary
        };

        let card = v_flex()
            .id(SharedString::from(format!("review-card-{review_id}")))
            .w_full()
            .h(px(SIDEBAR_ROW_HEIGHT))
            .justify_center()
            // No mx here: uniform_list items are laid out as taffy ROOT
            // nodes, whose margins are computed but never applied (R1e code
            // review, P3) — the side inset comes from the list's own
            // px_2 instead.
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(selected, |el| el.bg(surface_active))
            // Archived rows (visible only with the Archived filter on)
            // read as parked, not live work.
            .when(archived, |el| el.opacity(0.55))
            .hover(|el| el.bg(surface_active.opacity(0.5)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.open_review_row(&review_id, window, cx);
                }),
            )
            // Stash which review the context menu is being opened over —
            // this fires alongside `ContextMenuExt`'s own right-click
            // handling (both observe the same mouse-down), so by the time
            // a menu item dispatches its unit action, `menu_review` names
            // this card. See `AppShell::menu_review`.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    this.menu_review = Some(menu_review_id.clone());
                    cx.notify();
                }),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    // Line 1 carries the row's identity — near-full
                    // foreground, the bright-label tier; only the
                    // trailing age stays dim (R1e visual review, P2: an
                    // all-text_secondary card flattened the hierarchy).
                    .text_color(foreground)
                    .child(
                        div()
                            .flex_none()
                            .w(px(8.))
                            .h(px(8.))
                            .rounded_full()
                            .bg(dot_color),
                    )
                    .child(div().flex_1().min_w(px(0.)).truncate().child(repo))
                    // Whole-review +/− totals (R2) —
                    // same `+N`/`−N` recipe as the title bar's diffstat
                    // (workspace.rs render_header), sitting left of the age
                    // so recency keeps the right edge. Absent (None) on a
                    // not-yet-hydrated entry — the card just omits it.
                    .children(diffstat.map(|ds| {
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .child(
                                div()
                                    .text_color(success)
                                    .child(format!("+{}", ds.additions)),
                            )
                            .child(
                                div()
                                    .text_color(danger)
                                    .child(format!("\u{2212}{}", ds.deletions)),
                            )
                    }))
                    .child({
                        let updated_ms = entry.updated_ms;
                        div()
                            .id(SharedString::from(format!(
                                "review-card-age-{review_id_for_age}"
                            )))
                            .flex_none()
                            .text_color(text_secondary)
                            .tooltip(move |window, cx| {
                                Tooltip::new(absolute_timestamp(updated_ms)).build(window, cx)
                            })
                            .child(age)
                    }),
            )
            .child(
                h_flex()
                    .w_full()
                    // 8px dot + gap_2 = line-1 text starts at 16px; indent
                    // the nested line to align under it (the pl(16px)
                    // nested-line treatment — R1e visual review, P2).
                    .pl(px(16.))
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(11.))
                            .text_color(text_secondary)
                            .truncate()
                            .child(title),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1p5()
                            .items_center()
                            .when(unavailable, |el| {
                                el.child(div().text_color(warning).child("\u{26a0}"))
                            })
                            .children(conflict_count.map(|n| {
                                let id = SharedString::from(format!(
                                    "review-card-conflict-{review_id_for_age}"
                                ));
                                div()
                                    .id(id)
                                    .text_color(warning)
                                    // Same warning triangle `unavailable`
                                    // uses just above (proven to render —
                                    // an exotic glyph risks the same
                                    // font-coverage tofu R1e's visual
                                    // review flagged for `state_pill`
                                    // icons); the tooltip is what tells
                                    // the two apart on hover.
                                    .child(format!("\u{26a0} {n}"))
                                    .tooltip(move |window, cx| {
                                        let label = if n == 1 {
                                            "1 file would conflict on merge".to_string()
                                        } else {
                                            format!("{n} files would conflict on merge")
                                        };
                                        Tooltip::new(label).build(window, cx)
                                    })
                            }))
                            .children(pr.map(|pr| {
                                render_pr_glyphs(
                                    pr.is_draft,
                                    pr.state,
                                    pr.decision,
                                    pr.checks,
                                    muted_fg,
                                    success,
                                    danger,
                                    warning,
                                    accent_alt,
                                )
                            }))
                            .child(if open_comments > 0 {
                                state_pill(primary, format!("{open_comments}")).into_any_element()
                            } else if submitted {
                                state_pill_icon(success, gpui_component::IconName::Check)
                                    .into_any_element()
                            } else {
                                div().into_any_element()
                            }),
                    ),
            );

        // Right-click menu (gpui-component `ContextMenuExt`): the items
        // dispatch unit actions handled on the shell root; the right-
        // mouse-down stash above tells those handlers which review this
        // is. `archived` is a render-time snapshot — fine, the card
        // re-renders on every index change. `action_context` is REQUIRED:
        // the menu steals focus when it opens, and without a context to
        // focus back to, dismissing it (escape, click-away) strands focus
        // on the unmounted menu node — the whole keyboard dies until a
        // rescue click (the same R2 ctrl-b P2 class). Same workspace-else-
        // shell target `close_theme_picker` restores to. Known narrow gap
        // (post-hoc review P3, accepted): the handle is a render-time
        // snapshot latched into the open menu, so a watcher-driven active-
        // workspace swap while a menu is up can leave dismiss focusing the
        // parked entity's handle; user-driven switches dismiss the menu
        // first, so only that async race hits it.
        use gpui_component::menu::ContextMenuExt as _;
        let menu_focus_target = match &self.active {
            Some(ws) => ws.focus_handle(cx),
            None => self.focus_handle.clone(),
        };
        card.context_menu(move |menu, _window, _cx| {
            menu.action_context(menu_focus_target.clone())
                .menu(
                    if archived { "Unarchive" } else { "Archive" },
                    Box::new(ToggleArchiveReview),
                )
                .separator()
                .menu("Delete review…", Box::new(DeleteReviewPrompt))
        })
    }

    /// A group header row, sitting above a run of cards when
    /// `sidebar_grouping` is non-`None` (docs/phase-6-review-navigator.md
    /// deliverable 3) — fixed to [`SIDEBAR_ROW_HEIGHT`], the same height as
    /// [`Self::render_review_card`] (see that constant's doc comment for
    /// why `uniform_list`'s scroll math requires it). Restyled by R1e to
    /// a dim directory-header treatment
    /// ("nested line pl(16px) 11px text_secondary") — 11px,
    /// `text_secondary` rather than the old bold `muted.foreground`, since
    /// dv's group headers play a "quiet section label" role.
    ///
    /// The gpui element id is built from `key`, never `label`: two distinct
    /// groups can share an identical display label (`Pr` grouping's label
    /// is host-stripped, so two different GitHub Enterprise hosts with the
    /// same owner/repo/pr collide on label text while their keys — the
    /// full slug — still differ). Building the id from `label` would give
    /// two `uniform_list` siblings the same stateful element id, a gpui
    /// duplicate-id hazard (confirmed P3 finding).
    fn render_sidebar_header(
        &self,
        key: SharedString,
        label: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let text_secondary = themes::dv_theme(cx).text_secondary;
        let id = SharedString::from(format!("sidebar-header-{key}"));
        div()
            .id(id)
            .w_full()
            .h(px(SIDEBAR_ROW_HEIGHT))
            .flex()
            .items_center()
            .px_2()
            .text_size(px(11.))
            .text_color(text_secondary)
            .truncate()
            .child(label)
    }

    /// Sidebar grouping segmented control (docs/phase-6-review-navigator.md
    /// deliverable 3): four small ghost buttons, mouse-only — no key
    /// binding captures anything here, so no `window.focus` re-home is
    /// needed the way the theme picker/settings panel overlays require
    /// (cross-cutting risk E).
    fn render_sidebar_grouping_control(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.settings.sidebar_grouping;
        let row = |id: &'static str,
                   label: &'static str,
                   value: SidebarGrouping,
                   cx: &mut Context<Self>| {
            Button::new(id)
                .ghost()
                .xsmall()
                .selected(current == value)
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_sidebar_grouping(value, cx);
                }))
        };
        h_flex()
            .gap_1()
            .child(row("sidebar-group-none", "Flat", SidebarGrouping::None, cx))
            .child(row("sidebar-group-repo", "Repo", SidebarGrouping::Repo, cx))
            .child(row(
                "sidebar-group-status",
                "Status",
                SidebarGrouping::Status,
                cx,
            ))
            .child(row("sidebar-group-pr", "PR", SidebarGrouping::Pr, cx))
    }

    /// Sidebar filter-popover toggle button (deliverable 4) — same
    /// mouse-only reasoning as the grouping control above.
    fn render_sidebar_filter_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("sidebar-filter-toggle")
            .ghost()
            .xsmall()
            .selected(self.filter_popover_open)
            .label("Filter")
            .on_click(cx.listener(|this, _, _, cx| {
                // Mirror `on_open_theme_picker`/`on_open_settings`'s own
                // mutual-exclusion guards: don't stack this popover's
                // full-window backdrop on top of any of the other three
                // overlays (review finding — the button is always
                // rendered, so it's reachable even while one is open). The
                // PR picker isn't modal like the other two, so it doesn't
                // occlude the sidebar and this button stays clickable while
                // it's up — without this check the filter popover's
                // `inset_0().occlude()` backdrop, mounted last, would paint
                // on top of the picker and swallow its first click.
                if this.settings_panel.is_some()
                    || this.theme_picker.is_some()
                    || this.command_palette.is_some()
                    || this
                        .active
                        .as_ref()
                        .is_some_and(|ws| ws.read(cx).pr_picker_open())
                {
                    return;
                }
                this.filter_popover_open = !this.filter_popover_open;
                cx.notify();
            }))
    }

    /// One filter-popover checkbox row (deliverable 4): a checkmark glyph
    /// (success color when on) + label. Clicking computes the toggled copy
    /// of the whole [`SidebarFilters`] and replaces it via
    /// [`Self::set_sidebar_filters`] — `get`/`set` come from
    /// [`PR_FILTER_ROWS`]/[`REVIEW_FILTER_ROWS`], so this one function
    /// renders all nine rows rather than one hand-written row per field.
    #[allow(clippy::too_many_arguments)]
    fn render_filter_row(
        &self,
        id: &'static str,
        label: &'static str,
        get: fn(&SidebarFilters) -> bool,
        set: fn(&mut SidebarFilters, bool),
        hover: Hsla,
        success: Hsla,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let on = get(&self.settings.sidebar_filters);
        h_flex()
            .id(id)
            .w_full()
            .gap_2()
            .items_center()
            .px_2()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .hover(move |el| el.bg(hover.opacity(0.5)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    let mut filters = this.settings.sidebar_filters.clone();
                    let toggled = !get(&filters);
                    set(&mut filters, toggled);
                    this.set_sidebar_filters(filters, cx);
                }),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(16.))
                    .text_color(success)
                    .child(if on { "\u{2713}" } else { "" }),
            )
            .child(div().flex_1().text_sm().child(label))
    }

    /// The sidebar's filter popover (deliverable 4), when open: a small
    /// panel anchored under the "Filter" toggle button, listing every PR-
    /// status and review-status checkbox row. A full-window, invisible
    /// backdrop closes it on any outside click (same swallow-the-click
    /// pattern as `render_settings_panel`'s backdrop); the popover's own
    /// content stops propagation so a click inside it doesn't also close
    /// it. Pure mouse throughout — no key binding, so (cross-cutting risk
    /// E) no `window.focus` re-home on open/close, unlike the theme
    /// picker/settings panel.
    ///
    /// Mutual exclusion with those two overlays is enforced on the *other*
    /// side rather than here: `render_sidebar_filter_button`'s `on_click`
    /// won't open this popover while either is up, and
    /// `on_open_theme_picker`/`on_open_settings` close this popover on
    /// entry — otherwise this backdrop, being the last child mounted (see
    /// the bottom of `Render for AppShell`), would paint/hit-test on top of
    /// the settings panel or theme picker and swallow their first click
    /// (review finding).
    fn render_sidebar_filter_popover(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.filter_popover_open {
            return None;
        }
        let theme = cx.theme();
        let border = theme.border;
        let popover = theme.popover;
        let popover_fg = theme.popover_foreground;
        let muted = theme.muted_foreground;
        let accent = theme.accent;
        let success = theme.success;

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.filter_popover_open = false;
                        cx.notify();
                    }),
                )
                .child(
                    div().absolute().top(px(88.)).left(px(12.)).child(
                        v_flex()
                            .id("sidebar-filter-popover")
                            .w(px(240.))
                            .max_h(px(420.))
                            .overflow_hidden()
                            .p_2()
                            .gap_1()
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
                                    .child("PR STATUS"),
                            )
                            .children(PR_FILTER_ROWS.iter().map(|(id, label, get, set)| {
                                self.render_filter_row(id, label, *get, *set, accent, success, cx)
                            }))
                            .child(
                                div()
                                    .px_2()
                                    .pt_2()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("REVIEW STATUS"),
                            )
                            .children(REVIEW_FILTER_ROWS.iter().map(|(id, label, get, set)| {
                                self.render_filter_row(id, label, *get, *set, accent, success, cx)
                            })),
                    ),
                ),
        )
    }

    /// The sidebar's inner-edge drag handle (Phase 4 deliverable 5): a 6px
    /// invisible-until-hover strip straddling the sidebar/main-content
    /// boundary, showing a slim accent line on hover and a col-resize
    /// cursor. Dragging computes the new width directly from the cursor's
    /// absolute window-space x — the sidebar is flush against the window's
    /// left edge, so `mouse.x` *is* the new width, no start-position
    /// bookkeeping needed. Live-updates `self.settings.sidebar_width` on
    /// every drag-move frame (for immediate visual feedback); persists via
    /// `Settings::save` only on release or a double-click reset — never
    /// per-pixel (docs/phase-4-settings-and-theming.md deliverable 5).
    ///
    /// Built on gpui's `on_drag`/`on_drag_move` (see `Div::on_drag_move`'s
    /// doc comment: "useful for implementing draggable UIs that don't
    /// conform to a drag and drop style interaction, like resizing") rather
    /// than gpui-component's `resizable` module (`refs/pr-test-gpui-component`'s
    /// `crates/ui/src/resizable/`, the same commit this workspace's
    /// `gpui-component` is pinned to) — that module's `ResizablePanelGroup`
    /// is built for N-way docked panel layouts (serialized sizes, a shared
    /// `ResizableState` entity, a dedicated full-bounds paint-time element
    /// for its `window.on_mouse_event` registration); adopting it here would
    /// mean restructuring the whole sidebar/main-content split around it for
    /// a single fixed handle. `on_drag`/`on_drag_move` is the primitive that
    /// component itself is built on, without the panel-group machinery.
    fn render_sidebar_resize_handle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = cx.theme().primary;
        div()
            .id("sidebar-resize-handle")
            .absolute()
            .top_0()
            .bottom_0()
            .right(px(-3.))
            .w(px(6.))
            .occlude()
            .cursor_col_resize()
            .group("sidebar-resize-handle")
            .on_drag(SidebarResizeDrag, |_, _, _, cx| cx.new(|_| EmptyView))
            .on_drag_move::<SidebarResizeDrag>(cx.listener(
                |this, event: &DragMoveEvent<SidebarResizeDrag>, _, cx| {
                    let width = f32::from(event.event.position.x)
                        .clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX);
                    this.sidebar_dragging = true;
                    this.settings.sidebar_width = width;
                    cx.notify();
                },
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    // Double-click resets to the default width.
                    if event.click_count >= 2 {
                        this.settings.sidebar_width = DEFAULT_SIDEBAR_WIDTH;
                        this.settings.save();
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if std::mem::take(&mut this.sidebar_dragging) {
                        this.settings.save();
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    // The common case: a drag almost always ends with the
                    // cursor well outside this 6px strip. Gated on the drag
                    // flag — up_out fires for EVERY outside release.
                    if std::mem::take(&mut this.sidebar_dragging) {
                        this.settings.save();
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(2.))
                    .w(px(2.))
                    .group_hover("sidebar-resize-handle", move |el| el.bg(accent)),
            )
    }

    /// The delete-review confirmation modal (context menu → "Delete
    /// review…"), on the shared modal recipe (`render_theme_picker`'s doc
    /// comment). Destructive-action posture: names exactly what will be
    /// deleted (title, repo, comment count), the store delete only runs
    /// from its Delete button / enter, and a failure surfaces here rather
    /// than closing optimistically.
    fn render_delete_confirm(&self, cx: &mut Context<Self>) -> Option<Div> {
        use gpui_component::Disableable as _;
        use gpui_component::button::{Button, ButtonVariants as _};

        let confirm = self.delete_confirm.as_ref()?;
        let theme = cx.theme();
        let panel_bg = theme.sidebar;
        let fg = theme.foreground;
        let muted = theme.muted_foreground;
        let danger = theme.danger;
        let dv = themes::dv_theme(cx);
        let seam = dv.modal_border;
        let backdrop = dv.backdrop;
        let text_secondary = dv.text_secondary;

        let entry = self.index.get(&confirm.review_id);
        let title = entry.map(|e| e.title.clone()).unwrap_or_default();
        let repo = entry
            .map(|e| dv_core::repo_label(&e.location, e.remote.as_ref()))
            .unwrap_or_default();
        let comments = entry.map(|e| e.open_comments).unwrap_or(0);
        let detail = if comments == 1 {
            format!("{repo} · {title} · 1 open comment")
        } else if comments > 1 {
            format!("{repo} · {title} · {comments} open comments")
        } else {
            format!("{repo} · {title}")
        };
        let in_flight = confirm.in_flight;
        let error = confirm.error.clone();

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(backdrop)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.close_delete_confirm(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(48.))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            v_flex()
                                .w(px(560.))
                                .max_w_full()
                                .overflow_hidden()
                                .p_3()
                                .gap_2()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .bg(panel_bg)
                                .text_color(fg)
                                .text_size(px(13.))
                                .border_1()
                                .border_color(seam)
                                .rounded_lg()
                                .shadow_lg()
                                .child(div().font_semibold().child("Delete review?"))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(text_secondary)
                                        .truncate()
                                        .child(detail),
                                )
                                .child(div().text_color(muted).child(
                                    "This permanently deletes the review and its comments \
                                     from the repo's .git/dv store. Archiving (right-click) \
                                     hides it instead, without deleting anything.",
                                ))
                                .when_some(error, |el, err| {
                                    el.child(div().text_color(danger).child(err))
                                })
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .justify_end()
                                        .child(
                                            Button::new("delete-confirm-cancel")
                                                .ghost()
                                                .small()
                                                .label("Cancel")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.close_delete_confirm(window, cx);
                                                })),
                                        )
                                        .child(
                                            Button::new("delete-confirm-delete")
                                                .danger()
                                                .small()
                                                .label(if in_flight {
                                                    "Deleting…"
                                                } else {
                                                    "Delete"
                                                })
                                                .disabled(in_flight)
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.on_delete_review_confirm(
                                                        &DeleteReviewConfirm,
                                                        window,
                                                        cx,
                                                    );
                                                })),
                                        ),
                                ),
                        ),
                ),
        )
    }

    /// The theme picker overlay (`ctrl-shift-t`), when open: same
    /// popover-over-everything chrome as `workspace.rs`'s `render_pr_picker`,
    /// but listing the static theme registry instead — no loading/error
    /// state, since there's no async fetch involved.
    /// The shared modal-picker recipe
    /// (R2 item 6): full-viewport `occlude()` scrim at
    /// DvTheme `backdrop` (click closes, matching escape), 560px panel on
    /// `sidebar.background` with a `muted` seam border (never `border` —
    /// Aura's is #000), 13px base text, 11px caption, 30px rows
    /// (`mx_1 px_2 rounded_md`, selected = `surface_active`). Same recipe
    /// as `Workspace::render_palette`/`render_pr_picker`.
    fn render_theme_picker(&self, cx: &mut Context<Self>) -> Option<Div> {
        let picker = self.theme_picker.as_ref()?;

        let theme = cx.theme();
        let panel_bg = theme.sidebar;
        let fg = theme.foreground;
        let muted = theme.muted_foreground;
        let success = theme.success;
        let dv = themes::dv_theme(cx);
        let seam = dv.modal_border;
        let backdrop = dv.backdrop;
        let surface_active = dv.surface_active;
        let active_theme = self.settings.theme.clone();

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(backdrop)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.close_theme_picker(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(48.))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            v_flex()
                                .w(px(560.))
                                .max_w_full()
                                .overflow_hidden()
                                .p_2()
                                .gap_2()
                                // Same swallow-the-click-on-chrome reasoning as
                                // `render_pr_picker`.
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .bg(panel_bg)
                                .text_color(fg)
                                .text_size(px(13.))
                                .border_1()
                                .border_color(seam)
                                .rounded_lg()
                                .shadow_lg()
                                .child(
                                    div()
                                        .px_2()
                                        .pt_1()
                                        .text_size(px(11.))
                                        .text_color(muted)
                                        .child("Theme \u{b7} enter to apply, esc to close"),
                                )
                                .child(v_flex().w_full().children(
                                    themes::names().enumerate().map(|(i, name)| {
                                        let selected = i == picker.selected;
                                        let is_active = name == active_theme;
                                        // No w_full alongside mx_1: 100%
                                        // width PLUS margins overflows the
                                        // content box 4px past the right
                                        // inset (R2 review, P2) — flex
                                        // stretch alone sizes the row to
                                        // content-box minus margins.
                                        h_flex()
                                            .id(("theme-picker-row", i))
                                            .h(px(30.))
                                            .mx_1()
                                            .gap_2()
                                            .px_2()
                                            .items_center()
                                            .rounded_md()
                                            .cursor_pointer()
                                            .when(selected, |el| el.bg(surface_active))
                                            .hover(|el| el.bg(surface_active.opacity(0.5)))
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
                                                    // `Icon`, not U+2713 text — the UI
                                                    // font tofus the glyph at small
                                                    // sizes (R1e finding).
                                                    .children(is_active.then(|| {
                                                        gpui_component::Icon::new(
                                                            gpui_component::IconName::Check,
                                                        )
                                                        .xsmall()
                                                    })),
                                            )
                                            .child(div().flex_1().child(name))
                                    }),
                                )),
                        ),
                ),
        )
    }

    /// The settings panel (`ctrl-,`), when open: a centered modal (wider
    /// than the pickers — this has more to show), two sections. Mouse-first
    /// throughout; only escape has a keybinding (see `init`'s
    /// `SettingsPanelOpen` context) — every control here is a button,
    /// stepper, or text input clicked/typed directly, which the doc
    /// explicitly signs off on ("this is fine").
    fn render_settings_panel(&self, cx: &mut Context<Self>) -> Option<Div> {
        let panel = self.settings_panel.as_ref()?;

        let theme = cx.theme();
        // R2 item 6: the modal chrome tokens the pickers standardized on
        // (dv `backdrop` scrim, `sidebar` panel, `modal_border` seam) —
        // one modal recipe app-wide.
        let border = themes::dv_theme(cx).modal_border;
        let popover = theme.sidebar;
        let popover_fg = theme.foreground;
        let muted = theme.muted_foreground;
        let backdrop = themes::dv_theme(cx).backdrop;

        let segmented_row =
            |id_prefix: &'static str,
             selected_name: String,
             small: bool,
             on_pick: fn(&mut Self, &'static str, &mut Window, &mut Context<Self>),
             cx: &mut Context<Self>| {
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children(themes::names().map(move |name| {
                        let selected = name == selected_name;
                        let btn = Button::new(format!("{id_prefix}-{name}"))
                            .ghost()
                            .selected(selected)
                            .label(name)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                on_pick(this, name, window, cx);
                            }));
                        if small { btn.xsmall() } else { btn }
                    }))
            };

        let view_mode = self.settings.view_mode_default;
        let context_lines = self.settings.context_lines;
        let follow_on = self.settings.follow_os_appearance;
        let font_size = self.settings.mono_font_size;

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(backdrop)
                // `occlude()` blocks hover/scroll from bleeding through to
                // whatever's dimmed underneath (e.g. a recent-review row's
                // hover state lighting up through the backdrop). Click on
                // the dimmed backdrop closes the panel, matching escape —
                // but not a click on the panel itself (stopped below), same
                // swallow-the-click pattern the pickers use. Review finding:
                // this handler used to close the panel *and* let the click
                // fall through to whatever sidebar row was underneath,
                // opening it — `stop_propagation()` is the actual fix,
                // `occlude()` only covers the hover/scroll half of the hole.
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.close_settings_panel(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    v_flex()
                        .id("settings-panel")
                        .w(px(560.))
                        .max_w_full()
                        .max_h(px(600.))
                        .overflow_hidden()
                        .p_4()
                        .gap_4()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .bg(popover)
                        .text_color(popover_fg)
                        .border_1()
                        .border_color(border)
                        .rounded_lg()
                        .shadow_lg()
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .child(div().font_semibold().child("Settings"))
                                .child(div().text_xs().text_color(muted).child("esc to close")),
                        )
                        // ---- General ------------------------------------
                        .child(
                            v_flex()
                                .gap_3()
                                .child(div().text_xs().text_color(muted).child("GENERAL"))
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_center()
                                        .child(div().text_sm().child("Default view"))
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .child(
                                                    Button::new("settings-view-unified")
                                                        .ghost()
                                                        .selected(
                                                            view_mode == ViewModeSetting::Unified,
                                                        )
                                                        .label("Unified")
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.set_view_mode_default(
                                                                ViewModeSetting::Unified,
                                                                cx,
                                                            );
                                                        })),
                                                )
                                                .child(
                                                    Button::new("settings-view-split")
                                                        .ghost()
                                                        .selected(
                                                            view_mode == ViewModeSetting::Split,
                                                        )
                                                        .label("Split")
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.set_view_mode_default(
                                                                ViewModeSetting::Split,
                                                                cx,
                                                            );
                                                        })),
                                                ),
                                        ),
                                )
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_center()
                                        .child(div().text_sm().child("Context lines"))
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .items_center()
                                                .child(
                                                    Button::new("settings-context-minus")
                                                        .ghost()
                                                        .xsmall()
                                                        .label("−")
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.set_context_lines(
                                                                    context_lines.saturating_sub(1),
                                                                    cx,
                                                                );
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .w(px(24.))
                                                        .text_center()
                                                        .text_sm()
                                                        .child(context_lines.to_string()),
                                                )
                                                .child(
                                                    Button::new("settings-context-plus")
                                                        .ghost()
                                                        .xsmall()
                                                        .label("+")
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.set_context_lines(
                                                                    context_lines + 1,
                                                                    cx,
                                                                );
                                                            },
                                                        )),
                                                ),
                                        ),
                                ),
                        )
                        .child(div().h(px(1.)).w_full().bg(border))
                        // ---- Appearance ----------------------------------
                        .child(
                            v_flex()
                                .gap_3()
                                .child(div().text_xs().text_color(muted).child("APPEARANCE"))
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(div().text_sm().child("Theme"))
                                        .child(segmented_row(
                                            "settings-theme",
                                            self.settings.theme.clone(),
                                            false,
                                            |this, name, window, cx| {
                                                this.choose_theme(name, window, cx)
                                            },
                                            cx,
                                        )),
                                )
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_center()
                                        .child(div().text_sm().child("Follow OS appearance"))
                                        .child(
                                            Button::new("settings-follow-os")
                                                .ghost()
                                                .selected(follow_on)
                                                .label(if follow_on { "On" } else { "Off" })
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.set_follow_os_appearance(
                                                            !follow_on, window, cx,
                                                        );
                                                    },
                                                )),
                                        ),
                                )
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .items_center()
                                                .child(
                                                    div()
                                                        .w(px(40.))
                                                        .text_xs()
                                                        .text_color(muted)
                                                        .child("Light"),
                                                )
                                                .child(segmented_row(
                                                    "settings-light",
                                                    self.settings.light_theme.clone(),
                                                    true,
                                                    |this, name, window, cx| {
                                                        // Always a name from `themes::names()`
                                                        // (the row above only renders those), so
                                                        // this can't actually fail — `.ok()`
                                                        // just discards the `Result` to match
                                                        // `on_pick`'s `fn(...)` return type.
                                                        this.set_light_theme(
                                                            name.to_string(),
                                                            window,
                                                            cx,
                                                        )
                                                        .ok();
                                                    },
                                                    cx,
                                                )),
                                        )
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .items_center()
                                                .child(
                                                    div()
                                                        .w(px(40.))
                                                        .text_xs()
                                                        .text_color(muted)
                                                        .child("Dark"),
                                                )
                                                .child(segmented_row(
                                                    "settings-dark",
                                                    self.settings.dark_theme.clone(),
                                                    true,
                                                    |this, name, window, cx| {
                                                        // Same reasoning as the light-theme row
                                                        // above: always a valid name, `.ok()`
                                                        // just matches `on_pick`'s return type.
                                                        this.set_dark_theme(
                                                            name.to_string(),
                                                            window,
                                                            cx,
                                                        )
                                                        .ok();
                                                    },
                                                    cx,
                                                )),
                                        )
                                        .child(div().text_xs().text_color(muted).child(
                                            "Re-checked on real OS light/dark change events \
                                                 (no per-frame poll — see docs).",
                                        )),
                                )
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_center()
                                        .child(div().text_sm().child("Mono font"))
                                        .child(
                                            div()
                                                .w(px(220.))
                                                .child(Input::new(&panel.mono_font_input)),
                                        ),
                                )
                                .child(
                                    h_flex()
                                        .justify_between()
                                        .items_center()
                                        .child(div().text_sm().child("Font size"))
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .items_center()
                                                .child(
                                                    Button::new("settings-font-minus")
                                                        .ghost()
                                                        .xsmall()
                                                        .label("−")
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.set_mono_font_size(
                                                                    font_size - 1.,
                                                                    cx,
                                                                );
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .w(px(28.))
                                                        .text_center()
                                                        .text_sm()
                                                        .child(format!("{font_size:.0}")),
                                                )
                                                .child(
                                                    Button::new("settings-font-plus")
                                                        .ghost()
                                                        .xsmall()
                                                        .label("+")
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.set_mono_font_size(
                                                                    font_size + 1.,
                                                                    cx,
                                                                );
                                                            },
                                                        )),
                                                ),
                                        ),
                                ),
                        ),
                ),
        )
    }

    /// The onboarding overlay (S8e) — same modal backdrop pattern as
    /// `render_settings_panel`/`render_theme_picker`: `inset_0().occlude()`
    /// plus a click-to-close backdrop, with `stop_propagation()` on the
    /// card itself so a click doesn't fall through to whatever sidebar row
    /// sits under it (same review finding that pattern's own doc comment
    /// describes).
    fn render_onboarding(&self, cx: &mut Context<Self>) -> Option<Div> {
        let page = self.onboarding.as_ref()?;

        let theme = cx.theme();
        // Same app-wide modal chrome as the pickers/settings panel (R2
        // item 6) — see `render_settings_panel`.
        let border = themes::dv_theme(cx).modal_border;
        let popover = theme.sidebar;
        let popover_fg = theme.foreground;
        let muted = theme.muted_foreground;
        let backdrop = themes::dv_theme(cx).backdrop;

        let title = if page.first_run {
            "Welcome to dv"
        } else {
            "Setup status"
        };
        let subtitle = if page.running {
            "Checking your setup\u{2026}"
        } else {
            "Everything dv needs to review code, in one place."
        };

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(backdrop)
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.close_onboarding_page(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    v_flex()
                        .id("onboarding-page")
                        .w(px(520.))
                        .max_w_full()
                        .max_h(px(560.))
                        .overflow_hidden()
                        .p_4()
                        .gap_3()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .bg(popover)
                        .text_color(popover_fg)
                        .border_1()
                        .border_color(border)
                        .rounded_lg()
                        .shadow_lg()
                        .child(
                            h_flex()
                                .justify_between()
                                .items_center()
                                .child(div().font_semibold().child(title))
                                .child(
                                    Button::new("onboarding-close")
                                        .ghost()
                                        .xsmall()
                                        .label("Close")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.close_onboarding_page(window, cx);
                                        })),
                                ),
                        )
                        .child(div().text_xs().text_color(muted).child(subtitle))
                        .child(
                            v_flex().gap_2().children(
                                page.rows
                                    .iter()
                                    .enumerate()
                                    .map(|(idx, row)| self.render_onboarding_row(idx, row, cx)),
                            ),
                        ),
                ),
        )
    }

    /// One onboarding row: a status glyph + title on the first line, an
    /// optional detail/guidance string on the second, and (only for a
    /// `RowState::Consent` row) an "Install" button running
    /// `Self::on_onboarding_consent_install` — the app's ONLY UI entry
    /// point into `dv_core::provision::install_vtsls`. `idx` (the row's
    /// position, not its `ComponentId`) keys the install button's element
    /// id: two DIFFERENT distros can both land on a `NodeVtsls` consent row
    /// in the same render, and `ComponentId` alone would collide.
    fn render_onboarding_row(
        &self,
        idx: usize,
        row: &crate::onboarding::Row,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let success = theme.success;
        let danger = theme.danger;
        let warning = theme.warning;
        let muted = theme.muted_foreground;
        let (glyph, glyph_color, detail, detail_color): (&str, Hsla, Option<String>, Hsla) =
            match &row.state {
                RowState::Checking => ("\u{25cb}", muted, None, muted),
                RowState::Ok(detail) => ("\u{25cf}", success, Some(detail.clone()), muted),
                RowState::Missing(guidance) => {
                    ("\u{25cb}", warning, Some(guidance.clone()), warning)
                }
                RowState::Consent(_, detail) => {
                    ("\u{25cf}", warning, Some(detail.clone()), warning)
                }
                RowState::Installing => (
                    "\u{25cf}",
                    warning,
                    Some("installing\u{2026}".to_string()),
                    muted,
                ),
                RowState::Failed(error) => ("\u{2715}", danger, Some(error.clone()), danger),
                RowState::Skipped(reason) => ("\u{25cb}", muted, Some(reason.clone()), muted),
            };
        // Per-action button label: "Install" fits a package install but
        // would misdescribe the PATH registration.
        let consent_label: Option<&'static str> = match &row.state {
            RowState::Consent(ConsentAction::InstallVtsls { .. }, _) => Some("Install"),
            RowState::Consent(ConsentAction::AddDvToPath { .. }, _) => Some("Add to PATH"),
            _ => None,
        };

        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_color(glyph_color).child(glyph))
                            .child(div().text_sm().child(row.title.clone())),
                    )
                    .when_some(consent_label, |el, label| {
                        el.child(
                            Button::new(SharedString::from(format!("onboarding-install-{idx}")))
                                .ghost()
                                .xsmall()
                                .label(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    // Errors here are precondition rejections
                                    // (page closed, row no longer awaiting
                                    // consent, install already in flight) —
                                    // nothing new for a mouse click to show;
                                    // `Self::automation_onboarding_consent`
                                    // surfaces the same `Err` to a script.
                                    let _ = this.on_onboarding_consent_install(idx, cx);
                                })),
                        )
                    }),
            )
            .children(detail.map(|d| div().text_xs().text_color(detail_color).child(d)))
    }

    /// Bottom status-bar keycap legend
    /// (R1f): a fixed 28px strip
    /// of outlined keycap chips + dim labels listing dv's real shortcuts. A
    /// static, hand-maintained list — deliberately
    /// NOT a bindings-registry reflection (the plan's key_signatures forbid
    /// over-engineering this). Keep in sync with `init`'s `bind_keys` and
    /// workspace.rs's own bindings when those change — the R1f code review
    /// caught a "c → comment" chip here for a binding that doesn't exist
    /// (comments are mouse-drag only); every entry below is a real binding.
    ///
    /// Chips are hand-styled (not `Kbd`'s default appearance) in an
    /// outlined-transparent recipe — 1px border, footer-bg interior — using
    /// `Kbd::format` for the platform keystroke text. Border/labels use
    /// `muted.foreground` alphas rather than `muted.background`, which
    /// Claude Light defines identical to `sidebar.background` (the R1f
    /// visual review's invisible-chip P2).
    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted_fg = theme.muted_foreground;
        let chip_border = muted_fg.opacity(0.35);
        let hint = move |keys: &[&str], label: &'static str| {
            let mut hint = h_flex().items_center().gap_1();
            for key in keys {
                if let Ok(stroke) = gpui::Keystroke::parse(key) {
                    hint = hint.child(
                        div()
                            .flex_none()
                            .px_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(chip_border)
                            .text_size(px(11.))
                            .text_color(muted_fg)
                            .child(Kbd::format(&stroke)),
                    );
                }
            }
            hint.child(div().text_color(muted_fg).child(SharedString::from(label)))
        };
        h_flex()
            .h(px(28.))
            .flex_none()
            .items_center()
            .gap_4()
            .px_3()
            .overflow_hidden()
            .bg(theme.sidebar)
            .border_t_1()
            .border_color(muted_fg.opacity(0.25))
            .text_size(px(12.))
            .child(hint(&["j", "k"], "files"))
            .child(hint(&["n", "p"], "hunks"))
            .child(hint(&["s"], "split"))
            .child(hint(&["r"], "review"))
            .child(hint(&["f"], "jump"))
            .child(hint(&["ctrl-g"], "PRs"))
            .child(hint(&["ctrl-b"], "sidebar"))
            .child(hint(&["ctrl-,"], "settings"))
            .child(hint(&["ctrl-shift-t"], "theme"))
            .child(hint(&["escape"], "close"))
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Onboarding spine (S8e review, P2): a drift page auto-surfaced by
        // `Self::apply_consistency_report` is set from inside a background
        // task's `Entity::update`, which has no `Window` to focus with
        // directly — unlike the theme picker/settings panel/first-run open,
        // none of which have this gap, since each of THEIR open paths always
        // runs with a real `Window` in hand and focuses the shell as its
        // last step. Pull focus back onto the shell here instead, every
        // render while the page is open (`is_focused` makes this a no-op
        // once it's already true). Without it, keyboard input — most
        // importantly `escape` — keeps going wherever it already was (a
        // workspace's own deeper `escape` binding outranks the shell's
        // `OnboardingClose` once focus is on that deeper node), and the
        // modal is only dismissible by mouse.
        if self.onboarding.is_some() && !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        let theme = cx.theme();
        // Computed once per render (mirrors the old `review_count`'s own
        // one-per-render computation) and `move`d into the `uniform_list`
        // processor below, rather than recomputed per visible range —
        // `visible_sidebar_items` is what both this render pass and
        // `Self::automation_state`'s `"sidebar"` field build from, so
        // what a script asserts is exactly what's on screen.
        let sidebar_items = self.visible_sidebar_items();
        let sidebar_items_len = sidebar_items.len();
        // Copy colors hoisted out of `theme` for the sidebar's `when`
        // closure below (it needs `cx` uniquely, so it can't also hold
        // `theme`'s shared borrow of `cx`).
        let sidebar_seam = theme.muted;
        let sidebar_bg = theme.sidebar;
        let sidebar_label_fg = theme.muted_foreground;
        let sidebar_danger = theme.danger;
        let quick_open_error = self.quick_open_error.clone();

        let active_title = self
            .selected_review_id
            .as_deref()
            .and_then(|id| self.index.get(id))
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

        // While the theme picker, settings panel, or onboarding page is open
        // the shell node carries an extra identifier, flipping which key
        // bindings apply (see `init`) — same mechanism `workspace.rs` uses
        // for its own overlays. All three are mutually exclusive (see
        // `on_open_theme_picker`/`on_open_settings`/`on_open_onboarding`'s
        // guards), so at most one of these ever applies.
        let mut key_context = KEY_CONTEXT.to_string();
        if self.theme_picker.is_some() {
            key_context.push(' ');
            key_context.push_str(THEME_PICKER_CONTEXT);
        }
        if self.settings_panel.is_some() {
            key_context.push(' ');
            key_context.push_str(SETTINGS_PANEL_CONTEXT);
        }
        if self.onboarding.is_some() {
            key_context.push(' ');
            key_context.push_str(ONBOARDING_CONTEXT);
        }
        if self.delete_confirm.is_some() {
            key_context.push(' ');
            key_context.push_str(DELETE_CONFIRM_CONTEXT);
        }
        if self.command_palette.is_some() {
            key_context.push(' ');
            key_context.push_str(COMMAND_PALETTE_CONTEXT);
        }

        v_flex()
            .size_full()
            .relative()
            .track_focus(&self.focus_handle)
            .key_context(key_context.as_str())
            .on_action(cx.listener(Self::on_new_review))
            .on_action(cx.listener(Self::on_refresh_badges))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_open_theme_picker))
            .on_action(cx.listener(Self::on_theme_picker_next))
            .on_action(cx.listener(Self::on_theme_picker_prev))
            .on_action(cx.listener(Self::on_theme_picker_close))
            .on_action(cx.listener(Self::on_theme_picker_choose))
            .on_action(cx.listener(Self::on_open_settings))
            .on_action(cx.listener(Self::on_settings_close))
            .on_action(cx.listener(Self::on_open_onboarding))
            .on_action(cx.listener(Self::on_onboarding_close))
            .on_action(cx.listener(Self::on_toggle_archive_review))
            .on_action(cx.listener(Self::on_delete_review_prompt))
            .on_action(cx.listener(Self::on_delete_review_confirm))
            .on_action(cx.listener(Self::on_delete_review_cancel))
            .on_action(cx.listener(Self::on_open_command_palette))
            .on_action(cx.listener(Self::on_command_palette_next))
            .on_action(cx.listener(Self::on_command_palette_prev))
            .on_action(cx.listener(Self::on_command_palette_close))
            .on_action(cx.listener(Self::on_command_palette_choose))
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
                    // Sidebar: the review navigator. Omitted from the tree
                    // entirely while hidden (ctrl-b) — not width-0; see
                    // `Self::set_sidebar_visible`. The `when` closure needs
                    // unique access to `cx` (the render_sidebar_* helpers),
                    // so it uses the hoisted Copy colors above rather than
                    // capturing `theme`'s shared borrow of the same `cx`.
                    .when(self.settings.sidebar_visible, |el| {
                        el.child(
                            v_flex()
                                .h_full()
                                .w(px(self.settings.sidebar_width))
                                .flex_none()
                                .relative()
                                .border_r_1()
                                // muted, not `border`: Aura Dark defines
                                // `border` as #000000 (a called-out
                                // trap) — the divider should read as a soft
                                // lightened seam.
                                .border_color(sidebar_seam)
                                .bg(sidebar_bg)
                                // "Open anything"
                                // (R2): one field for a PR URL /
                                // owner/repo#123 / #123 / path, with an
                                // inline error that the next edit (or a
                                // click on it) dismisses.
                                .child(
                                    v_flex()
                                        .px_2()
                                        .pt_2()
                                        .gap_1()
                                        .w_full()
                                        .child(Input::new(&self.quick_open).small().w_full())
                                        .children(quick_open_error.map(|message| {
                                            div()
                                                .id("quick-open-error")
                                                .w_full()
                                                .text_size(px(11.))
                                                .text_color(sidebar_danger)
                                                .cursor_pointer()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.quick_open_error = None;
                                                    cx.notify();
                                                }))
                                                .child(message)
                                        })),
                                )
                                .child(
                                    div().p_2().w_full().child(
                                        // Quiet chrome (the sidebar
                                        // density pass, R1e): an
                                        // outlined `primary` rather than a
                                        // solid-filled one — still the row's
                                        // most prominent control, but no longer
                                        // a heavy block sitting above the dense,
                                        // low-contrast card list below it.
                                        Button::new("new-review")
                                            .primary()
                                            .outline()
                                            .small()
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
                                        .gap_1()
                                        .items_center()
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_xs()
                                                .text_color(sidebar_label_fg)
                                                .child("REVIEWS"),
                                        )
                                        .child(
                                            // Manual reopen of the onboarding page
                                            // (S8e) — the only other entry point
                                            // besides true first run / an
                                            // auto-surfaced drift page /
                                            // `--automation`'s `OpenOnboarding`
                                            // action (`Self::on_open_onboarding`).
                                            Button::new("open-onboarding")
                                                .ghost()
                                                .xsmall()
                                                .label("Setup")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.on_open_onboarding(
                                                        &OpenOnboarding,
                                                        window,
                                                        cx,
                                                    )
                                                })),
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
                                                    this.on_refresh_badges(
                                                        &RefreshBadges,
                                                        window,
                                                        cx,
                                                    )
                                                })),
                                        ),
                                )
                                // Grouping/filtering control row (docs/phase-6-
                                // review-navigator.md deliverables 3/4) — mouse-
                                // only throughout (cross-cutting risk E).
                                .child(
                                    h_flex()
                                        .px_2()
                                        .pb_1()
                                        .gap_1()
                                        .items_center()
                                        .child(self.render_sidebar_grouping_control(cx))
                                        .child(div().flex_1())
                                        .child(self.render_sidebar_filter_button(cx)),
                                )
                                .child(
                                    uniform_list(
                                        "review-list",
                                        sidebar_items_len,
                                        cx.processor(
                                            move |this, range: std::ops::Range<usize>, _, cx| {
                                                range
                                                    .map(|i| match &sidebar_items[i] {
                                                        SidebarItem::Header { key, label } => this
                                                            .render_sidebar_header(
                                                                key.clone(),
                                                                label.clone(),
                                                                cx,
                                                            )
                                                            .into_any_element(),
                                                        SidebarItem::Review(idx) => {
                                                            let entry = &this.index.entries()[*idx];
                                                            let selected = this
                                                                .selected_review_id
                                                                .as_deref()
                                                                == Some(entry.review_id.as_str());
                                                            this.render_review_card(
                                                                entry, selected, cx,
                                                            )
                                                            .into_any_element()
                                                        }
                                                    })
                                                    .collect::<Vec<_>>()
                                            },
                                        ),
                                    )
                                    .flex_1()
                                    // px_2, not px_1: carries the side inset the
                                    // card itself cannot (root-node margins are
                                    // inert in uniform_list — see
                                    // render_review_card).
                                    .px_2(),
                                )
                                .child(self.render_sidebar_resize_handle(cx)),
                        )
                    })
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
            .child(self.render_footer(cx))
            .children(self.render_theme_picker(cx))
            .children(self.render_settings_panel(cx))
            .children(self.render_onboarding(cx))
            .children(self.render_sidebar_filter_popover(cx))
            .children(self.render_delete_confirm(cx))
            .children(self.render_command_palette(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChecksSummary, DiffSource, PrBadge, PrState, ReviewBadge, merge_local_badge,
        should_stamp_conflict,
    };

    fn badge(open: usize, pr: Option<PrBadge>, pr_number: Option<u64>) -> ReviewBadge {
        ReviewBadge {
            open,
            submitted: false,
            pr,
            pr_number,
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

    // --- should_stamp_conflict (review finding P2-1) -----------------------

    fn range(base: &str, head: &str) -> DiffSource {
        DiffSource::Range {
            base: base.to_string(),
            head: head.to_string(),
            merge_base: true,
        }
    }

    #[test]
    fn stamp_conflict_same_kind_same_oids_stamps() {
        // The trivial agreement case: workspace shows exactly the review's
        // stored range — the live probe is about this very diff.
        assert!(should_stamp_conflict(
            &range("aaa", "bbb"),
            &range("aaa", "bbb")
        ));
    }

    #[test]
    fn stamp_conflict_same_kind_moved_oids_still_stamps() {
        // The P2-1 case: a reopened PR workspace probes a FRESH range (new
        // head/merge-base) while `review.source` stays frozen at draft
        // creation. The workspace's probe is the authoritative fresh answer
        // for this review — exact-equality gating locked it out forever and
        // let hydration's older-head answer contradict the open header.
        assert!(should_stamp_conflict(
            &range("frozen-merge-base", "old-head"),
            &range("fresh-merge-base", "new-head")
        ));
    }

    #[test]
    fn stamp_conflict_cross_kind_does_not_stamp() {
        // The guard's motivating case (must keep working): a bare launch
        // adopts a range review while the workspace shows the WORKING-TREE
        // diff — its `ls-files -u` probe says nothing about the range and
        // must not erase hydration's persisted-live-base finding.
        assert!(!should_stamp_conflict(
            &range("aaa", "bbb"),
            &DiffSource::WorkingTree
        ));
    }

    // --- merge_local_badge (review finding P3-3) ---------------------------

    #[test]
    fn merge_local_badge_fetch_remote_true_takes_fresh_local_verbatim() {
        let existing = Some(badge(1, Some(pr_badge()), Some(1)));
        let fresh_local = badge(3, None, Some(1)); // local pass never sets `pr` itself
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
        let existing = Some(badge(1, Some(pr_badge()), Some(1)));
        let fresh_local = badge(2, None, Some(1)); // same PR still latest
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
        let merged = merge_local_badge(None, badge(1, None, None), false);
        assert!(
            merged.pr.is_none(),
            "nothing to preserve when there was no existing badge at all"
        );
    }

    #[test]
    fn merge_local_badge_fetch_remote_false_drops_pr_when_latest_review_changes() {
        // The location's "latest" review switched to a different PR (e.g. a
        // newer draft was created) between local passes — the previously
        // cached `pr` belongs to the *old* latest review's PR and must not
        // be carried over onto the new one just because `fetch_remote` is
        // false (review finding P2, generalized to the badge cache).
        let existing = Some(badge(1, Some(pr_badge()), Some(10)));
        let fresh_local = badge(2, None, Some(20));
        let merged = merge_local_badge(existing, fresh_local, false);
        assert!(
            merged.pr.is_none(),
            "a stale pr badge for a different PR must not be carried forward"
        );
        assert_eq!(
            merged.pr_number,
            Some(20),
            "pr_number tracks the new latest review"
        );
    }

    // --- entry_passes_filters / repo_group_key / status_group_label
    // (docs/phase-6-review-navigator.md deliverables 3/4) -----------------

    #[cfg(feature = "automation")]
    use super::grouping_word;
    use super::{
        SidebarFilters, entry_passes_filters, pr_group_key, repo_group_key, status_group_label,
    };
    #[cfg(feature = "automation")]
    use crate::settings::SidebarGrouping;
    use std::path::PathBuf;

    fn local(name: &str) -> dv_core::RepoLocation {
        dv_core::RepoLocation::Local(PathBuf::from(format!("D:\\code\\{name}")))
    }

    fn remote_ref(pr: u64) -> dv_core::RemoteRef {
        dv_core::RemoteRef {
            provider: "github".to_string(),
            slug: "github.com/kylekz/difftest".to_string(),
            pr,
            url: format!("https://github.com/kylekz/difftest/pull/{pr}"),
            submitted_review_id: None,
            submitted_url: None,
        }
    }

    fn sample_index_entry(id: &str) -> dv_core::IndexEntry {
        dv_core::IndexEntry {
            review_id: id.to_string(),
            location: local("difftest"),
            source: dv_core::DiffSource::WorkingTree,
            title: "working tree".to_string(),
            state: dv_core::ReviewState::Draft,
            open_comments: 0,
            remote: None,
            pr_status: None,
            diffstat: None,
            conflict: None,
            updated_ms: 1,
            last_opened_ms: 1,
            health: dv_core::EntryHealth::Ok,
            archived: false,
        }
    }

    fn cached_pr(is_draft: bool, state: PrState) -> dv_core::CachedPrStatus {
        dv_core::CachedPrStatus {
            state,
            is_draft,
            decision: None,
            checks: ChecksSummary::None,
        }
    }

    #[test]
    fn entry_passes_filters_unlinked_review_checked_against_unlinked_bucket_only() {
        let entry = sample_index_entry("r-1");
        assert!(entry.remote.is_none());
        let mut filters = SidebarFilters::default();
        assert!(entry_passes_filters(&entry, &filters));
        filters.unlinked = false;
        assert!(
            !entry_passes_filters(&entry, &filters),
            "a local-only review must be gated by `unlinked`, not any pr_* bool"
        );
    }

    #[test]
    fn entry_passes_filters_pr_linked_not_yet_hydrated_always_passes_pr_axis() {
        // cross-cutting: a PR-linked review whose `pr_status` hasn't landed
        // yet must not be hidden by ANY pr_* filter being off — "unknown"
        // is not "filtered out".
        let mut entry = sample_index_entry("r-1");
        entry.remote = Some(remote_ref(7));
        entry.pr_status = None;
        let filters = SidebarFilters {
            pr_draft: false,
            pr_open: false,
            pr_merged: false,
            pr_closed: false,
            ..SidebarFilters::default()
        };
        assert!(
            entry_passes_filters(&entry, &filters),
            "an un-hydrated PR-linked review must pass regardless of pr_* filters"
        );
    }

    #[test]
    fn entry_passes_filters_pr_linked_buckets_by_draft_then_state() {
        let mut entry = sample_index_entry("r-1");
        entry.remote = Some(remote_ref(7));

        entry.pr_status = Some(cached_pr(true, PrState::Open));
        let filters = SidebarFilters {
            pr_draft: false,
            ..SidebarFilters::default()
        };
        assert!(
            !entry_passes_filters(&entry, &filters),
            "is_draft must win over PrState::Open — a draft PR is filtered by pr_draft, not pr_open"
        );

        entry.pr_status = Some(cached_pr(false, PrState::Merged));
        let mut filters = SidebarFilters {
            pr_merged: false,
            ..SidebarFilters::default()
        };
        assert!(!entry_passes_filters(&entry, &filters));
        filters.pr_merged = true;
        assert!(entry_passes_filters(&entry, &filters));
    }

    #[test]
    fn entry_passes_filters_closed_draft_pr_buckets_by_pr_closed_not_pr_draft() {
        // GitHub allows closing a draft PR without ever marking it ready
        // for review, so `is_draft: true, state: Closed` is a reachable
        // combination — it must be gated by `pr_closed`, not `pr_draft`,
        // or toggling either filter independently stops matching what it
        // promises for this entry.
        let mut entry = sample_index_entry("r-1");
        entry.remote = Some(remote_ref(7));
        entry.pr_status = Some(cached_pr(true, PrState::Closed));

        let filters = SidebarFilters {
            pr_draft: false,
            pr_closed: true,
            ..SidebarFilters::default()
        };
        assert!(
            entry_passes_filters(&entry, &filters),
            "a closed draft must stay visible when pr_closed is on, even with pr_draft off"
        );

        let filters = SidebarFilters {
            pr_draft: true,
            pr_closed: false,
            ..SidebarFilters::default()
        };
        assert!(
            !entry_passes_filters(&entry, &filters),
            "a closed draft must be hidden when pr_closed is off, even with pr_draft on"
        );
    }

    #[test]
    fn entry_passes_filters_review_status_buckets_draft_and_each_verdict() {
        let mut entry = sample_index_entry("r-1");
        let filters = SidebarFilters {
            review_draft: false,
            ..SidebarFilters::default()
        };
        assert!(!entry_passes_filters(&entry, &filters));

        entry.state = dv_core::ReviewState::Submitted {
            verdict: dv_core::Verdict::Approve,
            at_ms: 1,
        };
        let mut filters = SidebarFilters::default();
        assert!(entry_passes_filters(&entry, &filters));
        filters.review_approved = false;
        assert!(!entry_passes_filters(&entry, &filters));
        // A different verdict bucket must be unaffected by
        // `review_approved` being off.
        entry.state = dv_core::ReviewState::Submitted {
            verdict: dv_core::Verdict::RequestChanges,
            at_ms: 1,
        };
        assert!(entry_passes_filters(&entry, &filters));
        filters.review_changes = false;
        assert!(!entry_passes_filters(&entry, &filters));
    }

    #[test]
    fn entry_passes_filters_unavailable_repo_bypasses_every_axis() {
        // A WSL-hosted review whose distro stopped (or whose path vanished)
        // keeps its last-known state/verdict per `apply_hydration`'s
        // contract (crates/core/src/index/mod.rs) — an unrelated filter
        // (e.g. hiding approved reviews) must not be able to hide the row
        // and take the sidebar's only unavailable-repo glyph out of view
        // with it (P3 finding).
        let mut entry = sample_index_entry("r-1");
        entry.state = dv_core::ReviewState::Submitted {
            verdict: dv_core::Verdict::Approve,
            at_ms: 1,
        };
        entry.remote = Some(remote_ref(7));
        entry.pr_status = Some(cached_pr(false, PrState::Closed));
        entry.health = dv_core::EntryHealth::RepoUnavailable;

        let filters = SidebarFilters {
            review_approved: false,
            pr_closed: false,
            unlinked: false,
            ..SidebarFilters::default()
        };
        assert!(
            entry_passes_filters(&entry, &filters),
            "an unavailable/missing repo's entry must stay visible regardless of PR/review filters"
        );

        entry.health = dv_core::EntryHealth::Missing;
        assert!(
            entry_passes_filters(&entry, &filters),
            "Missing health must bypass filters the same way RepoUnavailable does"
        );
    }

    #[test]
    fn entry_passes_filters_archived_hidden_by_default_and_wins_over_health_bypass() {
        let mut entry = sample_index_entry("r-1");
        entry.archived = true;
        let mut filters = SidebarFilters::default();
        assert!(
            !entry_passes_filters(&entry, &filters),
            "archived must be hidden with the default filter set"
        );
        filters.archived = true;
        assert!(
            entry_passes_filters(&entry, &filters),
            "the Archived filter opts archived rows back in"
        );

        // Archived beats the unavailable-repo bypass: tucking a review
        // away must hold even when its repo goes unreachable.
        entry.health = dv_core::EntryHealth::RepoUnavailable;
        filters.archived = false;
        assert!(
            !entry_passes_filters(&entry, &filters),
            "an archived review must stay hidden even when its repo is unavailable"
        );
    }

    #[test]
    fn repo_group_key_ignores_remote_so_every_review_at_a_location_shares_one_group() {
        // Live-verified regression (see the function's own doc comment):
        // an earlier version of this key passed `remote` through to
        // `repo_label`, so a PR-linked review (`owner/repo`) and a
        // local-only review at the exact same location (folder name) split
        // into two different Repo-grouping headers for what is genuinely
        // one repo.
        let loc = local("difftest");
        let key_unlinked = repo_group_key(&loc);
        assert_eq!(key_unlinked, "d:\\code\\difftest");

        // Two different PRs at the same location, plus the unlinked case
        // above, must all key identically — `repo_group_key` doesn't even
        // take a `remote` argument, so this is really just confirming the
        // key is a pure function of `location`.
        assert_eq!(repo_group_key(&local("difftest")), key_unlinked);
    }

    #[test]
    fn repo_group_key_keys_on_full_path_not_just_basename() {
        // Confirmed P3/P2 finding: keying on `repo_label`'s basename-only
        // output merged two entirely different repos that happen to share
        // a final path segment (e.g. an employer's repo and an unrelated
        // personal repo both named "api") into one Repo-grouping header.
        // Two different parent directories must produce two different
        // keys even though `local()` in these tests, and the finding's own
        // repro, share the trailing folder name.
        let work_api = dv_core::RepoLocation::Local(PathBuf::from(r"D:\work\api"));
        let side_api = dv_core::RepoLocation::Local(PathBuf::from(r"D:\side-projects\api"));
        assert_ne!(
            repo_group_key(&work_api),
            repo_group_key(&side_api),
            "two distinct repos sharing only a basename must not collapse into one group"
        );
    }

    #[test]
    fn repo_group_key_case_folds_local_but_not_wsl() {
        // Windows/macOS is case-insensitive+case-preserving: the same
        // repo can be recorded with two different-case full paths (e.g. a
        // stale `recent.json` seed vs. a freshly-typed path) — the key
        // must still land both in one group.
        assert_eq!(
            repo_group_key(&local("Difftest")),
            repo_group_key(&local("difftest")),
            "Local locations must key case-insensitively on the full path"
        );

        let wsl_lower = dv_core::RepoLocation::Wsl {
            distro: "Ubuntu".to_string(),
            path: "/home/kyle/difftest".to_string(),
        };
        let wsl_upper = dv_core::RepoLocation::Wsl {
            distro: "Ubuntu".to_string(),
            path: "/home/kyle/Difftest".to_string(),
        };
        assert_ne!(
            repo_group_key(&wsl_lower),
            repo_group_key(&wsl_upper),
            "WSL POSIX paths are genuinely case-sensitive — folding here would merge distinct repos"
        );

        // The distro name, unlike the POSIX path, IS case-insensitive at
        // the OS level — two entries for the same repo differing only in
        // distro-name spelling (e.g. a `recent.json` seed vs. a
        // `\\wsl.localhost\UBUNTU\...` Explorer path) must still land in
        // one group.
        let distro_upper = dv_core::RepoLocation::Wsl {
            distro: "UBUNTU".to_string(),
            path: "/home/kyle/difftest".to_string(),
        };
        assert_eq!(
            repo_group_key(&wsl_lower),
            repo_group_key(&distro_upper),
            "WSL distro names are case-insensitive — differing distro spelling must not split the group"
        );

        // A different POSIX path under the *same* (case-folded) distro
        // must still be a distinct group.
        let other_path_same_distro = dv_core::RepoLocation::Wsl {
            distro: "ubuntu".to_string(),
            path: "/home/kyle/other-repo".to_string(),
        };
        assert_ne!(
            repo_group_key(&wsl_lower),
            repo_group_key(&other_path_same_distro),
            "distinct POSIX paths under the same distro must still key differently"
        );
    }

    #[test]
    fn pr_group_key_disambiguates_by_full_slug_not_just_owner_repo_pr() {
        // Two different GitHub Enterprise hosts sharing an identical
        // owner/repo name and PR number must NOT collide into one key,
        // even though `repo_label`'s display string (host-stripped) would
        // be identical for both.
        let github_com = dv_core::RemoteRef {
            provider: "github".to_string(),
            slug: "github.com/acme/widgets".to_string(),
            pr: 7,
            url: "https://github.com/acme/widgets/pull/7".to_string(),
            submitted_review_id: None,
            submitted_url: None,
        };
        let ghe = dv_core::RemoteRef {
            slug: "ghe.internal.example.com/acme/widgets".to_string(),
            ..github_com.clone()
        };
        let loc = local("irrelevant"); // pr_group_key ignores location when remote is Some
        assert_ne!(
            pr_group_key(&loc, Some(&github_com)),
            pr_group_key(&loc, Some(&ghe)),
            "identical owner/repo/pr on two different hosts must key differently"
        );
    }

    #[test]
    fn pr_group_key_case_folds_the_slug_so_owner_repo_casing_drift_stays_one_group() {
        // Two reviews of the *same* PR, recorded while `origin`'s owner/repo
        // casing differed (e.g. before/after the remote URL was
        // retyped/normalized), must still land under one header.
        let mixed_case = dv_core::RemoteRef {
            provider: "github".to_string(),
            slug: "github.com/KyleKZ/difftest".to_string(),
            pr: 7,
            url: "https://github.com/KyleKZ/difftest/pull/7".to_string(),
            submitted_review_id: None,
            submitted_url: None,
        };
        let lower_case = dv_core::RemoteRef {
            slug: "github.com/kylekz/difftest".to_string(),
            url: "https://github.com/kylekz/difftest/pull/7".to_string(),
            ..mixed_case.clone()
        };
        let loc = local("irrelevant"); // pr_group_key ignores location when remote is Some
        assert_eq!(
            pr_group_key(&loc, Some(&mixed_case)),
            pr_group_key(&loc, Some(&lower_case)),
            "owner/repo casing drift between two reviews of the same PR must not split the group"
        );
    }

    #[test]
    fn status_group_label_covers_draft_and_every_verdict() {
        assert_eq!(status_group_label(&dv_core::ReviewState::Draft), "Draft");
        assert_eq!(
            status_group_label(&dv_core::ReviewState::Submitted {
                verdict: dv_core::Verdict::Comment,
                at_ms: 1,
            }),
            "Comment"
        );
        assert_eq!(
            status_group_label(&dv_core::ReviewState::Submitted {
                verdict: dv_core::Verdict::Approve,
                at_ms: 1,
            }),
            "Approved"
        );
        assert_eq!(
            status_group_label(&dv_core::ReviewState::Submitted {
                verdict: dv_core::Verdict::RequestChanges,
                at_ms: 1,
            }),
            "Changes Requested"
        );
    }

    #[cfg(feature = "automation")]
    #[test]
    fn grouping_word_matches_automation_set_setting_strings() {
        // `automation_set_setting`'s "sidebar_grouping" arm accepts exactly
        // these four strings back in — round-trip sanity so the two sides
        // of the wire format can't silently drift apart.
        assert_eq!(grouping_word(SidebarGrouping::None), "none");
        assert_eq!(grouping_word(SidebarGrouping::Repo), "repo");
        assert_eq!(grouping_word(SidebarGrouping::Status), "status");
        assert_eq!(grouping_word(SidebarGrouping::Pr), "pr");
    }
}
