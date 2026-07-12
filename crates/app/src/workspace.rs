use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use dv_core::{
    BlobSpec, ChangeStatus, ChangedFile, ChecksSummary, DiffOptions, DiffSource, FileDiff, GitRepo,
    GithubClient, LineKind, PrMeta, PrState, PrSummary, RemoteRef, RepoLocation, ReviewDecision,
};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::highlighter::HighlightTheme;
use gpui_component::{ActiveTheme, Disableable as _, StyledExt as _, h_flex, v_flex};

use crate::highlight::{self, LineRuns};
use crate::settings::{SUMMARY_WIDTH_MAX, SUMMARY_WIDTH_MIN, ViewModeSetting};
use crate::submit::{self, SubmissionOutcome, Violation, ViolationKind};

actions!(
    workspace,
    [
        NextFile,
        PrevFile,
        NextHunk,
        PrevHunk,
        ToggleSplit,
        ToggleSummary,
        JumpToFile,
        ClearSelection,
        CancelComment,
        PaletteNext,
        PalettePrev,
        PaletteClose,
        OpenPrPicker,
        PrPickerNext,
        PrPickerPrev,
        PrPickerClose,
        PrPickerChoose,
        RefreshPr
    ]
);

const KEY_CONTEXT: &str = "Workspace";
/// Stamped onto the workspace node while the inline comment editor is open
/// (same mechanism as [`PALETTE_CONTEXT`]) — single-char bindings must not
/// fire while the user types a comment.
const EDITOR_CONTEXT: &str = "EditorOpen";

/// Every diff row (lines and hunk headers alike) renders at exactly
/// [`row_height`] for the active `mono_font_size` setting. A uniform height
/// across row *kinds* is what keeps line backgrounds/thread-anchor
/// alignment gap-free — a variant taller than the rest (the old padded
/// headers) makes every other row sit short in its slot, leaving unpainted
/// gaps between line backgrounds.
///
/// This used to be a bare `24.` constant. Scaling it with `mono_font_size`
/// preserves the *exact* ratio that constant had against the old fixed
/// 14px diff text size (`settings::DEFAULT_MONO_FONT_SIZE`) — `24. / 14.`
/// — so a user who never touches the font-size setting sees byte-identical
/// layout to before this slice existed. `.max(16.)` keeps a row from
/// getting vanishingly short at the bottom of the settings panel's 8..=24
/// clamp.
fn row_height(font_size: f32) -> f32 {
    (font_size * (24. / 14.)).round().max(16.)
}
/// Width of each line-number gutter column — unified rows have two
/// (old/new), split cells have one per side. This used to be a bare
/// `w_12()` (Tailwind's `12 * 4px` = 48px), tuned for the old fixed 14px
/// diff text size but fixed regardless of the font-size setting — so a
/// 4-digit line number clipped at larger sizes (review finding). `3.5`
/// keeps size-14 visually unchanged for practical purposes (49px vs. the
/// old 48px) while giving size-24 enough room (84px) for 4 digits plus the
/// column's own padding.
fn gutter_width(font_size: f32) -> f32 {
    font_size * 3.5
}
/// Extra identifier stamped onto the workspace node while the jump-to-file
/// palette is open. Single-char bindings are scoped to `!PaletteOpen` so
/// they keep bubbling into the palette's text input instead of firing.
const PALETTE_CONTEXT: &str = "PaletteOpen";
/// Same mechanism as [`PALETTE_CONTEXT`], for the PR picker (`ctrl-g`).
const PR_PICKER_CONTEXT: &str = "PrPickerOpen";

pub fn init(cx: &mut App) {
    let browse = Some("Workspace && !PaletteOpen && !EditorOpen && !PrPickerOpen");
    let palette = Some("Workspace && PaletteOpen");
    let editor = Some("Workspace && EditorOpen");
    let pr_picker = Some("Workspace && PrPickerOpen");
    cx.bind_keys([KeyBinding::new("escape", CancelComment, editor)]);
    cx.bind_keys([
        KeyBinding::new("j", NextFile, browse),
        KeyBinding::new("down", NextFile, browse),
        KeyBinding::new("k", PrevFile, browse),
        KeyBinding::new("up", PrevFile, browse),
        KeyBinding::new("n", NextHunk, browse),
        KeyBinding::new("p", PrevHunk, browse),
        KeyBinding::new("s", ToggleSplit, browse),
        KeyBinding::new("r", ToggleSummary, browse),
        KeyBinding::new("f", JumpToFile, browse),
        KeyBinding::new("ctrl-p", JumpToFile, browse),
        KeyBinding::new("cmd-p", JumpToFile, browse),
        KeyBinding::new("ctrl-g", OpenPrPicker, browse),
        KeyBinding::new("escape", ClearSelection, browse),
        KeyBinding::new("down", PaletteNext, palette),
        KeyBinding::new("up", PalettePrev, palette),
        KeyBinding::new("escape", PaletteClose, palette),
        KeyBinding::new("down", PrPickerNext, pr_picker),
        KeyBinding::new("up", PrPickerPrev, pr_picker),
        KeyBinding::new("escape", PrPickerClose, pr_picker),
        KeyBinding::new("enter", PrPickerChoose, pr_picker),
    ]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Unified,
    Split,
}

impl From<ViewModeSetting> for ViewMode {
    fn from(setting: ViewModeSetting) -> Self {
        match setting {
            ViewModeSetting::Unified => ViewMode::Unified,
            ViewModeSetting::Split => ViewMode::Split,
        }
    }
}

enum Status {
    Loading,
    Ready,
    Failed(String),
}

/// One renderable row of the diff pane, precomputed so the uniform_list
/// closure stays trivial.
enum Row {
    HunkHeader {
        label: SharedString,
        /// Index into the file's hunk list — identifies which gap a click
        /// expands.
        hunk: usize,
        /// `Some(n)` ⇒ n hidden context lines sit between the previous hunk
        /// (or file start) and this one, and clicking reveals them.
        expandable: Option<u32>,
    },
    Line {
        kind: LineKind,
        old_line: Option<u32>,
        new_line: Option<u32>,
        text: SharedString,
        /// Merged syntax + intraline style runs, line-relative. Empty ⇒
        /// render the text plainly.
        runs: Vec<(Range<usize>, HighlightStyle)>,
    },
    Binary,
    NoChanges,
}

/// One side's cell in a side-by-side row.
#[derive(Clone)]
struct SplitCell {
    kind: LineKind,
    line: Option<u32>,
    text: SharedString,
    runs: Vec<(Range<usize>, HighlightStyle)>,
}

/// One renderable row of the split (side-by-side) view. A `Pair` holds the
/// old side on the left and the new side on the right; either can be absent
/// (a pure add/delete leaves the opposite side blank).
enum SplitRow {
    HunkHeader {
        label: SharedString,
        hunk: usize,
        expandable: Option<u32>,
    },
    Pair {
        left: Option<SplitCell>,
        right: Option<SplitCell>,
    },
    Binary,
    NoChanges,
}

/// Colors resolved from the live theme on the UI thread and handed to the
/// off-thread diff computation (which has no `cx`).
#[derive(Clone)]
struct HighlightInputs {
    theme: Arc<HighlightTheme>,
    intra_added: Hsla,
    intra_removed: Hsla,
}

struct RenderedDiff {
    unified: Vec<Row>,
    split: Vec<SplitRow>,
    /// Row index where each hunk starts (its header — or, once its gap is
    /// expanded, its first revealed line), per view. n/p jump through these.
    hunk_rows_unified: Vec<usize>,
    hunk_rows_split: Vec<usize>,
    /// This "diff" is a cached failure message; re-selecting the file
    /// retries instead of pinning the error forever.
    error: bool,
}

/// Which side of the diff a line (and so a comment anchor) lives on.
/// Mirrors dv-core's review-side notion; unified rows resolve to New when
/// the line exists there, Old only for pure removals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffSide {
    Old,
    New,
}

/// An in-progress line/range selection made from the gutter — the precursor
/// to a comment. `anchor` is where the press started; `head` follows the
/// drag (or shift-click), so the range is unordered until read.
#[derive(Debug, Clone, Copy)]
struct GutterSelection {
    file: usize,
    side: DiffSide,
    anchor: u32,
    head: u32,
    /// Mouse button still down — rows extend the range on hover.
    dragging: bool,
}

impl GutterSelection {
    fn range(&self) -> (u32, u32) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    fn contains(&self, side: DiffSide, line: u32) -> bool {
        let (lo, hi) = self.range();
        self.side == side && (lo..=hi).contains(&line)
    }
}

/// One row of the diff pane as displayed: the precomputed diff rows with
/// comment threads (and the comment editor) interleaved under their anchor
/// lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayRow {
    /// Index into the RenderedDiff rows for the active view mode.
    Diff(usize),
    /// Index into the current review's comments.
    Thread(usize),
    /// The inline comment editor.
    Editor,
}

/// The inline comment editor, open under the gutter selection.
struct CommentEditor {
    input: Entity<gpui_component::input::InputState>,
    saving: bool,
    _subscription: Subscription,
}

/// What a thread's inline input is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadInputMode {
    Reply,
    EditBody,
}

/// Which threads the summary panel lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryFilter {
    All,
    Open,
    Resolved,
}

/// Zero-sized `on_drag`/`on_drag_move` payload tag for the review-summary
/// panel's resize handle (see `render_summary_resize_handle`) — see
/// `shell::SidebarResizeDrag`'s doc comment for why the sidebar's handle
/// needs its own distinct tag rather than sharing this one.
#[derive(Clone)]
struct SummaryResizeDrag;

/// A reply/edit input open inside one thread card. At most one across the
/// workspace — opening another closes this one.
struct ThreadInput {
    comment_id: String,
    mode: ThreadInputMode,
    input: Entity<gpui_component::input::InputState>,
    saving: bool,
    _subscription: Subscription,
}

/// The jump-to-file palette, while open: a text input plus a live-filtered
/// view of the file list.
struct Palette {
    input: Entity<gpui_component::input::InputState>,
    /// Indices into `files`, best match first.
    matches: Vec<usize>,
    /// Cursor within `matches`.
    selected: usize,
    _subscription: Subscription,
}

/// Everything the PR header panel needs, cached from the [`PrMeta`]
/// [`Workspace::open_pr`] last fetched — the workspace never re-hits `gh`
/// just to render the band.
struct PrHeader {
    number: u64,
    title: SharedString,
    author: SharedString,
    state: PrState,
    is_draft: bool,
    base_ref: SharedString,
    head_ref: SharedString,
    checks: ChecksSummary,
    review_decision: Option<ReviewDecision>,
    url: SharedString,
    body: SharedString,
}

impl From<PrMeta> for PrHeader {
    /// Everything the header band shows, straight from a fresh `pr_meta`
    /// fetch — shared by `open_pr`, `start_submit_validation`, and
    /// `refresh_pr` (review finding P3-b) so the three call sites can't
    /// drift and each keeps the header in sync with whatever `gh` just
    /// said, rather than only ever refreshing on the original `open_pr`.
    fn from(meta: PrMeta) -> Self {
        Self {
            number: meta.number,
            title: meta.title.into(),
            author: meta.author.into(),
            state: meta.state,
            is_draft: meta.is_draft,
            base_ref: meta.base_ref.into(),
            head_ref: meta.head_ref.into(),
            checks: meta.checks,
            review_decision: meta.review_decision,
            url: meta.url.into(),
            body: meta.body.into(),
        }
    }
}

/// The PR picker overlay (`ctrl-g`), while open.
struct PrPicker {
    state: PrPickerState,
    /// Cursor into `state`'s list, once loaded.
    selected: usize,
}

enum PrPickerState {
    Loading,
    Loaded(Vec<PrSummary>),
    Error(String),
}

/// Everything a PR's background open resolves, applied to the workspace in
/// one shot on the UI thread once it lands.
struct PrOpenOutcome {
    meta: PrMeta,
    files: Vec<ChangedFile>,
    review: dv_core::Review,
    source: DiffSource,
}

/// Everything [`Workspace::on_submit_click`] needs to actually send the
/// review, computed once by [`Workspace::start_submit_validation`] and
/// carried from [`SubmitFlow::Confirming`] into the background submit task
/// — Submit itself never re-validates, matching docs/phase-3-github.md
/// deliverable 2 ("NOTHING is sent until Submit is clicked", not
/// "re-checked then sent").
#[derive(Clone)]
struct SubmitPrep {
    /// The review this submission was validated against, captured once
    /// here — `on_submit_click`'s background task and the writeback always
    /// fresh-load BY THIS id, never by whatever `self.review` happens to be
    /// at click/completion time (review finding P1-2, traced to a real
    /// cross-PR store corruption: a stale `self.review` read at click time
    /// could name a *different* review — even one linked to a different
    /// PR — than the one `submission` below was actually built from, and
    /// the local writeback would then mark that unrelated review
    /// Submitted/PR-linked while the review whose comments were truly just
    /// POSTed never gets recorded as sent at all).
    review_id: String,
    pr_number: u64,
    pr_url: String,
    submission: dv_core::ReviewSubmission,
}

/// GitHub submission flow (docs/phase-3-github.md deliverable 2), live only
/// once a verdict is clicked on a review whose [`dv_core::Review::remote`]
/// names a PR. `None` — the field's default and its state after every
/// terminal stage is dismissed — means "no flow active": a local-only
/// review's verdict click always goes straight to [`Workspace::submit_review`]
/// (the pre-existing local finish) regardless of this field.
enum SubmitFlow {
    /// Background validation in flight: fresh `pr_meta` + `prepare_pr` +
    /// `crate::submit::build_submission`. No `gh` write has happened yet.
    Validating { verdict: dv_core::Verdict },
    /// Validation found problems — a stale/unanchored comment, or one that
    /// fell outside the PR's diff. Nothing was sent; nothing can be until
    /// the underlying comments (or the PR itself) change and the verdict is
    /// clicked again.
    Blocked {
        verdict: dv_core::Verdict,
        violations: Vec<Violation>,
    },
    /// Clean: the submission is built and waiting on an explicit
    /// [Submit to GitHub] click. Still nothing sent.
    Confirming {
        verdict: dv_core::Verdict,
        prep: SubmitPrep,
    },
    /// The `gh api .../reviews` POST (plus local writeback) is in flight.
    /// Unlike every other stage, this one is not cancellable — see
    /// [`Workspace::cancel_submit_flow`] — and blocks a source switch the
    /// same way an in-flight comment save does (see `open_pr`/`select_file`).
    Submitting { verdict: dv_core::Verdict },
    /// GitHub accepted the review and the local writeback succeeded.
    Done {
        verdict: dv_core::Verdict,
        url: String,
    },
    /// Either the `gh` submission itself failed, or (rarer, and far worse)
    /// it succeeded but the local writeback then failed — `message` is
    /// `crate::submit::writeback_failure_message`'s text in that case, which
    /// warns against ever retrying.
    Failed {
        verdict: dv_core::Verdict,
        message: String,
    },
}

/// [`Workspace::start_submit_validation`]'s completion, as a pure function
/// of what validation produced — kept separate from the `cx.spawn` plumbing
/// around it so the three outcomes (clean/blocked/hard-error) are
/// unit-testable without a `Context` (see the `tests` module at the bottom
/// of this file).
fn submit_flow_from_validation(
    verdict: dv_core::Verdict,
    review_id: String,
    outcome: anyhow::Result<(PrMeta, SubmissionOutcome)>,
) -> SubmitFlow {
    match outcome {
        Ok((meta, SubmissionOutcome::Ready(submission))) => SubmitFlow::Confirming {
            verdict,
            prep: SubmitPrep {
                review_id,
                pr_number: meta.number,
                pr_url: meta.url,
                submission,
            },
        },
        Ok((_, SubmissionOutcome::Blocked(violations))) => SubmitFlow::Blocked {
            verdict,
            violations,
        },
        Err(err) => SubmitFlow::Failed {
            verdict,
            message: format!("{err:#}"),
        },
    }
}

/// [`Workspace::on_submit_click`]'s completion, as a pure function of the
/// background submit+writeback's result — `Some(Review)` only on success
/// (the caller applies it and emits `ReviewChanged`; a failure leaves the
/// workspace's existing `review` alone).
fn submit_flow_from_submission(
    verdict: dv_core::Verdict,
    result: Result<(dv_core::Review, String), String>,
) -> (Option<dv_core::Review>, SubmitFlow) {
    match result {
        Ok((review, url)) => (Some(review), SubmitFlow::Done { verdict, url }),
        Err(message) => (None, SubmitFlow::Failed { verdict, message }),
    }
}

/// [`Workspace::cancel_submit_flow`]'s decision, as a pure function:
/// `Submitting` must run to completion (returned unchanged, `changed =
/// false`, `submit_epoch` untouched — there's nothing to invalidate);
/// every other `Some` resets to `None` AND bumps `submit_epoch` (review
/// finding P1-1: a `Validating` completion spawned before the bump must
/// discard itself on landing rather than resurrect a flow the user just
/// cancelled, or — worse — clobber a newer flow already in progress); a
/// `None` input leaves the epoch alone too (nothing was cancelled). The
/// caller only calls `cx.notify()` when `changed` is true.
fn cancel_submit_flow_outcome(
    flow: Option<SubmitFlow>,
    epoch: u64,
) -> (Option<SubmitFlow>, bool, u64) {
    if matches!(flow, Some(SubmitFlow::Submitting { .. })) {
        (flow, false, epoch)
    } else {
        let changed = flow.is_some();
        let epoch = if changed { epoch + 1 } else { epoch };
        (None, changed, epoch)
    }
}

pub struct Workspace {
    focus_handle: FocusHandle,
    source: DiffSource,
    /// Bumped every time [`Self::open_pr`] is dispatched — never on a plain
    /// file switch. `open_pr` and `request_diff` completions capture the
    /// value in force at spawn time and discard themselves (before
    /// touching any state) if it no longer matches on completion: a stale
    /// background fetch/diff computed against a source that's since been
    /// replaced by a newer `open_pr` (review finding P1-a).
    source_epoch: u64,
    status: Status,
    repo: Option<Arc<GitRepo>>,
    title: SharedString,
    head: SharedString,
    files: Vec<ChangedFile>,
    selected: Option<usize>,
    diffs: HashMap<usize, Arc<RenderedDiff>>,
    diff_pending: HashSet<usize>,
    /// Bumped by [`Self::invalidate_diff_cache`] (theme swaps via
    /// [`Self::on_theme_changed`], and context-lines changes via
    /// [`Self::set_context_lines`] — anything that requires a full
    /// recompute, not just a re-render). `request_diff` resolves
    /// `cx.theme().highlight_theme` once, off-thread, and bakes concrete
    /// colors into the cached `RenderedDiff` rows for performance — so
    /// unlike UI chrome (which reads `cx.theme()` fresh every render), a
    /// cached diff does *not* pick up a new theme's syntax palette (or a
    /// new context-lines count) on its own. Captured at spawn time alongside
    /// `source_epoch` and checked on completion, so a diff computed against
    /// settings that have since changed again (two quick picker/panel
    /// choices) never clobbers a newer computation that already landed.
    highlight_epoch: u64,
    view_mode: ViewMode,
    /// Context lines shown around each hunk (`dv_core::DiffOptions`'s
    /// existing knob) — from `settings::Settings::context_lines`, live via
    /// [`Self::set_context_lines`].
    context_lines: u32,
    /// Mono/code text size in px — from `settings::Settings::mono_font_size`,
    /// live via [`Self::set_font_size`]. Also drives row height (see
    /// `row_height`); never baked into a cached [`RenderedDiff`] (only
    /// colors are), so a change just needs a re-measure + re-render, not a
    /// recompute.
    font_size: f32,
    /// Captured from the *original* source, before `resolve_source` rewrites
    /// a merge-base range to a plain two-dot range — so the header keeps
    /// saying "range (merge base)".
    source_desc: SharedString,
    /// Which hunk n/p last jumped to in the selected file.
    current_hunk: usize,
    /// Per file: hunk indices whose preceding context gap has been expanded
    /// (click on the hunk header). Feeds row rebuilding.
    expanded: HashMap<usize, HashSet<usize>>,
    file_scroll: UniformListScrollHandle,
    /// The diff pane is a gpui `list` (not uniform_list): comment threads
    /// render inline under their anchor rows at whatever height their
    /// markdown needs, so row heights must be measured, not assumed.
    diff_list: ListState,
    palette: Option<Palette>,
    /// Live gutter selection (comment anchor being chosen).
    selection: Option<GutterSelection>,
    /// A reply/edit input open inside a thread card, if any.
    thread_input: Option<ThreadInput>,
    /// Review summary panel (all threads across files + verdict) open?
    summary_open: bool,
    summary_filter: SummaryFilter,
    /// Summary panel width, in px — from `settings::Settings::summary_width`,
    /// live during a drag of `render_summary_resize_handle`'s handle (which
    /// mutates this directly, for immediate visual feedback) and pushed
    /// down externally by [`Self::set_summary_width_external`] (settings
    /// panel / `set_setting`). Persistence is the *shell's* job (it owns
    /// `Settings`) — a drag-release emits [`SummaryWidthChanged`] rather
    /// than writing settings.json from here.
    summary_width: f32,
    /// True while a summary-handle drag is in progress (set by the first
    /// drag-move frame). Gates the mouse-up emit — `on_mouse_up_out` fires
    /// on ANY outside left release, which would otherwise persist settings
    /// on every click in the window.
    summary_dragging: bool,
    /// A comment to scroll to once its file's rows exist — set by summary
    /// clicks, consumed by reset_diff_list.
    pending_jump: Option<String>,
    /// Comment ids whose anchored blob no longer matches the diff — the
    /// content drifted (rebase/amend/edit) since the comment was made.
    stale: HashSet<String>,
    /// (file, review.updated_ms) the stale set was last computed for, so
    /// repeated display rebuilds don't re-run git.
    stale_checked: Option<(usize, u64)>,
    /// The repo location, for opening the review store off-thread.
    location: RepoLocation,
    /// The active draft review (latest draft in the store), lazily loaded.
    review: Option<dv_core::Review>,
    editor: Option<CommentEditor>,
    /// Diff rows + interleaved threads/editor, in display order. The list
    /// element renders these; rebuilt by [`Self::rebuild_display`].
    display: Vec<DisplayRow>,
    /// diff row index → display row index, for hunk scrolling.
    diff_to_display: Vec<usize>,
    /// Wall-clock of the most recent per-file diff computation (blob fetch
    /// + diff + highlight), for `--automation` perf validation.
    last_diff_ms: Option<u64>,
    /// Keeps the review-store watcher alive; external edits (agent CLI,
    /// another window) stream in through it. Dropped with the workspace.
    _watcher: Option<dv_core::ReviewWatcher>,
    /// Keeps a worktree watch alive (plan §6/S4) — a HOST-ONLY capability,
    /// so `None` for a `Local` repo, a disabled/no-host WSL session, or
    /// once the source has switched away from `WorkingTree` (`open_pr`
    /// tears it down explicitly; there's no natural way back to
    /// `WorkingTree` from a PR in this codebase today, so nothing re-sets
    /// it). See [`Self::automation_state`]'s `worktree_watch` bool for the
    /// automation-visible signal this exists.
    _worktree_watcher: Option<dv_core::remote::WorktreeWatcher>,
    /// The PR this workspace's diff source was opened from, if any (set by
    /// [`Self::open_pr`]). `None` for a plain local review.
    pr: Option<PrHeader>,
    /// The remote linkage (provider/slug/pr) of the currently-open PR, if
    /// any — copied from the review [`load_pr`] found-or-created for it.
    /// Kept separately from `pr` (which has no slug) so [`pick_review`] can
    /// tell whether the workspace is PR-scoped and, if so, to which PR
    /// (review finding P1-b: the watcher must not adopt an unrelated PR's
    /// newest draft out from under an open PR).
    pr_remote: Option<RemoteRef>,
    /// Whether the PR header's "details" (body) toggle is expanded.
    pr_details_open: bool,
    /// Set (to the PR number) while [`Self::open_pr`] is fetching — reused
    /// only for the loading caption; `Status::Loading` already gates
    /// `--automation`'s `wait_ready`/`state`.
    pr_loading: Option<u64>,
    /// The most recent `open_pr` failure, if any. Deliberately separate
    /// from `Status::Failed`: that variant blanks the whole pane, but a
    /// failed PR fetch must leave the workspace exactly as usable as it was
    /// on its previous source.
    pr_error: Option<String>,
    /// The PR-picker overlay (`ctrl-g`), when open.
    pr_picker: Option<PrPicker>,
    /// Comment/reply author, resolved once in the background at load
    /// (`crate::author::resolve_author`). `None` until that resolves —
    /// callers fall back to a placeholder rather than block on it.
    author: Option<String>,
    /// GitHub submission flow state (docs/phase-3-github.md deliverable 2),
    /// live only after a verdict click on a PR-linked review. See
    /// [`SubmitFlow`]'s doc comment.
    submit: Option<SubmitFlow>,
    /// Bumped on every `self.submit`-transition initiated from the UI
    /// thread — entering the flow ([`Self::on_verdict_clicked`] via
    /// [`Self::start_submit_validation`]), leaving it early
    /// ([`Self::cancel_submit_flow`]), and the one internal transition
    /// mid-flow ([`Self::on_submit_click`]'s Confirming→Submitting).
    /// `start_submit_validation`'s and `on_submit_click`'s background
    /// completions each capture the value in force at spawn and, before
    /// touching any state, verify it still matches — a mismatch means a
    /// newer transition (cancel, a later verdict click, `open_pr`) has
    /// since superseded them, and they discard themselves rather than
    /// resurrect a dead flow or clobber a newer one (review finding P1-1).
    /// Mirrors `source_epoch`'s exact same pattern for `open_pr`/
    /// `request_diff`.
    submit_epoch: u64,
}

/// Which blob a comment on `side` of `path` anchors to, given the review's
/// (resolved) diff source. The CLI carries the same mapping; keep in sync
/// until it moves into dv-core (backlog).
fn anchor_spec(source: &DiffSource, side: dv_core::Side, path: &str) -> BlobSpec {
    match (side, source) {
        (dv_core::Side::Old, DiffSource::WorkingTree | DiffSource::Staged) => BlobSpec::Rev {
            rev: "HEAD".into(),
            path: path.into(),
        },
        (dv_core::Side::Old, DiffSource::Range { base, .. }) => BlobSpec::Rev {
            rev: base.clone(),
            path: path.into(),
        },
        (dv_core::Side::Old, DiffSource::Commit(sha)) => BlobSpec::Rev {
            rev: format!("{sha}^"),
            path: path.into(),
        },
        (dv_core::Side::New, DiffSource::WorkingTree) => BlobSpec::Working { path: path.into() },
        (dv_core::Side::New, DiffSource::Staged) => BlobSpec::Index { path: path.into() },
        (dv_core::Side::New, DiffSource::Range { head, .. }) => BlobSpec::Rev {
            rev: head.clone(),
            path: path.into(),
        },
        (dv_core::Side::New, DiffSource::Commit(sha)) => BlobSpec::Rev {
            rev: sha.clone(),
            path: path.into(),
        },
    }
}

/// Which review the workspace should display, from a store listing
/// (newest-first).
///
/// When the workspace has a PR open (`current_pr` is `Some`), a draft
/// created by a completely unrelated `dv comment add` (e.g. the CLI writing
/// against some other PR's review) must never hijack the display just for
/// being newest — review finding P1-b, reproduced live: the watcher's old
/// "newest draft wins" rule swapped a PR-A workspace onto PR-B's
/// freshly-created draft, and subsequent GUI comments then saved into the
/// wrong PR's review. So with a PR open, selection is: (1) the draft (or,
/// failing that, any review) whose `remote` matches this PR (slug compared
/// case-insensitively, matching main.rs's origin/URL slug comparison), else
/// (2) keep showing whatever is already on screen (`current_id`) — never
/// fall back to an unrelated draft.
///
/// With no PR open (`current_pr` is `None`), the original local-review
/// precedence applies unchanged: the latest draft (where comments
/// accumulate — matches the CLI's targeting), else the review already on
/// screen (so a just-submitted review doesn't vanish), else the latest
/// review of any state (so submitted work is still visible after a
/// restart).
fn pick_review(
    reviews: Vec<dv_core::Review>,
    current_id: Option<&str>,
    current_pr: Option<&RemoteRef>,
) -> Option<dv_core::Review> {
    if let Some(pr) = current_pr {
        let matches_pr = |r: &&dv_core::Review| {
            r.remote.as_ref().is_some_and(|remote| {
                remote.pr == pr.pr && remote.slug.eq_ignore_ascii_case(&pr.slug)
            })
        };
        if let Some(matched) = reviews.iter().find(matches_pr) {
            return Some(matched.clone());
        }
        return current_id
            .and_then(|id| reviews.iter().find(|r| r.id == id))
            .cloned();
    }

    if let Some(draft) = reviews
        .iter()
        .find(|r| matches!(r.state, dv_core::ReviewState::Draft))
    {
        return Some(draft.clone());
    }
    if let Some(id) = current_id
        && let Some(current) = reviews.iter().find(|r| r.id == id)
    {
        return Some(current.clone());
    }
    reviews.into_iter().next()
}

/// A one-row "diff" carrying an error message where the hunks would be.
fn error_diff(msg: SharedString) -> RenderedDiff {
    RenderedDiff {
        unified: vec![Row::HunkHeader {
            label: msg.clone(),
            hunk: 0,
            expandable: None,
        }],
        split: vec![SplitRow::HunkHeader {
            label: msg,
            hunk: 0,
            expandable: None,
        }],
        hunk_rows_unified: Vec::new(),
        hunk_rows_split: Vec::new(),
        error: true,
    }
}

/// Trim trailing newline(s) from a comment/reply body before it's
/// persisted (review finding P3-a, proven live: a GUI-saved body ended
/// `"...here.\n"`). `ctrl-enter` (secondary `PressEnter`) races the
/// multi-line input's own newline-on-Enter handling, so the value read at
/// submit time can carry one or more trailing `\n` the user never meant
/// to type — the CLI's bodies never have one. Trailing *newlines* only:
/// a whole-body `trim_end()` would also eat intentional trailing spaces
/// (e.g. inside a fenced code block), which this must leave alone.
fn trim_trailing_newlines(body: String) -> String {
    body.trim_end_matches('\n').to_string()
}

fn source_label(source: &DiffSource) -> &'static str {
    match source {
        DiffSource::WorkingTree => "working tree",
        DiffSource::Staged => "staged",
        DiffSource::Range {
            merge_base: false, ..
        } => "range",
        DiffSource::Range {
            merge_base: true, ..
        } => "range (merge base)",
        DiffSource::Commit(_) => "commit",
    }
}

/// String forms of the GitHub types [`Workspace::automation_state`] dumps —
/// small, deliberately duplicated copies of `cli/pr_cmd.rs`'s private
/// equivalents (that module isn't `pub`, and these are one match arm each).
/// `pub(crate)` so `shell.rs`'s sidebar badge dump (deliverable 3) reuses
/// these instead of growing a third copy — that module isn't a descendant
/// of this one the way `cli::pr_cmd` fails to be, so plain visibility is
/// enough.
pub(crate) fn pr_state_word(state: PrState) -> &'static str {
    match state {
        PrState::Open => "open",
        PrState::Closed => "closed",
        PrState::Merged => "merged",
    }
}

pub(crate) fn checks_word(checks: ChecksSummary) -> &'static str {
    match checks {
        ChecksSummary::Passing => "passing",
        ChecksSummary::Failing => "failing",
        ChecksSummary::Pending => "pending",
        ChecksSummary::None => "none",
    }
}

pub(crate) fn review_decision_word(decision: ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::Approved => "approved",
        ReviewDecision::ChangesRequested => "changes_requested",
        ReviewDecision::ReviewRequired => "review_required",
    }
}

/// Human-readable verdict word for UI text — the wording the "Submitted
/// · ..." caption always used, now shared with the Phase-3 submit-flow
/// panels too.
fn verdict_label(verdict: dv_core::Verdict) -> &'static str {
    match verdict {
        dv_core::Verdict::Comment => "comment",
        dv_core::Verdict::Approve => "approve",
        dv_core::Verdict::RequestChanges => "request changes",
    }
}

/// snake_case verdict word for `--automation`'s JSON, matching
/// `cli/pr_cmd.rs`'s `event_word` convention (machine-parsed, so no space).
fn verdict_automation_word(verdict: dv_core::Verdict) -> &'static str {
    match verdict {
        dv_core::Verdict::Comment => "comment",
        dv_core::Verdict::Approve => "approve",
        dv_core::Verdict::RequestChanges => "request_changes",
    }
}

/// snake_case word for a [`ViolationKind`], for `--automation`'s JSON.
fn violation_kind_word(kind: ViolationKind) -> &'static str {
    match kind {
        ViolationKind::StaleAnchor => "stale_anchor",
        ViolationKind::Unanchored => "unanchored",
        ViolationKind::RenameUnverifiable => "rename_unverifiable",
        ViolationKind::NotInDiff => "not_in_diff",
        ViolationKind::NothingToSubmit => "nothing_to_submit",
    }
}

/// Whether `review` is the review `load_pr`'s find-or-create should adopt
/// as the active review for PR `pr` on `slug`. Slug comparison is
/// case-insensitive, matching main.rs's origin/URL slug comparison.
///
/// Deliberately requires `Draft` state (review finding P1, proven live):
/// without this, re-opening a PR whose review was already **submitted**
/// would "adopt" that submitted review as the mutable active one, and the
/// very next gutter comment would append into it — stranded, since the
/// verdict bar hides comments on a submitted review and the CLI refuses
/// to touch one. A submitted review matching this PR must be treated as
/// no match at all, so the caller falls through to creating a fresh
/// draft (linked to the same PR, `submitted_review_id`/`submitted_url`
/// left `None`). This does NOT affect [`pick_review`]'s PR-arm, which
/// intentionally still matches a submitted review for *display* (the
/// Done panel case) — only this find-or-create's *adoption for further
/// mutation* is restricted to drafts.
fn review_adopts_pr(review: &dv_core::Review, slug: &str, pr: u64) -> bool {
    matches!(review.state, dv_core::ReviewState::Draft)
        && review
            .remote
            .as_ref()
            .is_some_and(|remote| remote.pr == pr && remote.slug.eq_ignore_ascii_case(slug))
}

/// Fetch a PR's metadata, make sure its diff range is available locally,
/// reload the changed-file list against it, and find-or-create the draft
/// review it links to — everything [`Workspace::open_pr`] needs, done
/// off-thread in one shot so the UI only ever sees a finished outcome.
fn load_pr(repo: &GitRepo, number: u64, location: RepoLocation) -> anyhow::Result<PrOpenOutcome> {
    let client = GithubClient::for_repo(repo)?;
    client.preflight()?;
    let meta = client.pr_meta(number)?;
    let range = crate::pr::prepare_pr(repo, &meta)?;
    // `range.merge_base` is already the resolved merge-base tip — build the
    // concrete two-dot source directly rather than routing back through
    // `resolve_source`'s `merge_base: true` path (which would just
    // recompute the same `git merge-base` call).
    let source = DiffSource::Range {
        base: range.merge_base.clone(),
        head: range.head_oid.clone(),
        merge_base: false,
    };
    let files = repo.changed_files(&source)?;

    let store = dv_core::ReviewStore::open(location);
    let slug = client.slug().to_string();
    // Case-insensitive, matching main.rs's `resolve_pr_target` slug
    // comparison — a host/owner/repo differing only in case is the same
    // repo on GitHub. Only a Draft review is eligible for adoption here —
    // see `review_adopts_pr` (review finding P1).
    let existing = store
        .list()?
        .into_iter()
        .find(|r| review_adopts_pr(r, &slug, number));
    let review = match existing {
        Some(review) => review,
        None => {
            // Re-list immediately before creating: closes the TOCTOU window
            // against a concurrent creator (another `dv` process, or a CLI
            // invocation racing this fetch) that also saw "no existing"
            // from the list() above and would otherwise dupe the draft.
            // The GUI itself can no longer race here (open_pr now refuses a
            // second concurrent open — review finding P2-b), but this is a
            // cheap belt-and-suspenders check against future regressions
            // or an out-of-process writer.
            let recheck = store
                .list()?
                .into_iter()
                .find(|r| review_adopts_pr(r, &slug, number));
            match recheck {
                Some(review) => review,
                None => {
                    // A fresh draft, linked to the PR from the first click —
                    // so comments accumulate against it immediately,
                    // matching how a local review already targets the
                    // latest draft by default.
                    let mut review = store.create(source.clone())?;
                    review.remote = Some(RemoteRef {
                        provider: "github".to_string(),
                        slug,
                        pr: number,
                        url: meta.url.clone(),
                        submitted_review_id: None,
                        submitted_url: None,
                    });
                    store.save(&review)?;
                    review
                }
            }
        }
    };

    Ok(PrOpenOutcome {
        meta,
        files,
        review,
        source,
    })
}

/// Worktree-watch reconciliation (review finding P1-1): a `changed_files`
/// refresh driven by the worktree watcher may return a file list that has
/// shrunk, grown, or simply reordered relative to what's on screen — and
/// `selected`/`diffs`/`diff_pending`/`expanded` are all keyed by INDEX into
/// that list, not by path. Blindly swapping the list in without remapping
/// the selected index either panics (`self.files[old_index]` once the list
/// has shrunk past it — the original crash: an external `git commit`
/// emptying a 3-file list while file 2 was selected) or silently renders
/// the wrong file's cached diff (a new file sorting earlier shifts every
/// later index up by one, so the SAME index now names a DIFFERENT file).
///
/// Returns `(new_selected, needs_invalidation)`:
/// - `new_selected` is the old selection's path, relocated in `new_files`,
///   or `None` if that file no longer exists there (the caller falls back
///   to index 0 when the list is non-empty, or clears the diff area
///   cleanly when it's empty).
/// - `needs_invalidation` is `false` only in the genuinely unchanged case
///   (same paths in the same order — every existing index still names the
///   same file, so cached per-index state stays valid); `true` any other
///   time an index might now point at a different file than it used to.
fn reconcile_file_selection(
    old_files: &[ChangedFile],
    new_files: &[ChangedFile],
    old_selected: Option<usize>,
) -> (Option<usize>, bool) {
    let stable = old_files.len() == new_files.len()
        && old_files
            .iter()
            .zip(new_files)
            .all(|(a, b)| a.path == b.path);
    if stable {
        return (old_selected, false);
    }
    let new_selected = old_selected
        .and_then(|i| old_files.get(i))
        .and_then(|old| new_files.iter().position(|f| f.path == old.path));
    (new_selected, true)
}

impl Workspace {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        location: RepoLocation,
        source: DiffSource,
        pending_pr: Option<u64>,
        view_mode_default: ViewModeSetting,
        context_lines: u32,
        font_size: f32,
        summary_width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Watch the review store so agent-CLI (or other-window) edits show
        // up live. The channel is created now, but the watcher itself only
        // starts once the repo has loaded: the store must live at the repo
        // TOPLEVEL (GitRepo::open normalizes), not at whatever subdirectory
        // dv was pointed at — otherwise the GUI and CLI silently use two
        // different stores.
        let (watch_tx, mut watch_rx) = futures::channel::mpsc::unbounded::<()>();
        // Same "channel now, watcher once the repo's loaded" shape as
        // `watch_tx`/`watch_rx` above, for the worktree watch (plan §6/S4)
        // — only ever actually subscribed for a WSL repo with a
        // watch-capable host and a `WorkingTree` source (see the load
        // completion below and `_worktree_watcher`'s doc comment).
        let (worktree_tx, mut worktree_rx) = futures::channel::mpsc::unbounded::<()>();

        let this = Self {
            focus_handle: cx.focus_handle(),
            title: location.display_name().into(),
            source_desc: source_label(&source).into(),
            location: location.clone(),
            review: None,
            editor: None,
            display: Vec::new(),
            diff_to_display: Vec::new(),
            source: source.clone(),
            source_epoch: 0,
            highlight_epoch: 0,
            status: Status::Loading,
            repo: None,
            head: "".into(),
            files: Vec::new(),
            selected: None,
            diffs: HashMap::new(),
            diff_pending: HashSet::new(),
            view_mode: view_mode_default.into(),
            context_lines,
            font_size,
            current_hunk: 0,
            expanded: HashMap::new(),
            file_scroll: UniformListScrollHandle::new(),
            diff_list: ListState::new(0, ListAlignment::Top, px(600.)),
            palette: None,
            selection: None,
            thread_input: None,
            summary_open: false,
            summary_filter: SummaryFilter::All,
            summary_width,
            summary_dragging: false,
            pending_jump: None,
            stale: HashSet::new(),
            stale_checked: None,
            last_diff_ms: None,
            _watcher: None,
            _worktree_watcher: None,
            pr: None,
            pr_remote: None,
            pr_details_open: false,
            pr_loading: None,
            pr_error: None,
            pr_picker: None,
            author: None,
            submit: None,
            submit_epoch: 0,
        };

        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while watch_rx.next().await.is_some() {
                // Coalesce event bursts (temp write + rename fire separately)
                // into one reload.
                while watch_rx.try_recv().is_ok() {}
                // The normalized location (set by the load task), the
                // review currently shown so it isn't dropped when a submit
                // leaves no draft behind, and the PR this workspace is
                // scoped to (if any) so a reload can't adopt some other
                // PR's draft out from under it (review finding P1-b).
                let Ok((location, current_id, current_pr)) = this.update(cx, |this, _| {
                    (
                        this.location.clone(),
                        this.review.as_ref().map(|r| r.id.clone()),
                        this.pr_remote.clone(),
                    )
                }) else {
                    break;
                };
                let review = cx
                    .background_executor()
                    .spawn(async move {
                        pick_review(
                            dv_core::ReviewStore::open(location)
                                .list()
                                .unwrap_or_default(),
                            current_id.as_deref(),
                            current_pr.as_ref(),
                        )
                    })
                    .await;
                let alive = this.update(cx, |this, cx| {
                    let fingerprint = |r: &Option<dv_core::Review>| {
                        r.as_ref()
                            .map(|r| (r.id.clone(), r.updated_ms, r.comments.len()))
                    };
                    if fingerprint(&this.review) != fingerprint(&review) {
                        this.review = review;
                        cx.emit(ReviewChanged);
                        // A watcher-driven reload invalidates a parked
                        // submit panel — it was built against the review
                        // as it stood before this external change (review
                        // finding P1-3).
                        this.cancel_submit_flow_if_parked(cx);
                        this.reset_diff_list(cx);
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break; // workspace dropped
                }
            }
        })
        .detach();

        // Worktree watch consumer (plan §6/S4): reacts to a `_worktree_watcher`
        // event by re-running `changed_files` for a `WorkingTree` source and
        // forcing a fresh staleness check — the "backlogged staleness
        // refresh" this watch exists for (a worktree edit can drift a
        // comment's anchor without the review itself ever changing, which
        // is otherwise invisible to `refresh_stale`'s
        // `(file, review.updated_ms)` cache key). No-op for every session
        // that never gets a live `_worktree_watcher` in the first place —
        // this loop simply never receives anything then.
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while worktree_rx.next().await.is_some() {
                // Coalesce a burst, then apply the client-side debounce on
                // top of the host's own 200ms coalesce (plan §6) — an
                // editor's atomic save is a write+rename pair, and `git`
                // touching the index during a stage/commit fires several
                // more.
                while worktree_rx.try_recv().is_ok() {}
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                while worktree_rx.try_recv().is_ok() {}

                let Ok((repo, source, epoch)) = this.update(cx, |this, _| {
                    (this.repo.clone(), this.source.clone(), this.source_epoch)
                }) else {
                    break;
                };
                let Some(repo) = repo else { continue };
                if !matches!(source, DiffSource::WorkingTree) {
                    // The source has since moved on to a PR/range (open_pr
                    // already tears the watcher down when that happens —
                    // this just guards the narrow window before that takes
                    // effect).
                    continue;
                }

                let files = cx
                    .background_executor()
                    .spawn(async move { repo.changed_files(&source) })
                    .await;

                let alive = this.update(cx, |this, cx| {
                    // Epoch alone isn't sufficient (review finding P2-1):
                    // `open_pr` bumps `source_epoch` BEFORE its fetch even
                    // starts, so a worktree event landing while a PR load is
                    // still in flight could pass an epoch-only check even
                    // though `this.source` has already moved off
                    // `WorkingTree` — re-check the source's actual type too.
                    if this.source_epoch != epoch || !matches!(this.source, DiffSource::WorkingTree)
                    {
                        return; // superseded — discard rather than clobber newer state
                    }
                    if let Ok(files) = files {
                        let old_files = std::mem::replace(&mut this.files, files);
                        // `selected`/`diffs`/`diff_pending`/`expanded` are
                        // all keyed by INDEX into `this.files`, not by path
                        // — a naive replace either panics once the list has
                        // shrunk past the old selected index (review
                        // finding P1-1: an external `git commit` emptying
                        // the list while file 2 was selected) or silently
                        // re-renders the wrong file's cached diff under a
                        // reused index once a new file sorts in earlier.
                        let (new_selected, needs_invalidation) =
                            reconcile_file_selection(&old_files, &this.files, this.selected);
                        if needs_invalidation {
                            this.diffs.clear();
                            this.diff_pending.clear();
                            this.expanded.clear();
                            this.pending_jump = None;
                            match new_selected {
                                Some(index) => {
                                    // Same file as before, just relocated —
                                    // preserve `current_hunk` and the diff
                                    // list's scroll position (reset_diff_list,
                                    // called below, keeps the current offset).
                                    this.selected = Some(index);
                                    this.request_diff(index, cx);
                                }
                                None if !this.files.is_empty() => {
                                    // The previously selected file is gone
                                    // (or nothing was selected yet) — this is
                                    // genuinely a different file, so reset
                                    // hunk navigation same as a normal
                                    // `select_file`.
                                    this.selected = Some(0);
                                    this.current_hunk = 0;
                                    this.request_diff(0, cx);
                                }
                                None => {
                                    // Nothing left to show — clear cleanly
                                    // rather than leave a dangling index (the
                                    // original crash: `self.files[2]` after
                                    // an external commit emptied the list).
                                    this.selected = None;
                                }
                            }
                        }
                    }
                    // Force `refresh_stale` (called at the tail of
                    // `reset_diff_list` below) to actually re-run its git
                    // check even though neither the review nor the
                    // selection changed.
                    this.stale_checked = None;
                    this.reset_diff_list(cx);
                    cx.notify();
                });
                if alive.is_err() {
                    break; // workspace dropped
                }
            }
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let repo = GitRepo::open(location.clone())?;
                    let head = repo.head_label().unwrap_or_default();
                    // Resolve a merge-base range to a concrete two-dot range
                    // once here, so both the file list and every per-file
                    // old-side blob load from the merge base rather than from
                    // `base` directly (correct when base has advanced past
                    // the fork point). `git diff a...b` ≡ `a-merge-base..b`.
                    let source = resolve_source(&repo, source)?;
                    let files = repo.changed_files(&source)?;
                    // The store lives at the repo toplevel — use the
                    // normalized location, never the CLI/picker argument.
                    let store_location = repo.location().clone();
                    // The latest draft review is the one comments accumulate
                    // into (matching the CLI's default targeting). No PR is
                    // open yet at this point in the load — even when
                    // `pending_pr` is set, `open_pr` runs right after and
                    // assigns its own PR-linked review, superseding this.
                    let review = pick_review(
                        dv_core::ReviewStore::open(store_location.clone())
                            .list()
                            .unwrap_or_default(),
                        None,
                        None,
                    );
                    // Resolved here (off-thread: it may hit a `gh` subprocess
                    // on first use) so new comments/replies stamp the real
                    // author from the very first one, not just once some
                    // later save happens to trigger it.
                    let author = crate::author::resolve_author(&repo);
                    anyhow::Ok((
                        Arc::new(repo),
                        head,
                        files,
                        source,
                        review,
                        store_location,
                        author,
                    ))
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                match loaded {
                    Ok((repo, head, files, source, review, store_location, author)) => {
                        this.repo = Some(repo);
                        this.head = head.into();
                        this.files = files;
                        this.source = source;
                        this.review = review;
                        this.author = Some(author);
                        cx.emit(ReviewChanged);
                        this.location = store_location.clone();
                        this.status = Status::Ready;
                        // Start watching now that the true store path is
                        // known.
                        this._watcher = dv_core::ReviewStore::open(store_location)
                            .watch(Box::new(move || {
                                watch_tx.unbounded_send(()).ok();
                            }))
                            .inspect_err(|err| eprintln!("review watcher unavailable: {err:#}"))
                            .ok();
                        // Worktree watching (plan §6/S4) is a HOST-ONLY
                        // capability, and only worth it for a working-tree
                        // source — a PR/range/commit view's file list is
                        // fixed by its endpoints, not by what's on disk
                        // right now. Skipped entirely when `pending_pr` is
                        // set: `open_pr` right below immediately replaces
                        // `this.source` with a `Range` anyway, so setting
                        // this up here just to tear it down again a moment
                        // later would be wasted work (its own completion
                        // handles the teardown for the later, user-driven
                        // case instead — see its `_worktree_watcher = None`).
                        if pending_pr.is_none() && matches!(this.source, DiffSource::WorkingTree) {
                            this._worktree_watcher = dv_core::remote::watch_worktree(
                                this.location.clone(),
                                Box::new(move || {
                                    worktree_tx.unbounded_send(()).ok();
                                }),
                            );
                        }
                        if let Some(number) = pending_pr {
                            this.open_pr(number, window, cx);
                        } else if !this.files.is_empty() {
                            this.select_file_inner(0, cx);
                        }
                    }
                    Err(err) => this.status = Status::Failed(format!("{err:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();

        this
    }

    /// Switch to another file. A live gutter selection or comment editor
    /// belongs to the file it was made on — carrying it across would
    /// persist a comment against the new file with the old file's line
    /// numbers (review P1) — so switching drops them. Mid-save the switch
    /// is refused instead, so the in-flight comment can't be orphaned.
    pub(crate) fn select_file(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected != Some(index) {
            if self.editor.as_ref().is_some_and(|e| e.saving)
                || self.thread_input.as_ref().is_some_and(|t| t.saving)
            {
                return;
            }
            self.selection = None;
            let dropped_editor = self.editor.take().is_some();
            let dropped_input = self.thread_input.take().is_some();
            if dropped_editor || dropped_input {
                window.focus(&self.focus_handle, cx);
            }
        }
        self.select_file_inner(index, cx);
    }

    /// The window-free core of [`Self::select_file`], for the initial load
    /// path (no editor can exist yet there).
    fn select_file_inner(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.files.len() {
            return;
        }
        self.selected = Some(index);
        self.current_hunk = 0;
        self.file_scroll
            .scroll_to_item(index, ScrollStrategy::Nearest);
        let jumped = self.reset_diff_list(cx);
        // reset_diff_list preserves the viewport for in-place updates; a
        // file switch starts reading from the top — unless a summary jump
        // just placed the viewport (last scroll_to wins).
        if !jumped {
            self.diff_list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
        }
        cx.notify();

        // A cached failure retries on reselect; a good diff is final.
        let cached_ok = self.diffs.get(&index).is_some_and(|d| !d.error);
        if cached_ok || self.diff_pending.contains(&index) {
            return;
        }
        self.request_diff(index, cx);
    }

    /// Kick off (or re-run) the off-thread row computation for one file,
    /// honoring its current gap-expansion state.
    fn request_diff(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.diff_pending.insert(index);

        let file = self.files[index].clone();
        let source = self.source.clone();
        // Captured now; checked on completion (see `source_epoch`'s doc
        // comment) — an `open_pr` landing (or even just starting) while
        // this computation runs means `index` will refer to a different
        // source's file list by the time this resolves.
        let epoch = self.source_epoch;
        // Same staleness trick, for a theme swap instead of a source swap
        // (see `highlight_epoch`'s doc comment).
        let highlight_epoch = self.highlight_epoch;
        let expand = self.expanded.get(&index).cloned().unwrap_or_default();
        let context_lines = self.context_lines;
        let theme = cx.theme();
        let hl = HighlightInputs {
            theme: theme.highlight_theme.clone(),
            intra_added: theme.success.opacity(0.32),
            intra_removed: theme.danger.opacity(0.32),
        };
        cx.spawn(async move |this, cx| {
            let started = std::time::Instant::now();
            let rendered = cx
                .background_executor()
                .spawn(
                    async move { compute_diff(&repo, &source, &file, &hl, &expand, context_lines) },
                )
                .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;

            this.update(cx, |this, cx| {
                if this.source_epoch != epoch || this.highlight_epoch != highlight_epoch {
                    // Stale: computed against a source that's since been
                    // replaced, or a theme that's since been swapped again.
                    // Discard outright rather than write rows for the wrong
                    // file/palette into the cache under a reused index
                    // (review finding P1-a; same reasoning for the theme
                    // case, see `highlight_epoch`).
                    return;
                }
                this.diff_pending.remove(&index);
                this.last_diff_ms = Some(elapsed_ms);
                match rendered {
                    Ok(diff) => {
                        this.diffs.insert(index, Arc::new(diff));
                    }
                    Err(err) => {
                        let msg: SharedString = format!("failed to compute diff: {err:#}").into();
                        this.diffs.insert(index, Arc::new(error_diff(msg)));
                    }
                }
                // Row indices may have shifted (gap expansion inserts rows
                // above); re-sync the list and re-anchor the viewport on
                // the current hunk so the content doesn't visually jump.
                if this.selected == Some(index) {
                    let jumped = this.reset_diff_list(cx);
                    if !jumped {
                        this.scroll_to_current_hunk();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Called by the shell right after a live theme swap (`themes::apply_theme`
    /// updates the *global* theme via `Theme::change`/`apply_config`, which
    /// UI chrome picks up for free since it reads `cx.theme()` fresh every
    /// render — but the diff pane's syntax-highlighted rows were baked with
    /// the *old* theme's concrete colors at compute time, see
    /// `highlight_epoch`'s doc comment). Dropping the whole cache and
    /// recomputing only the currently selected file is what makes the swap
    /// visually complete immediately; any other (currently unselected) file
    /// simply recomputes lazily — under the new theme — the next time it's
    /// picked, same as a first-ever view of it.
    pub(crate) fn on_theme_changed(&mut self, cx: &mut Context<Self>) {
        self.invalidate_diff_cache(cx);
    }

    /// Drop every cached [`RenderedDiff`] and recompute the currently
    /// selected file — shared by [`Self::on_theme_changed`] (colors baked
    /// stale) and [`Self::set_context_lines`] (hunk structure itself
    /// changed, so the cache is stale in a much more literal sense). Any
    /// other (currently unselected) file simply recomputes lazily under the
    /// new settings the next time it's picked, same as a first-ever view.
    fn invalidate_diff_cache(&mut self, cx: &mut Context<Self>) {
        self.highlight_epoch += 1;
        self.diffs.clear();
        self.diff_pending.clear();
        if let Some(index) = self.selected {
            self.request_diff(index, cx);
        }
        cx.notify();
    }

    /// Settings-panel/`set_setting` live update for "Context lines" — see
    /// `settings::Settings::context_lines`. A no-op when unchanged, so a
    /// stepper click that hits a clamp boundary doesn't pay for a recompute.
    pub(crate) fn set_context_lines(&mut self, context_lines: u32, cx: &mut Context<Self>) {
        if self.context_lines == context_lines {
            return;
        }
        self.context_lines = context_lines;
        self.invalidate_diff_cache(cx);
    }

    /// Settings-panel/`set_setting` live update for "Font size" — see
    /// `settings::Settings::mono_font_size`. Unlike a theme/context-lines
    /// change, nothing here is baked into the [`RenderedDiff`] cache (only
    /// colors and hunk structure are — see `highlight.rs`'s `HighlightStyle`
    /// runs, which carry no size), so this only needs to re-measure the
    /// list's cached row heights (`ListState::remeasure`, built for exactly
    /// this: "item heights may have changed... but the number and identity
    /// of items remains the same") and repaint — no recompute, no epoch
    /// bump.
    pub(crate) fn set_font_size(&mut self, font_size: f32, cx: &mut Context<Self>) {
        if self.font_size == font_size {
            return;
        }
        self.font_size = font_size;
        self.diff_list.remeasure();
        cx.notify();
    }

    /// External (top-down) update for "Summary panel width" — the settings
    /// panel or `set_setting`'s `"summary_width"` case, routed through
    /// `AppShell::set_summary_width`. The handle's own live drag
    /// (`render_summary_resize_handle`) writes `self.summary_width` directly
    /// instead of calling this, since it already owns the render-time value
    /// and just needs `cx.notify()` per frame — no `Settings` round trip
    /// mid-drag.
    pub(crate) fn set_summary_width_external(&mut self, width: f32, cx: &mut Context<Self>) {
        let width = width.clamp(SUMMARY_WIDTH_MIN, SUMMARY_WIDTH_MAX);
        if self.summary_width == width {
            return;
        }
        self.summary_width = width;
        cx.notify();
    }

    /// Settings-panel/`set_setting` live update for "Default view" — applies
    /// to the workspace that's open right now, not just future ones (the
    /// setting only picks what a *freshly opened* review starts in).
    pub(crate) fn set_view_mode_setting(&mut self, mode: ViewModeSetting, cx: &mut Context<Self>) {
        self.set_view_mode(mode.into(), cx);
    }

    fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.view_mode == mode {
            return;
        }
        self.view_mode = mode;
        // Row count and indices differ between the views; re-sync the list
        // and keep the eye on the same hunk across the toggle.
        self.reset_diff_list(cx);
        self.scroll_to_current_hunk();
        cx.notify();
    }

    /// Reveal the hidden context above `hunk` in the selected file, then
    /// recompute that file's rows.
    fn expand_hunk_gap(&mut self, hunk: usize, cx: &mut Context<Self>) {
        let Some(index) = self.selected else {
            return;
        };
        self.expanded.entry(index).or_default().insert(hunk);
        // Anchor the viewport on the expanded hunk once the rebuilt rows
        // land (see request_diff) — its own row index is about to move.
        self.current_hunk = hunk;
        // Keep the stale rows on screen while the recompute runs; the
        // completion overwrites them. Removing them first blanks the pane
        // for the whole recompute (visible on large files).
        self.request_diff(index, cx);
    }

    // ---- GitHub PR open flow ------------------------------------------

    /// Open PR `number`: fetch its metadata + range (off-thread), switch
    /// the diff source to it, reload the file list, and find-or-create the
    /// draft review it links to (docs/phase-3-github.md deliverable 2).
    /// Reused by the GUI PR picker, `dv pr <number|url>`'s launch path, and
    /// the `open_pr` automation command.
    pub(crate) fn open_pr(&mut self, number: u64, window: &mut Window, cx: &mut Context<Self>) {
        // Same contract as `select_file`: a save in flight must never be
        // dropped. Refuse the whole open rather than orphan it (review
        // finding P2-a — this used to `take()` the editor unconditionally
        // at dispatch, so a typo'd PR number cost the user their typed
        // comment even though the fetch itself hadn't touched anything).
        // A GitHub submit POST in flight (`SubmitFlow::Submitting`) gets
        // the same treatment (docs/phase-3-github.md deliverable 2): it
        // must run to completion, so a source switch mid-submit is refused
        // rather than left to race the writeback.
        if self.editor.as_ref().is_some_and(|e| e.saving)
            || self.thread_input.as_ref().is_some_and(|t| t.saving)
            || self.submit_in_flight()
        {
            return;
        }
        // Only one PR open in flight at a time (review finding P2-b): the
        // second of a back-to-back pair is simply ignored. Simpler UX, and
        // it closes the find-or-create TOCTOU window in `load_pr` against
        // a concurrent second open (belt-and-suspenders re-list there too).
        if self.pr_loading.is_some() {
            return;
        }
        // A parked submit panel (Validating/Blocked/Confirming) is scoped
        // to the review being left behind — surviving a PR switch is
        // exactly the "stuck validating panel" / cross-PR corruption this
        // fixes (review finding P1-2). `Submitting` was already refused
        // above, so this can only cancel, never clobber an in-flight POST.
        self.cancel_submit_flow(cx);

        let Some(repo) = self.repo.clone() else {
            // The repo hasn't finished its own initial load yet — nothing
            // to fetch against. Shouldn't happen on any of this method's
            // real call sites (picker/automation both require an already-
            // ready workspace; the `pending_pr` launch path only calls
            // this once the initial load itself just succeeded).
            self.pr_error = Some("repository is still loading — try again in a moment".into());
            cx.notify();
            return;
        };

        // Closing the picker here (rather than deferred to the completion
        // below, like the rest of the teardown) is still fine: it holds no
        // user data, and leaving it open over the "Loading" pane would
        // just look broken.
        if self.pr_picker.take().is_some() {
            window.focus(&self.focus_handle, cx);
        }

        self.pr_error = None;
        self.pr_loading = Some(number);
        self.status = Status::Loading;
        // Bumped before the fetch even starts, so any request_diff already
        // in flight (and this open_pr's own completion, below) can tell a
        // superseding open_pr apart from itself (see the field's doc
        // comment).
        self.source_epoch += 1;
        let epoch = self.source_epoch;
        cx.notify();

        let location = self.location.clone();
        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { load_pr(&repo, number, location) })
                .await;

            this.update_in(cx, |this, window, cx| {
                if this.source_epoch != epoch {
                    // A newer open_pr superseded this one before this
                    // fetch finished: don't touch source/files/review/pr,
                    // don't flip Loading back to Ready, don't tear down
                    // whatever the newer call is showing. At most clear
                    // our own cosmetic loading caption, and only if it's
                    // still showing this call's number (the newer call
                    // already overwrote it with its own otherwise).
                    if this.pr_loading == Some(number) {
                        this.pr_loading = None;
                    }
                    return;
                }
                this.pr_loading = None;
                match outcome {
                    Ok(PrOpenOutcome {
                        meta,
                        files,
                        review,
                        source,
                    }) => {
                        // Only now — a real, current-epoch success — is it
                        // safe to tear down the live selection/editor/
                        // thread-input: `Status::Loading` has had the body
                        // pane replaced this whole time, so nothing could
                        // have created a new one in the meantime (review
                        // finding P2-a).
                        this.selection = None;
                        let dropped_editor = this.editor.take().is_some();
                        let dropped_input = this.thread_input.take().is_some();
                        if dropped_editor || dropped_input {
                            window.focus(&this.focus_handle, cx);
                        }
                        this.source = source;
                        this.source_desc = format!("PR #{number}").into();
                        // A PR's file list is fixed by its endpoints, not
                        // by the working tree — worktree watching (plan
                        // §6/S4) only ever applies to a `WorkingTree`
                        // source (see where it's set up, in the initial
                        // load completion above). There's no path back to
                        // `WorkingTree` from a PR in this codebase today,
                        // so nothing ever needs to re-create it once torn
                        // down here.
                        this._worktree_watcher = None;
                        this.files = files;
                        // The old file list's diffs are keyed by index into
                        // a now-replaced list — stale caches would render
                        // the wrong file's content under the right name.
                        this.diffs.clear();
                        this.diff_pending.clear();
                        this.expanded.clear();
                        this.stale.clear();
                        this.stale_checked = None;
                        this.selected = None;
                        this.pending_jump = None;
                        this.pr_remote = review.remote.clone();
                        this.review = Some(review);
                        cx.emit(ReviewChanged);
                        this.pr = Some(meta.into());
                        this.pr_details_open = false;
                        this.status = Status::Ready;
                        if this.files.is_empty() {
                            this.reset_diff_list(cx);
                        } else {
                            this.select_file(0, window, cx);
                        }
                    }
                    Err(err) => {
                        // Leave source/files/pr exactly as they were — the
                        // workspace stays on whatever it was showing before
                        // this attempt, selection/editor/thread-input
                        // included.
                        this.status = Status::Ready;
                        this.pr_error = Some(format!("{err:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-fetch this workspace's open PR's metadata and refresh just the
    /// header band from it (review finding P3-b, proven live: a DRAFT
    /// chip persisted through both `gh pr ready` and merge, since the
    /// header used to only ever refresh on `open_pr`). Unlike `open_pr`
    /// this never touches source/files/review/selection, so there's no
    /// save-in-flight/TOCTOU concern to guard against — only the result
    /// needs an epoch check. A no-op with no PR open. Wired to the
    /// sidebar's `RefreshBadges` button and directly dispatchable via the
    /// `RefreshPr` action (no keybinding — there's no natural key for it,
    /// automation/the button are the only callers). Deliberately not
    /// auto-polled; only ever fired by an explicit user gesture.
    pub(crate) fn refresh_pr(&mut self, cx: &mut Context<Self>) {
        if self.pr_loading.is_some() {
            // An `open_pr` is in flight: `source_epoch` was already bumped
            // but `self.pr` still shows the OLD header, so a refresh spawned
            // now would fetch the old PR yet pass the epoch check and stamp
            // its header over the new PR's. Refresh after the open lands.
            return;
        }
        let Some(current) = &self.pr else {
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let number = current.number;
        let epoch = self.source_epoch;
        cx.spawn(async move |this, cx| {
            let result: anyhow::Result<PrMeta> = cx
                .background_executor()
                .spawn(async move {
                    let client = submit::github_client(&repo)?;
                    anyhow::Ok(client.pr_meta(number)?)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.source_epoch != epoch || this.pr.as_ref().map(|p| p.number) != Some(number)
                {
                    // A source switch (or a fresh `open_pr`) superseded
                    // this fetch — discard rather than clobber whatever
                    // replaced it. The number check is belt-and-braces for
                    // any header swap that didn't bump the epoch.
                    return;
                }
                if let Ok(meta) = result {
                    this.pr = Some(meta.into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn on_refresh_pr(&mut self, _: &RefreshPr, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_pr(cx);
    }

    // ---- PR picker -----------------------------------------------------

    fn on_open_pr_picker(
        &mut self,
        _: &OpenPrPicker,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pr_picker.is_some() {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.pr_picker = Some(PrPicker {
            state: PrPickerState::Loading,
            selected: 0,
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let client = GithubClient::for_repo(&repo).map_err(|err| err.to_string())?;
                    client.preflight().map_err(|err| err.to_string())?;
                    client.list_prs().map_err(|err| err.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                if let Some(picker) = &mut this.pr_picker {
                    picker.selected = 0;
                    picker.state = match result {
                        Ok(prs) => PrPickerState::Loaded(prs),
                        Err(err) => PrPickerState::Error(err),
                    };
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn close_pr_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pr_picker.take().is_some() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    fn on_pr_picker_close(
        &mut self,
        _: &PrPickerClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_pr_picker(window, cx);
    }

    fn on_pr_picker_next(&mut self, _: &PrPickerNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = &mut self.pr_picker
            && let PrPickerState::Loaded(prs) = &picker.state
            && !prs.is_empty()
        {
            picker.selected = (picker.selected + 1).min(prs.len() - 1);
            cx.notify();
        }
    }

    fn on_pr_picker_prev(&mut self, _: &PrPickerPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = &mut self.pr_picker {
            picker.selected = picker.selected.saturating_sub(1);
            cx.notify();
        }
    }

    fn on_pr_picker_choose(
        &mut self,
        _: &PrPickerChoose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = &self.pr_picker else {
            return;
        };
        let PrPickerState::Loaded(prs) = &picker.state else {
            return;
        };
        let Some(number) = prs.get(picker.selected).map(|pr| pr.number) else {
            return;
        };
        self.open_pr(number, window, cx);
    }

    /// Semantic state for `--automation` (`{"cmd":"state"}`): what the
    /// workspace believes, independent of layout, so agents can assert on
    /// behavior and reserve screenshots for style.
    pub(crate) fn automation_state(&self) -> serde_json::Value {
        use serde_json::json;
        let status = match &self.status {
            Status::Loading => "loading".to_string(),
            Status::Ready => "ready".to_string(),
            Status::Failed(err) => format!("failed: {err}"),
        };
        json!({
            "title": self.title.to_string(),
            "head": self.head.to_string(),
            "source": self.source_desc.to_string(),
            "status": status,
            "settled": self.automation_settled(),
            "view_mode": match self.view_mode {
                ViewMode::Unified => "unified",
                ViewMode::Split => "split",
            },
            // Diff-display metrics (docs/phase-4-settings-and-theming.md
            // deliverable 3/7): lets a script assert row pitch scales with
            // `mono_font_size` without pixel-measuring a screenshot.
            "context_lines": self.context_lines,
            "font_size": self.font_size,
            "row_height": row_height(self.font_size),
            "selected": self.selected,
            "current_hunk": self.current_hunk,
            "last_diff_ms": self.last_diff_ms,
            "selection": self.selection.as_ref().map(|sel| {
                let (start, end) = sel.range();
                json!({
                    "file": sel.file,
                    "side": match sel.side { DiffSide::Old => "old", DiffSide::New => "new" },
                    "start": start,
                    "end": end,
                })
            }),
            "editor_open": self.editor.is_some(),
            // Plan §8 S4 gate: whether a live host-backed worktree watch
            // (plan §6) is active this session — `false` for every Local
            // repo, a disabled/no-host WSL session, or a PR/range source.
            "worktree_watch": self._worktree_watcher.is_some(),
            "summary_open": self.summary_open,
            // Drag-to-resize deliverable (Phase 4 deliverable 5): the
            // summary panel's own live width (the sidebar's mirror-image
            // `sidebar_width` lives on the shell — see
            // `AppShell::automation_state`).
            "summary_width": self.summary_width,
            "review": self.review.as_ref().map(|r| json!({
                "id": r.id,
                "comments": r.comments.len(),
                "open": r.comments.iter()
                    .filter(|c| c.status == dv_core::CommentStatus::Open)
                    .count(),
                "replies": r.comments.iter().map(|c| c.replies.len()).sum::<usize>(),
                "stale": self.stale.len(),
                "state": match &r.state {
                    dv_core::ReviewState::Draft => "draft".to_string(),
                    dv_core::ReviewState::Submitted { verdict, .. } => format!("submitted:{verdict:?}"),
                },
            })),
            "display_rows": self.display.len(),
            "scroll_item": self.diff_list.logical_scroll_top().item_ix,
            "palette": self.palette.as_ref().map(|p| json!({
                "matches": p.matches.len(),
                "selected": p.selected,
                "top": p.matches.first().map(|&i| self.files[i].path.clone()),
            })),
            "files": self.files.iter().map(|file| json!({
                "path": file.path,
                "old_path": file.old_path,
                "status": format!("{:?}", file.status),
            })).collect::<Vec<_>>(),
            "rows": self.selected.and_then(|i| self.diffs.get(&i)).map(|diff| json!({
                "unified": diff.unified.len(),
                "split": diff.split.len(),
            })),
            "pr": self.pr.as_ref().map(|pr| json!({
                "number": pr.number,
                "title": pr.title.to_string(),
                "state": pr_state_word(pr.state),
                "is_draft": pr.is_draft,
                "checks": checks_word(pr.checks),
                "review_decision": pr.review_decision.map(review_decision_word),
                "base_ref": pr.base_ref.to_string(),
                "head_ref": pr.head_ref.to_string(),
                "url": pr.url.to_string(),
            })),
            "pr_error": self.pr_error,
            "pr_picker_open": self.pr_picker.is_some(),
            "pr_picker": self.pr_picker.as_ref().map(|p| json!({
                "loading": matches!(p.state, PrPickerState::Loading),
                "items": match &p.state {
                    PrPickerState::Loaded(prs) => prs.len(),
                    _ => 0,
                },
                "selected": p.selected,
                "error": match &p.state {
                    PrPickerState::Error(err) => Some(err.clone()),
                    _ => None,
                },
            })),
            "submit": self.submit.as_ref().map(|flow| match flow {
                SubmitFlow::Validating { verdict } => json!({
                    "stage": "validating",
                    "verdict": verdict_automation_word(*verdict),
                }),
                SubmitFlow::Blocked { verdict, violations } => json!({
                    "stage": "blocked",
                    "verdict": verdict_automation_word(*verdict),
                    "violations": violations.iter().map(|v| json!({
                        "comment_id": v.comment_id,
                        "path": v.path,
                        "lines": v.lines,
                        "kind": violation_kind_word(v.kind),
                        "message": v.message,
                    })).collect::<Vec<_>>(),
                }),
                SubmitFlow::Confirming { verdict, prep } => json!({
                    "stage": "confirming",
                    "verdict": verdict_automation_word(*verdict),
                    "pr": prep.pr_number,
                    "comments": prep.submission.comments.len(),
                }),
                SubmitFlow::Submitting { verdict } => json!({
                    "stage": "submitting",
                    "verdict": verdict_automation_word(*verdict),
                }),
                SubmitFlow::Done { verdict, url } => json!({
                    "stage": "done",
                    "verdict": verdict_automation_word(*verdict),
                    "url": url,
                }),
                SubmitFlow::Failed { verdict, message } => json!({
                    "stage": "failed",
                    "verdict": verdict_automation_word(*verdict),
                    "message": message,
                }),
            }),
        })
    }

    /// Bounds-checked selection for `--automation`: unlike the UI path
    /// (which silently ignores stale indices), scripts get a hard error so
    /// the response never lies about what happened.
    pub(crate) fn automation_select_file(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        if index >= self.files.len() {
            anyhow::bail!(
                "file index {index} out of range ({} files)",
                self.files.len()
            );
        }
        self.select_file(index, window, cx);
        Ok(())
    }

    /// True once there is nothing left in flight: repo loaded (or failed),
    /// the selected file's diff computed with no recompute pending (gap
    /// expansion keeps stale rows visible while it rebuilds), an in-flight
    /// PR open settled (`open_pr` reuses `Status::Loading` for this), and
    /// an open PR picker's `gh pr list` fetch finished loading. `wait_ready`
    /// polls this.
    pub(crate) fn automation_settled(&self) -> bool {
        if self
            .pr_picker
            .as_ref()
            .is_some_and(|p| matches!(p.state, PrPickerState::Loading))
        {
            return false;
        }
        match &self.status {
            Status::Loading => false,
            Status::Failed(_) => true,
            Status::Ready => match self.selected {
                Some(index) => {
                    self.diffs.contains_key(&index) && !self.diff_pending.contains(&index)
                }
                None => true,
            },
        }
    }

    fn on_next_file(&mut self, _: &NextFile, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.selected.map_or(0, |i| i + 1);
        self.select_file(next.min(self.files.len().saturating_sub(1)), window, cx);
    }

    fn on_prev_file(&mut self, _: &PrevFile, window: &mut Window, cx: &mut Context<Self>) {
        let prev = self.selected.map_or(0, |i| i.saturating_sub(1));
        self.select_file(prev, window, cx);
    }

    fn on_toggle_split(&mut self, _: &ToggleSplit, _: &mut Window, cx: &mut Context<Self>) {
        let next = match self.view_mode {
            ViewMode::Unified => ViewMode::Split,
            ViewMode::Split => ViewMode::Unified,
        };
        self.set_view_mode(next, cx);
    }

    /// Row indices of the hunk starts for the selected file in the active
    /// view mode.
    fn hunk_rows(&self) -> Option<&[usize]> {
        let diff = self.selected.and_then(|i| self.diffs.get(&i))?;
        Some(match self.view_mode {
            ViewMode::Unified => &diff.hunk_rows_unified,
            ViewMode::Split => &diff.hunk_rows_split,
        })
    }

    /// The diff pane's row count for the selected file in the active mode.
    fn diff_row_count(&self) -> usize {
        self.selected
            .and_then(|i| self.diffs.get(&i))
            .map_or(0, |d| match self.view_mode {
                ViewMode::Unified => d.unified.len(),
                ViewMode::Split => d.split.len(),
            })
    }

    /// The diff row a `(side, line)` anchor points at in the active mode —
    /// where a comment thread (or the editor) hangs.
    fn anchor_row(&self, side: DiffSide, line: u32) -> Option<usize> {
        let diff = self.selected.and_then(|i| self.diffs.get(&i))?;
        match self.view_mode {
            ViewMode::Unified => diff.unified.iter().position(|row| match row {
                Row::Line {
                    old_line, new_line, ..
                } => match side {
                    DiffSide::New => *new_line == Some(line),
                    DiffSide::Old => *old_line == Some(line),
                },
                _ => false,
            }),
            ViewMode::Split => diff.split.iter().position(|row| match row {
                SplitRow::Pair { left, right } => {
                    let cell = match side {
                        DiffSide::Old => left,
                        DiffSide::New => right,
                    };
                    cell.as_ref().is_some_and(|c| c.line == Some(line))
                }
                _ => false,
            }),
        }
    }

    /// Rebuild the display row set — diff rows with comment threads and the
    /// editor interleaved under their anchors — and re-sync the list.
    /// Anything that changes rows, comments, selection, or the editor calls
    /// this. Threads whose anchor line isn't visible (outside hunks, stale)
    /// append at the end so they're never silently hidden.
    fn reset_diff_list(&mut self, cx: &mut Context<Self>) -> bool {
        let row_count = self.diff_row_count();
        let file_path = self.selected.map(|i| self.files[i].path.clone());

        // Anchor each of this file's comments to a diff row.
        let mut at_row: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut unanchored: Vec<usize> = Vec::new();
        if let (Some(path), Some(review)) = (&file_path, &self.review) {
            for (ci, comment) in review.comments.iter().enumerate() {
                if &comment.path != path {
                    continue;
                }
                let side = match comment.side {
                    dv_core::Side::Old => DiffSide::Old,
                    dv_core::Side::New => DiffSide::New,
                };
                match self.anchor_row(side, comment.end_line) {
                    Some(row) => at_row.entry(row).or_default().push(ci),
                    None => unanchored.push(ci),
                }
            }
        }
        let editor_row = self
            .editor
            .as_ref()
            .and(self.selection)
            .and_then(|sel| self.anchor_row(sel.side, sel.range().1));

        let mut display = Vec::with_capacity(row_count + 8);
        let mut diff_to_display = Vec::with_capacity(row_count);
        for row in 0..row_count {
            diff_to_display.push(display.len());
            display.push(DisplayRow::Diff(row));
            if let Some(comments) = at_row.get(&row) {
                display.extend(comments.iter().map(|&ci| DisplayRow::Thread(ci)));
            }
            if editor_row == Some(row) {
                display.push(DisplayRow::Editor);
            }
        }
        display.extend(unanchored.into_iter().map(DisplayRow::Thread));
        if self.editor.is_some() && editor_row.is_none() {
            display.push(DisplayRow::Editor);
        }

        self.display = display;
        self.diff_to_display = diff_to_display;
        // ListState::reset clears the scroll position (renders from item 0)
        // — but this rebuild runs for every comment mutation and watcher
        // reload, where yanking the viewport to the top mid-read would be
        // hostile. Preserve the offset (scroll_to clamps if rows shrank);
        // paths that *want* a position change (file switch, hunk jump)
        // scroll explicitly after calling this.
        let top = self.diff_list.logical_scroll_top();
        self.diff_list.reset(self.display.len());
        self.diff_list.scroll_to(top);

        // A summary-panel jump lands once its thread row exists — and only
        // against real rows: with the diff still computing, every thread
        // sits in the unanchored placeholder section and consuming the jump
        // there would burn it before the true anchor rows exist (review
        // finding: jumps never landed).
        let mut jumped = false;
        let diff_loaded = self.selected.and_then(|i| self.diffs.get(&i)).is_some();
        if diff_loaded
            && let Some(id) = self.pending_jump.clone()
            && let Some(review) = &self.review
            && let Some(ci) = review.comments.iter().position(|c| c.id == id)
            && let Some(ix) = self
                .display
                .iter()
                .position(|r| *r == DisplayRow::Thread(ci))
        {
            self.pending_jump = None;
            self.diff_list.scroll_to(ListOffset {
                item_ix: ix.saturating_sub(2), // a little context above
                offset_in_item: px(0.),
            });
            jumped = true;
        }

        // An externally-deleted comment must not leave a ghost reply/edit
        // input alive (hidden card + EditorOpen key context = dead keys).
        if let Some(ti) = &self.thread_input
            && self
                .review
                .as_ref()
                .is_none_or(|r| !r.comments.iter().any(|c| c.id == ti.comment_id))
        {
            self.thread_input = None;
        }

        self.refresh_stale(cx);
        jumped
    }

    /// Re-check which of the selected file's comments have drifted anchors
    /// (their stored blob sha no longer matches the diff's blob). Runs git
    /// off-thread; keyed on (file, review.updated_ms) so display rebuilds
    /// don't re-run it needlessly.
    fn refresh_stale(&mut self, cx: &mut Context<Self>) {
        let (Some(file), Some(review), Some(repo)) =
            (self.selected, self.review.as_ref(), self.repo.clone())
        else {
            return;
        };
        let key = (file, review.updated_ms);
        if self.stale_checked == Some(key) {
            return;
        }
        self.stale_checked = Some(key);

        let path = self.files[file].path.clone();
        let source = review.source.clone();
        let anchored: Vec<(String, dv_core::Side, Option<String>)> = review
            .comments
            .iter()
            .filter(|c| c.path == path)
            .map(|c| (c.id.clone(), c.side, c.blob_sha.clone()))
            .collect();
        if anchored.is_empty() {
            return;
        }

        cx.spawn(async move |this, cx| {
            let (checked, stale_ids) = cx
                .background_executor()
                .spawn(async move {
                    // One git call per side present, not per comment. The
                    // outer Option is the git call itself: a transient
                    // failure (index.lock, WSL hiccup) must leave existing
                    // verdicts untouched, not flag everything stale.
                    let mut current: HashMap<bool, Option<Option<String>>> = HashMap::new();
                    let mut checked = Vec::new();
                    let mut stale = Vec::new();
                    for (id, side, sha) in anchored {
                        let Some(sha) = sha else { continue }; // unverifiable
                        let is_new = matches!(side, dv_core::Side::New);
                        let entry = current.entry(is_new).or_insert_with(|| {
                            repo.blob_sha(&anchor_spec(&source, side, &path)).ok()
                        });
                        let Some(current_sha) = entry else {
                            continue; // git failed — skip, keep prior verdict
                        };
                        if current_sha.as_deref() != Some(sha.as_str()) {
                            stale.push(id.clone());
                        }
                        checked.push(id);
                    }
                    (checked, stale)
                })
                .await;

            this.update(cx, |this, cx| {
                // Fresh verdicts for everything checked this round; other
                // files' verdicts are left as last computed.
                for id in &checked {
                    this.stale.remove(id);
                }
                for id in stale_ids {
                    this.stale.insert(id);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn scroll_to_current_hunk(&mut self) {
        let current = self.current_hunk;
        if let Some(&row) = self.hunk_rows().and_then(|rows| rows.get(current))
            && let Some(&display_ix) = self.diff_to_display.get(row)
        {
            self.diff_list.scroll_to(ListOffset {
                item_ix: display_ix,
                offset_in_item: px(0.),
            });
        }
    }

    fn on_next_hunk(&mut self, _: &NextHunk, _: &mut Window, cx: &mut Context<Self>) {
        let count = self.hunk_rows().map_or(0, <[usize]>::len);
        if count == 0 {
            return;
        }
        self.current_hunk = (self.current_hunk + 1).min(count - 1);
        self.scroll_to_current_hunk();
        cx.notify();
    }

    fn on_prev_hunk(&mut self, _: &PrevHunk, _: &mut Window, cx: &mut Context<Self>) {
        if self.hunk_rows().is_none_or(<[usize]>::is_empty) {
            return;
        }
        self.current_hunk = self.current_hunk.saturating_sub(1);
        self.scroll_to_current_hunk();
        cx.notify();
    }

    // ---- Gutter selection (comment anchoring) ------------------------

    /// Mouse pressed on a line's gutter: start (or shift-extend) a
    /// selection on that side.
    fn gutter_down(&mut self, side: DiffSide, line: u32, shift: bool, cx: &mut Context<Self>) {
        let Some(file) = self.selected else {
            return;
        };
        match &mut self.selection {
            // Shift-click with a compatible selection extends it.
            Some(sel) if shift && sel.file == file && sel.side == side => {
                sel.head = line;
                sel.dragging = false;
            }
            _ => {
                self.selection = Some(GutterSelection {
                    file,
                    side,
                    anchor: line,
                    head: line,
                    dragging: true,
                });
            }
        }
        cx.notify();
    }

    /// Mouse moved over a row while dragging from the gutter: extend the
    /// range to this row's line on the selection's side.
    fn gutter_drag_over(&mut self, side: DiffSide, line: u32, cx: &mut Context<Self>) {
        if let Some(sel) = &mut self.selection
            && sel.dragging
            && sel.side == side
            && sel.head != line
        {
            sel.head = line;
            cx.notify();
        }
    }

    fn gutter_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sel) = &mut self.selection
            && sel.dragging
        {
            sel.dragging = false;
            // Releasing the drag is the commitment: open the editor under
            // the selection, GitHub-style.
            self.open_editor(window, cx);
            cx.notify();
        }
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.selection.take().is_some() {
            cx.notify();
        }
    }

    fn on_clear_selection(&mut self, _: &ClearSelection, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_selection(cx);
        // Escape also cancels a cancellable submit-flow stage
        // (docs/phase-3-github.md deliverable 2) — harmless when no flow is
        // active (`cancel_submit_flow` is a no-op then).
        self.cancel_submit_flow(cx);
    }

    // ---- Inline comment editor + store operations ---------------------

    fn open_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::{InputEvent, InputState};
        if self.selection.is_none() {
            return;
        }
        if let Some(editor) = &self.editor {
            let input = editor.input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
            self.reset_diff_list(cx);
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(3, 12)
                .placeholder("Leave a comment… (ctrl-enter to submit, esc to cancel)")
        });
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter {
                    secondary: true, ..
                } = event
                {
                    this.submit_comment(window, cx);
                }
            });
        input.update(cx, |input, cx| input.focus(window, cx));
        self.editor = Some(CommentEditor {
            input,
            saving: false,
            _subscription: subscription,
        });
        self.reset_diff_list(cx);
        if let Some(ix) = self.display.iter().position(|r| *r == DisplayRow::Editor) {
            self.diff_list.scroll_to_reveal_item(ix);
        }
        cx.notify();
    }

    fn close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.take().is_some() {
            self.selection = None;
            window.focus(&self.focus_handle, cx);
            self.reset_diff_list(cx);
            cx.notify();
        }
    }

    fn on_cancel_comment(
        &mut self,
        _: &CancelComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.thread_input.is_some() {
            self.close_thread_input(window, cx);
        } else {
            self.close_editor(window, cx);
        }
    }

    fn on_toggle_summary(&mut self, _: &ToggleSummary, _: &mut Window, cx: &mut Context<Self>) {
        self.summary_open = !self.summary_open;
        if !self.summary_open {
            // Closing the summary also cancels a cancellable submit-flow
            // stage (docs/phase-3-github.md deliverable 2).
            self.cancel_submit_flow(cx);
        }
        cx.notify();
    }

    /// Finish the draft review with a verdict (recorded locally; Phase 3
    /// maps this onto GitHub submission). Fresh-loads before mutating like
    /// every other store write.
    fn submit_review(&mut self, verdict: dv_core::Verdict, cx: &mut Context<Self>) {
        let Some(review) = self.review.clone() else {
            return;
        };
        let location = self.location.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let store = dv_core::ReviewStore::open(location);
                    let mut review = store.load(&review.id).ok().flatten().unwrap_or(review);
                    review.set_state(dv_core::ReviewState::Submitted {
                        verdict,
                        at_ms: dv_core::review::now_ms(),
                    });
                    store.save(&review)?;
                    anyhow::Ok(review)
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    // Keep showing the just-submitted review; the next
                    // comment auto-creates a fresh draft.
                    Ok(review) => {
                        this.review = Some(review);
                        cx.emit(ReviewChanged);
                    }
                    Err(err) => eprintln!("finish review failed: {err:#}"),
                }
                this.reset_diff_list(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- GitHub submission flow (docs/phase-3-github.md deliverable 2) --

    /// Whether a `gh` submit POST is currently in flight — the one
    /// [`SubmitFlow`] stage that must run to completion (see its doc
    /// comment): `open_pr` refuses a source switch while this is true, the
    /// same way it already refuses one over an in-flight comment save.
    fn submit_in_flight(&self) -> bool {
        matches!(self.submit, Some(SubmitFlow::Submitting { .. }))
    }

    /// Reset `self.submit` back to idle. A no-op while `Submitting` (must
    /// run to completion) or already idle; every other stage —
    /// `Validating`/`Blocked`/`Confirming`/`Done`/`Failed` — resets
    /// cleanly and bumps `submit_epoch` (review finding P1-1). Called from
    /// Escape (`on_clear_selection`), from closing the summary panel
    /// (`on_toggle_summary`), per docs/phase-3-github.md deliverable 2
    /// ("Escape/summary-close cancels Confirming/Blocked"), and from
    /// `open_pr` (review finding P1-2) so a PR switch can never leave a
    /// stale panel — or a stale in-flight validation — behind. The actual
    /// decision is [`cancel_submit_flow_outcome`], a pure function kept
    /// separate so it's unit-testable without a `Context`.
    fn cancel_submit_flow(&mut self, cx: &mut Context<Self>) {
        let (next, changed, epoch) =
            cancel_submit_flow_outcome(self.submit.take(), self.submit_epoch);
        self.submit = next;
        self.submit_epoch = epoch;
        if changed {
            cx.notify();
        }
    }

    /// Whether `self.submit` is "parked" against a comments/verdict
    /// snapshot that a review mutation can invalidate: `Validating` (still
    /// building that snapshot), `Confirming` (built, waiting on the user),
    /// `Blocked` (built, found problems). Deliberately excludes
    /// `Submitting` (the flow's own in-flight write) and `Done`/`Failed`
    /// (that write's own terminal result) — see
    /// [`Self::cancel_submit_flow_if_parked`].
    fn submit_flow_is_parked(&self) -> bool {
        matches!(
            self.submit,
            Some(
                SubmitFlow::Validating { .. }
                    | SubmitFlow::Confirming { .. }
                    | SubmitFlow::Blocked { .. }
            )
        )
    }

    /// Cancel a parked submit flow when the review it was built against
    /// changes out from under it (review finding P1-3, reproduced live:
    /// `Confirming` kept offering a comment that had just been resolved
    /// externally). A no-op unless [`Self::submit_flow_is_parked`] —
    /// `Submitting`/`Done`/`Failed` are never touched here, so the flow's
    /// own writeback (which itself updates `self.review` and emits
    /// `ReviewChanged` right before landing on `Done`/`Failed`) can't
    /// immediately dismiss the very outcome panel it just produced. Called
    /// from every site that assigns `self.review` in response to an
    /// external or independent change: the review-store watcher pump, and
    /// each GUI comment-mutation completion (add/status/delete/reply/edit)
    /// — never from `open_pr`'s completion, which already went through
    /// [`Self::cancel_submit_flow`] synchronously before its fetch even
    /// started, and never from `on_submit_click`'s own completion (already
    /// excluded above).
    fn cancel_submit_flow_if_parked(&mut self, cx: &mut Context<Self>) {
        if self.submit_flow_is_parked() {
            self.cancel_submit_flow(cx);
        }
    }

    /// Verdict button click on the summary panel. A local-only review (no
    /// `remote` linkage) behaves exactly as it always has — an immediate
    /// local finish via [`Self::submit_review`]. A PR-linked review instead
    /// starts the GitHub submit flow: background validation, then an
    /// explicit confirm step, before anything is ever sent (deliverable 2).
    fn on_verdict_clicked(&mut self, verdict: dv_core::Verdict, cx: &mut Context<Self>) {
        if self.submit.is_some() {
            return; // a flow is already active — buttons aren't even shown then, but belt-and-suspenders
        }
        match self.review.as_ref().and_then(|r| r.remote.clone()) {
            Some(remote) => self.start_submit_validation(verdict, remote, cx),
            None => self.submit_review(verdict, cx),
        }
    }

    /// Background validation for a PR-linked verdict click: fresh
    /// `pr_meta` + `prepare_pr` + `crate::submit::build_submission` — no
    /// `gh` write happens here, only reads. Lands on `Blocked` (a clear
    /// violation list) or `Confirming` (built and waiting on an explicit
    /// [Submit to GitHub] click). Epoch-guarded on `submit_epoch` (review
    /// finding P1-1 — this used to check only `source_epoch`, which an
    /// Escape-cancel never touched, so a cancelled `Validating` could
    /// silently resurrect the panel once the background fetch finally
    /// landed): a newer transition — cancel, a later verdict click, or an
    /// `open_pr` (which cancels any parked flow up front, bumping this same
    /// epoch) — while this runs means the result must be discarded rather
    /// than resurrect a dead flow or clobber whatever supersedes it.
    fn start_submit_validation(
        &mut self,
        verdict: dv_core::Verdict,
        remote: dv_core::RemoteRef,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = self.review.clone() else {
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.submit_epoch += 1;
        self.submit = Some(SubmitFlow::Validating { verdict });
        cx.notify();

        let epoch = self.submit_epoch;
        let pr_number = remote.pr;
        let review_id = review.id.clone();
        cx.spawn(async move |this, cx| {
            let outcome: anyhow::Result<(PrMeta, SubmissionOutcome)> = cx
                .background_executor()
                .spawn(async move {
                    let client = submit::github_client(&repo)?;
                    let meta = client.pr_meta(pr_number)?;
                    if meta.state != dv_core::PrState::Open {
                        anyhow::bail!(submit::pr_not_open_message(pr_number, &meta));
                    }
                    let pr_range = crate::pr::prepare_pr(&repo, &meta)?;
                    let outcome = submit::build_submission(
                        &repo,
                        &pr_range.merge_base,
                        &pr_range.head_oid,
                        &review,
                        verdict,
                        String::new(),
                        false,
                    )?;
                    anyhow::Ok((meta, outcome))
                })
                .await;

            this.update(cx, |this, cx| {
                if this.submit_epoch != epoch {
                    // Superseded — discard silently before touching any
                    // state (review finding P1-1).
                    return;
                }
                // A fresh `pr_meta` just came back — refresh the header
                // band from it in this same completion (review finding
                // P3-b), so a PR that went ready/merged/etc. since the
                // last `open_pr`/`refresh_pr` doesn't keep showing a
                // stale chip indefinitely. No extra fetch, no new race:
                // this is the same epoch-guarded completion already
                // deciding whether to land at all.
                if let Ok((meta, _)) = &outcome {
                    this.pr = Some(meta.clone().into());
                }
                this.submit = Some(submit_flow_from_validation(verdict, review_id, outcome));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// [Submit to GitHub] click on the `Confirming` panel: the actual `gh
    /// api .../reviews` POST, then the local writeback (fresh-load,
    /// `Submitted` state, `remote.submitted_review_id`/`url`, emit
    /// `ReviewChanged`) — all off-thread. This is the one step in the whole
    /// flow that actually sends anything. Operates on `prep.review_id`
    /// throughout — never `self.review` — so it can never target a
    /// different review than the one actually validated (review finding
    /// P1-2). Bumps `submit_epoch` on the Confirming→Submitting transition
    /// like every other UI-initiated `self.submit` assignment; nothing else
    /// can transition `self.submit` while `Submitting` is active
    /// (`on_verdict_clicked` refuses whenever `self.submit.is_some()`,
    /// `open_pr`/`cancel_submit_flow` both refuse/no-op on `Submitting` —
    /// see their own comments, and Escape is blocked the same way), so this
    /// method's own completion is the only thing that can ever land once
    /// captured here — it still re-checks the epoch before touching state,
    /// purely for uniformity with `start_submit_validation`'s completion
    /// and as a regression tripwire, not because a real race is possible.
    fn on_submit_click(&mut self, cx: &mut Context<Self>) {
        let (verdict, prep) = match &self.submit {
            Some(SubmitFlow::Confirming { verdict, prep }) => (*verdict, prep.clone()),
            _ => return,
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let location = self.location.clone();

        self.submit_epoch += 1;
        let epoch = self.submit_epoch;
        self.submit = Some(SubmitFlow::Submitting { verdict });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result: Result<(dv_core::Review, String), String> = cx
                .background_executor()
                .spawn(async move {
                    let store = dv_core::ReviewStore::open(location);
                    // Fresh-load by the id `prep` was actually validated
                    // against, BEFORE anything is sent: missing here is a
                    // clean, nothing-sent abort (the review vanished before
                    // the POST), distinct from a missing review turning up
                    // AFTER the POST inside `writeback_submitted_review`
                    // below (GitHub already has it by then — the
                    // duplicate-trap message, not this one).
                    if store
                        .load(&prep.review_id)
                        .map_err(|err| format!("{err:#}"))?
                        .is_none()
                    {
                        return Err(format!(
                            "review {} no longer exists — nothing was sent to GitHub",
                            prep.review_id
                        ));
                    }

                    let client = submit::github_client(&repo).map_err(|err| format!("{err:#}"))?;
                    let submitted = client
                        .submit_review(prep.pr_number, &prep.submission)
                        .map_err(|err| {
                            format!(
                                "{err}\n(hint: the PR may have been force-pushed since \
                                 validation — try again to re-check)"
                            )
                        })?;
                    let remote = dv_core::RemoteRef {
                        provider: "github".to_string(),
                        slug: client.slug().to_string(),
                        pr: prep.pr_number,
                        url: prep.pr_url,
                        submitted_review_id: Some(submitted.id),
                        submitted_url: Some(submitted.html_url.clone()),
                    };
                    // GitHub has now accepted the review — any failure from
                    // here on is the duplicate-trap case
                    // (`writeback_submitted_review`'s own message says so).
                    let fresh = submit::writeback_submitted_review(
                        &store,
                        &prep.review_id,
                        verdict,
                        prep.pr_number,
                        remote,
                        &submitted,
                    )?;
                    Ok((fresh, submitted.html_url))
                })
                .await;

            this.update(cx, |this, cx| {
                if this.submit_epoch != epoch {
                    return;
                }
                let (fresh_review, flow) = submit_flow_from_submission(verdict, result);
                if let Some(fresh) = fresh_review {
                    this.review = Some(fresh);
                    cx.emit(ReviewChanged);
                }
                this.submit = Some(flow);
                this.reset_diff_list(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Summary-panel click: show the comment's file and scroll its thread
    /// into view (deferred until the file's rows exist).
    fn jump_to_comment(&mut self, comment_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self
            .review
            .as_ref()
            .and_then(|r| r.comments.iter().find(|c| c.id == comment_id))
            .map(|c| c.path.clone())
        else {
            return;
        };
        let Some(file) = self.files.iter().position(|f| f.path == path) else {
            return; // comment on a file outside this diff (see backlog)
        };
        self.pending_jump = Some(comment_id);
        self.select_file(file, window, cx);
        // If the diff was already cached, the jump consumed inside
        // select_file's reset; otherwise it fires when the compute lands.
    }

    /// Persist the comment under composition: ensure a draft review exists,
    /// anchor the comment to the current blob, save — all off-thread (the
    /// store may do subprocess I/O into WSL).
    fn submit_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        if editor.saving {
            return;
        }
        let Some(sel) = self.selection else {
            return;
        };
        let Some(file) = self.selected else {
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let body = trim_trailing_newlines(editor.input.read(cx).value().to_string());
        if body.trim().is_empty() {
            self.close_editor(window, cx);
            return;
        }
        editor.saving = true;
        cx.notify();

        // The selection is made against the selected file by construction
        // (select_file drops it on switch); refuse to save if they ever
        // disagree rather than persist a comment on the wrong file.
        if sel.file != file {
            self.close_editor(window, cx);
            return;
        }

        let path = self.files[file].path.clone();
        let side = match sel.side {
            DiffSide::Old => dv_core::Side::Old,
            DiffSide::New => dv_core::Side::New,
        };
        let (start, end) = sel.range();
        let location = self.location.clone();
        let source = self.source.clone();
        let existing = self.review.clone();
        // Resolved once at load (`crate::author::resolve_author`); falls
        // back to the same placeholder the CLI defaults to until it lands.
        let author = self.author.clone().unwrap_or_else(|| "human".to_string());

        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let store = dv_core::ReviewStore::open(location);
                    // Never trust the UI's clone: a CLI write can land while
                    // this task runs (there's a git subprocess below), and
                    // saving the stale clone would erase it — unrepairable,
                    // since the watcher's reload then agrees with our write.
                    // Re-load the freshest state and mutate that.
                    let mut review = match &existing {
                        Some(known) => {
                            let fresh = store
                                .load(&known.id)
                                .ok()
                                .flatten()
                                .unwrap_or_else(|| known.clone());
                            if matches!(fresh.state, dv_core::ReviewState::Submitted { .. }) {
                                // The active review has already been
                                // submitted — to GitHub, or finished
                                // locally via `submit_review` — so
                                // appending here would strand the
                                // comment where the verdict bar hides it
                                // and the CLI refuses to touch it
                                // (review finding P1, mutation-guard
                                // half). Start a fresh draft instead,
                                // carrying over the PR linkage (minus
                                // the submission ids) when there was
                                // one — this is exactly what
                                // `submit_review`'s doc comment already
                                // promises ("the next comment
                                // auto-creates a fresh draft"), made to
                                // actually hold.
                                let mut draft = store.create(source.clone())?;
                                if let Some(remote) = &fresh.remote {
                                    draft.remote = Some(dv_core::RemoteRef {
                                        submitted_review_id: None,
                                        submitted_url: None,
                                        ..remote.clone()
                                    });
                                    store.save(&draft)?;
                                }
                                draft
                            } else {
                                fresh
                            }
                        }
                        None => match store
                            .list()
                            .unwrap_or_default()
                            .into_iter()
                            .find(|r| matches!(r.state, dv_core::ReviewState::Draft))
                        {
                            Some(fresh) => fresh,
                            None => store.create(source.clone())?,
                        },
                    };
                    // Anchor against the review's own source — it may have
                    // been created by the CLI over a different one.
                    let sha = repo
                        .blob_sha(&anchor_spec(&review.source, side, &path))
                        .ok()
                        .flatten();
                    review.add_comment(path, side, start, end, sha, body, author)?;
                    store.save(&review)?;
                    anyhow::Ok(review)
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(review) => {
                        this.review = Some(review);
                        cx.emit(ReviewChanged);
                        // A new comment can invalidate a parked submit
                        // panel's "nothing to submit" / comment count
                        // (review finding P1-3).
                        this.cancel_submit_flow_if_parked(cx);
                        this.close_editor(window, cx);
                    }
                    Err(err) => {
                        // Keep the editor (and the typed body) so nothing is
                        // lost; surface the failure in the card.
                        eprintln!("comment save failed: {err:#}");
                        if let Some(editor) = &mut this.editor {
                            editor.saving = false;
                        }
                    }
                }
                this.reset_diff_list(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Toggle a comment's open/resolved status (by id), persisting
    /// off-thread.
    fn set_comment_status(
        &mut self,
        comment_id: String,
        status: dv_core::CommentStatus,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = self.review.clone() else {
            return;
        };
        let location = self.location.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let store = dv_core::ReviewStore::open(location);
                    // Fresh-load before mutating (see submit_comment).
                    let mut review = store.load(&review.id).ok().flatten().unwrap_or(review);
                    review.set_status(&comment_id, status)?;
                    store.save(&review)?;
                    anyhow::Ok(review)
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(review) => {
                        this.review = Some(review);
                        cx.emit(ReviewChanged);
                        // Resolving/reopening a comment can invalidate a
                        // parked submit panel (review finding P1-3).
                        this.cancel_submit_flow_if_parked(cx);
                    }
                    Err(err) => eprintln!("comment status update failed: {err:#}"),
                }
                this.reset_diff_list(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Delete a comment (by id), persisting off-thread.
    fn delete_comment(&mut self, comment_id: String, cx: &mut Context<Self>) {
        let Some(review) = self.review.clone() else {
            return;
        };
        let location = self.location.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let store = dv_core::ReviewStore::open(location);
                    // Fresh-load before mutating (see submit_comment).
                    let mut review = store.load(&review.id).ok().flatten().unwrap_or(review);
                    review.comments.retain(|c| c.id != comment_id);
                    review.updated_ms = dv_core::review::now_ms();
                    store.save(&review)?;
                    anyhow::Ok(review)
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(review) => {
                        this.review = Some(review);
                        cx.emit(ReviewChanged);
                        // Deleting a comment can invalidate a parked submit
                        // panel (review finding P1-3).
                        this.cancel_submit_flow_if_parked(cx);
                    }
                    Err(err) => eprintln!("comment delete failed: {err:#}"),
                }
                this.reset_diff_list(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open a reply (or body-edit) input inside a thread card. Only one is
    /// open at a time.
    fn open_thread_input(
        &mut self,
        comment_id: String,
        mode: ThreadInputMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_component::input::{InputEvent, InputState};
        // An in-flight save keeps its input alive: replacing it here would
        // drop the pending body (and the completion would close the NEW
        // input). Finish or fail first.
        if self.thread_input.as_ref().is_some_and(|ti| ti.saving) {
            return;
        }
        let prefill = match mode {
            ThreadInputMode::Reply => String::new(),
            ThreadInputMode::EditBody => self
                .review
                .as_ref()
                .and_then(|r| r.comments.iter().find(|c| c.id == comment_id))
                .map(|c| c.body.clone())
                .unwrap_or_default(),
        };
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).multi_line(true).auto_grow(2, 8);
            state = match mode {
                ThreadInputMode::Reply => {
                    state.placeholder("Reply… (ctrl-enter to send, esc to cancel)")
                }
                ThreadInputMode::EditBody => state,
            };
            state
        });
        if !prefill.is_empty() {
            input.update(cx, |input, cx| input.set_value(prefill, window, cx));
        }
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter {
                    secondary: true, ..
                } = event
                {
                    this.submit_thread_input(window, cx);
                }
            });
        input.update(cx, |input, cx| input.focus(window, cx));
        self.thread_input = Some(ThreadInput {
            comment_id,
            mode,
            input,
            saving: false,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn close_thread_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.thread_input.take().is_some() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    /// Persist the open reply/edit, fresh-loading the review first (see
    /// submit_comment for why the UI clone can't be trusted).
    fn submit_thread_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ti) = &mut self.thread_input else {
            return;
        };
        if ti.saving {
            return;
        }
        let Some(review) = self.review.clone() else {
            return;
        };
        let body = trim_trailing_newlines(ti.input.read(cx).value().to_string());
        if body.trim().is_empty() {
            self.close_thread_input(window, cx);
            return;
        }
        ti.saving = true;
        cx.notify();

        let comment_id = ti.comment_id.clone();
        let done_id = ti.comment_id.clone();
        let mode = ti.mode;
        let location = self.location.clone();
        let author = self.author.clone().unwrap_or_else(|| "human".to_string());
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let store = dv_core::ReviewStore::open(location);
                    let mut review = store.load(&review.id).ok().flatten().unwrap_or(review);
                    match mode {
                        ThreadInputMode::Reply => {
                            review.reply(&comment_id, body, author)?;
                        }
                        ThreadInputMode::EditBody => {
                            let comment = review
                                .comments
                                .iter_mut()
                                .find(|c| c.id == comment_id)
                                .ok_or_else(|| {
                                anyhow::anyhow!("comment {comment_id} is gone")
                            })?;
                            comment.body = body;
                            comment.updated_ms = dv_core::review::now_ms();
                            review.updated_ms = comment.updated_ms;
                        }
                    }
                    store.save(&review)?;
                    anyhow::Ok(review)
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                // Only touch the input this save belongs to — the user may
                // have opened a different one meanwhile.
                let same_input = this
                    .thread_input
                    .as_ref()
                    .is_some_and(|ti| ti.comment_id == done_id && ti.mode == mode);
                match result {
                    Ok(review) => {
                        this.review = Some(review);
                        cx.emit(ReviewChanged);
                        // A reply or body edit can invalidate a parked
                        // submit panel (review finding P1-3).
                        this.cancel_submit_flow_if_parked(cx);
                        if same_input {
                            this.close_thread_input(window, cx);
                        }
                    }
                    Err(err) => {
                        eprintln!("thread input save failed: {err:#}");
                        if same_input && let Some(ti) = &mut this.thread_input {
                            ti.saving = false;
                        }
                    }
                }
                this.reset_diff_list(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The side+line a unified row anchors to: New wherever the line exists
    /// on the new side, Old only for pure removals — matching GitHub.
    fn row_anchor(old_line: Option<u32>, new_line: Option<u32>) -> Option<(DiffSide, u32)> {
        match (old_line, new_line) {
            (_, Some(n)) => Some((DiffSide::New, n)),
            (Some(o), None) => Some((DiffSide::Old, o)),
            (None, None) => None,
        }
    }

    // ---- Jump-to-file palette ----------------------------------------

    fn on_jump_to_file(&mut self, _: &JumpToFile, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::{InputEvent, InputState};
        if self.files.is_empty() {
            return;
        }
        if let Some(palette) = &self.palette {
            // Already open — just refocus the input.
            let input = palette.input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }

        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Jump to file…"));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    let query = input.read(cx).value().to_string();
                    if let Some(palette) = &mut this.palette {
                        palette.matches =
                            crate::fuzzy::rank(&query, this.files.iter().map(|f| f.path.as_str()));
                        palette.selected = 0;
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.palette_choose(window, cx),
                InputEvent::Blur => this.close_palette(window, cx),
                InputEvent::Focus => {}
            },
        );

        input.update(cx, |input, cx| input.focus(window, cx));
        self.palette = Some(Palette {
            input,
            matches: (0..self.files.len()).collect(),
            selected: 0,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn palette_choose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(palette) = &self.palette else {
            return;
        };
        let file = palette.matches.get(palette.selected).copied();
        self.close_palette(window, cx);
        if let Some(file) = file {
            self.select_file(file, window, cx);
        }
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    fn on_palette_next(&mut self, _: &PaletteNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = &mut self.palette
            && !palette.matches.is_empty()
        {
            palette.selected = (palette.selected + 1).min(palette.matches.len() - 1);
            cx.notify();
        }
    }

    fn on_palette_prev(&mut self, _: &PalettePrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = &mut self.palette {
            palette.selected = palette.selected.saturating_sub(1);
            cx.notify();
        }
    }

    fn on_palette_close(&mut self, _: &PaletteClose, window: &mut Window, cx: &mut Context<Self>) {
        self.close_palette(window, cx);
    }

    /// The jump-to-file palette overlay, when open: centered near the top,
    /// a query input above the ranked matches.
    fn render_palette(&self, cx: &mut Context<Self>) -> Option<Div> {
        const VISIBLE: usize = 12;
        let palette = self.palette.as_ref()?;
        let theme = cx.theme();

        // Keep the selection visible within the capped row window.
        let first = palette.selected.saturating_sub(VISIBLE - 1);
        let rows: Vec<Stateful<Div>> = palette
            .matches
            .iter()
            .enumerate()
            .skip(first)
            .take(VISIBLE)
            .map(|(match_ix, &file_ix)| {
                let selected = match_ix == palette.selected;
                div()
                    .id(("palette-row", match_ix))
                    .w_full()
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |el| el.bg(theme.accent))
                    .hover(|el| el.bg(theme.accent.opacity(0.5)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            if let Some(palette) = &mut this.palette {
                                palette.selected = match_ix;
                            }
                            this.palette_choose(window, cx);
                        }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .truncate()
                            .child(self.files[file_ix].path.clone()),
                    )
            })
            .collect();

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
                        .w(px(560.))
                        .max_w_full()
                        .p_2()
                        .gap_2()
                        // Swallow clicks on the popover chrome (padding,
                        // gaps): otherwise they bubble to the workspace
                        // root's focus-on-mousedown, blurring the input and
                        // closing the palette out from under the user.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .bg(theme.popover)
                        .text_color(theme.popover_foreground)
                        .border_1()
                        .border_color(theme.border)
                        .rounded_lg()
                        .shadow_lg()
                        .child(gpui_component::input::Input::new(&palette.input))
                        .child(v_flex().w_full().children(rows).when(
                            palette.matches.is_empty(),
                            |el| {
                                el.child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child("no matching files"),
                                )
                            },
                        )),
                ),
        )
    }

    /// The compact PR header band, shown above the diff area whenever a PR
    /// is open (docs/phase-3-github.md deliverable 2): `#N title`, author,
    /// `base ← head`, a state chip, a CI dot, the review decision, and a
    /// "details" toggle that expands the PR body underneath.
    fn render_pr_header(&self, cx: &mut Context<Self>) -> Option<Div> {
        use gpui_component::Sizable as _;
        use gpui_component::button::{Button, ButtonVariants as _};
        let pr = self.pr.as_ref()?;

        // Owned copies before any `cx.listener`/`Button` setup below — see
        // CLAUDE.md's gpui gotcha (and `render_summary`) on why holding
        // `&Theme` across those would be a borrow conflict.
        let theme = cx.theme();
        let border = theme.border;
        let band_bg = theme.secondary;
        let muted = theme.muted_foreground;
        let foreground = theme.foreground;
        let success = theme.success;
        let danger = theme.danger;
        let warning = theme.warning;
        // `accent_foreground` (near-white) reads as barely distinct from
        // DRAFT's gray chip. The Aura theme has no dedicated magenta/violet
        // token, but `primary` (`#a277ff`, a violet) already *is* the
        // theme's accent color and isn't used by any other chip/glyph in
        // this header — closest match to GitHub's purple "Merged" badge.
        let merged = theme.primary;

        let (chip_label, chip_color) = if pr.is_draft {
            ("DRAFT", muted)
        } else {
            match pr.state {
                PrState::Open => ("OPEN", success),
                PrState::Merged => ("MERGED", merged),
                PrState::Closed => ("CLOSED", danger),
            }
        };
        let checks_glyph = match pr.checks {
            ChecksSummary::Passing => Some(("\u{2713}", success)),
            ChecksSummary::Failing => Some(("\u{2717}", danger)),
            ChecksSummary::Pending => Some(("\u{25cf}", warning)),
            ChecksSummary::None => None,
        };
        let decision = pr.review_decision.map(|d| match d {
            ReviewDecision::Approved => ("approved", success),
            ReviewDecision::ChangesRequested => ("changes requested", danger),
            ReviewDecision::ReviewRequired => ("review required", muted),
        });

        let number = pr.number;
        let title = pr.title.clone();
        let base_head = format!("{} \u{2190} {}", pr.base_ref, pr.head_ref);
        let author = pr.author.clone();
        let details_open = self.pr_details_open;
        let body_text = pr.body.clone();
        let url = pr.url.clone();

        Some(
            v_flex()
                .w_full()
                .flex_none()
                .border_b_1()
                .border_color(border)
                .bg(band_bg)
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1p5()
                        .child(
                            div()
                                .flex_none()
                                .font_semibold()
                                .child(format!("#{number}")),
                        )
                        .child(div().flex_1().min_w(px(0.)).truncate().child(title))
                        .child(
                            div()
                                .flex_none()
                                .px_1p5()
                                .rounded_full()
                                .text_xs()
                                .bg(chip_color.opacity(0.16))
                                .text_color(chip_color)
                                .child(chip_label),
                        )
                        .children(
                            checks_glyph.map(|(glyph, color)| {
                                div().flex_none().text_color(color).child(glyph)
                            }),
                        )
                        .children(decision.map(|(label, color)| {
                            div().flex_none().text_xs().text_color(color).child(label)
                        }))
                        .child(
                            div()
                                .flex_none()
                                .text_sm()
                                .text_color(muted)
                                .child(base_head),
                        )
                        .child(div().flex_none().text_sm().text_color(muted).child(author))
                        .child(
                            Button::new("pr-details-toggle")
                                .ghost()
                                .small()
                                .label(if details_open {
                                    "hide details"
                                } else {
                                    "details"
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.pr_details_open = !this.pr_details_open;
                                    cx.notify();
                                })),
                        ),
                )
                .when(details_open, |el| {
                    el.child(
                        v_flex()
                            .id("pr-body")
                            .w_full()
                            .max_h(px(200.))
                            .overflow_y_scroll()
                            .px_3()
                            .py_2()
                            .gap_1()
                            .border_t_1()
                            .border_color(border)
                            .child(
                                div()
                                    .text_sm()
                                    .whitespace_normal()
                                    .text_color(foreground)
                                    .child(if body_text.trim().is_empty() {
                                        SharedString::from("(no description)")
                                    } else {
                                        body_text
                                    }),
                            )
                            .child(div().text_xs().text_color(muted).child(url)),
                    )
                }),
        )
    }

    /// The PR picker overlay (`ctrl-g`), when open: same positioning/chrome
    /// as [`Self::render_palette`], but no text input — a background `gh pr
    /// list` call and up/down/enter/escape over whatever it returns.
    fn render_pr_picker(&self, cx: &mut Context<Self>) -> Option<Div> {
        let picker = self.pr_picker.as_ref()?;

        let theme = cx.theme();
        let border = theme.border;
        let popover = theme.popover;
        let popover_fg = theme.popover_foreground;
        let accent = theme.accent;
        let muted = theme.muted_foreground;
        let danger = theme.danger;
        let mono = theme.mono_font_family.clone();

        let body: AnyElement =
            match &picker.state {
                PrPickerState::Loading => div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(muted)
                    .child("loading…")
                    .into_any_element(),
                PrPickerState::Error(err) => div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(danger)
                    .child(err.clone())
                    .into_any_element(),
                PrPickerState::Loaded(prs) if prs.is_empty() => div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(muted)
                    .child("no open PRs")
                    .into_any_element(),
                PrPickerState::Loaded(prs) => {
                    // No scroll container backs this overlay (unlike the diff
                    // pane's virtualized list) — cap the rendered window around
                    // the selection, same fixed-count trick `render_palette`
                    // uses, rather than relying on CSS overflow clipping (a
                    // `max_h` alone doesn't clip painted content in gpui; it
                    // just bounds layout, so an unwindowed list bleeds into
                    // whatever renders underneath it).
                    const VISIBLE: usize = 12;
                    let first = picker.selected.saturating_sub(VISIBLE - 1);
                    v_flex()
                        .w_full()
                        .children(prs.iter().enumerate().skip(first).take(VISIBLE).map(
                            |(i, pr)| {
                                let selected = i == picker.selected;
                                let number = pr.number;
                                h_flex()
                                    .id(("pr-picker-row", i))
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
                                            this.open_pr(number, window, cx);
                                        }),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .font_family(mono.clone())
                                            .text_color(muted)
                                            .child(format!("#{}", pr.number)),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .text_sm()
                                            .truncate()
                                            .child(pr.title.clone()),
                                    )
                                    .when(pr.is_draft, |el| {
                                        el.child(
                                            div()
                                                .flex_none()
                                                .text_xs()
                                                .text_color(muted)
                                                .child("draft"),
                                        )
                                    })
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(format!("by {}", pr.author)),
                                    )
                            },
                        ))
                        .into_any_element()
                }
            };

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
                        .w(px(560.))
                        .max_w_full()
                        .max_h(px(420.))
                        .overflow_hidden()
                        .p_2()
                        .gap_2()
                        // Same swallow-the-click-on-chrome reasoning as
                        // `render_palette`.
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
                                .child("Open PRs \u{b7} enter to open, esc to close"),
                        )
                        .child(body),
                ),
        )
    }

    fn render_file_row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let file = &self.files[index];
        let theme = cx.theme();
        let selected = self.selected == Some(index);
        let (glyph, color) = match file.status {
            ChangeStatus::Added => ("A", theme.success),
            ChangeStatus::Deleted => ("D", theme.danger),
            ChangeStatus::Renamed => ("R", theme.info),
            ChangeStatus::Copied => ("C", theme.info),
            ChangeStatus::Modified => ("M", theme.warning),
            ChangeStatus::TypeChanged => ("T", theme.warning),
            ChangeStatus::Unmerged => ("U", theme.danger),
            ChangeStatus::Unknown(_) => ("?", theme.muted_foreground),
        };
        let label: SharedString = match &file.old_path {
            Some(old) => format!("{old} → {}", file.path).into(),
            None => file.path.clone().into(),
        };

        h_flex()
            .id(index)
            .gap_2()
            .px_2()
            .py_0p5()
            .w_full()
            .overflow_hidden()
            .when(selected, |el| el.bg(theme.accent))
            .hover(|el| el.bg(theme.accent.opacity(0.6)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| this.select_file(index, window, cx)),
            )
            .child(
                div()
                    .w_4()
                    .flex_none()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(color)
                    .child(glyph),
            )
            .child(div().text_sm().truncate().child(label))
    }

    /// A hunk header row (shared by both views). When context is hidden
    /// above the hunk, the row is clickable and says how much it reveals.
    fn render_hunk_header(
        &self,
        label: SharedString,
        hunk: usize,
        expandable: Option<u32>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme();
        let base = h_flex()
            .id(("hunk-header", hunk))
            .w_full()
            .h(px(row_height(self.font_size)))
            .px_2()
            .bg(theme.muted)
            .font_family(theme.mono_font_family.clone())
            .text_size(px(self.font_size))
            .text_color(theme.muted_foreground);
        match expandable {
            None => base.child(label),
            Some(hidden) => {
                let accent = theme.primary;
                base.cursor_pointer()
                    .hover(|el| el.bg(theme.accent.opacity(0.4)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.expand_hunk_gap(hunk, cx)),
                    )
                    .child(h_flex().gap_2().child(label).child(
                        div().text_color(accent.opacity(0.9)).child(format!(
                            "⌃ {hidden} hidden line{} — click to expand",
                            if hidden == 1 { "" } else { "s" }
                        )),
                    ))
            }
        }
    }

    /// The review summary panel: every thread across files, filterable,
    /// click to jump, with the finish-review verdict at the bottom.
    fn render_summary(&self, cx: &mut Context<Self>) -> Option<Div> {
        use gpui_component::Selectable as _;
        use gpui_component::Sizable as _;
        use gpui_component::button::{Button, ButtonVariants as _};
        if !self.summary_open {
            return None;
        }
        // Owned copies: holding &Theme across the &mut cx listener setups
        // below would be a borrow conflict (same pattern as render_split_row).
        let border = cx.theme().border;
        let sidebar_bg = cx.theme().sidebar;
        let muted = cx.theme().muted_foreground;
        let success = cx.theme().success;
        let primary = cx.theme().primary;
        let warning = cx.theme().warning;
        let accent = cx.theme().accent;
        let review = self.review.as_ref();
        let comments: Vec<(usize, &dv_core::Comment)> = review
            .map(|r| {
                r.comments
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| match self.summary_filter {
                        SummaryFilter::All => true,
                        SummaryFilter::Open => c.status == dv_core::CommentStatus::Open,
                        SummaryFilter::Resolved => c.status == dv_core::CommentStatus::Resolved,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let open_count = review
            .map(|r| {
                r.comments
                    .iter()
                    .filter(|c| c.status == dv_core::CommentStatus::Open)
                    .count()
            })
            .unwrap_or(0);
        let total = review.map(|r| r.comments.len()).unwrap_or(0);
        let submitted = review.and_then(|r| match &r.state {
            dv_core::ReviewState::Draft => None,
            dv_core::ReviewState::Submitted { verdict, .. } => Some(*verdict),
        });

        let filter_button = |label: &'static str,
                             value: SummaryFilter,
                             current: SummaryFilter,
                             cx: &mut Context<Self>| {
            Button::new(("summary-filter", value as usize))
                .ghost()
                .small()
                .selected(value == current)
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.summary_filter = value;
                    cx.notify();
                }))
        };

        let stale = &self.stale;
        let rows: Vec<Stateful<Div>> = comments
            .iter()
            .map(|(_, comment)| {
                let id = comment.id.clone();
                let resolved = comment.status == dv_core::CommentStatus::Resolved;
                let first_line = comment.body.lines().next().unwrap_or("").to_string();
                div()
                    .id(SharedString::from(format!("summary-{}", comment.id)))
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|el| el.bg(accent.opacity(0.5)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.jump_to_comment(id.clone(), window, cx);
                        }),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(muted)
                            .child(
                                div()
                                    .text_color(if resolved { success } else { primary })
                                    .child(if resolved { "\u{25cf}" } else { "\u{25cb}" }),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .child(format!("{}:{}", comment.path, comment.start_line)),
                            )
                            .when(stale.contains(&comment.id), |el| {
                                el.child(div().text_color(warning).child("\u{26a0}"))
                            }),
                    )
                    .child(div().text_sm().truncate().child(first_line))
            })
            .collect();

        Some(
            v_flex()
                .h_full()
                .w(px(self.summary_width))
                .flex_none()
                .relative()
                .border_l_1()
                .border_color(border)
                .bg(sidebar_bg)
                .child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .child(div().font_semibold().child("Review"))
                        .child(div().text_color(muted).text_sm().child(format!(
                            "{open_count} open \u{b7} {} resolved",
                            total - open_count
                        ))),
                )
                .child(
                    h_flex()
                        .px_2()
                        .gap_1()
                        .child(filter_button(
                            "All",
                            SummaryFilter::All,
                            self.summary_filter,
                            cx,
                        ))
                        .child(filter_button(
                            "Open",
                            SummaryFilter::Open,
                            self.summary_filter,
                            cx,
                        ))
                        .child(filter_button(
                            "Resolved",
                            SummaryFilter::Resolved,
                            self.summary_filter,
                            cx,
                        )),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_hidden()
                        .p_1()
                        .gap_1()
                        .children(rows)
                        .when(comments.is_empty(), |el| {
                            el.child(
                                div().p_3().text_sm().text_color(muted).child(
                                    "No comments yet \u{2014} click a line number to start.",
                                ),
                            )
                        }),
                )
                .child(self.render_verdict_area(submitted, border, success, muted, cx))
                .child(self.render_summary_resize_handle(cx)),
        )
    }

    /// The review-summary panel's inner (*left*) edge drag handle (Phase 4
    /// deliverable 5) — mirror image of `shell.rs`'s sidebar handle (see its
    /// doc comment for why gpui's `on_drag`/`on_drag_move` was chosen over
    /// gpui-component's `resizable` module), but computing width off the
    /// panel's distance from the *right* edge of the window
    /// (`viewport_width - mouse.x`) since the panel is flush against the
    /// window's right edge, whereas the sidebar is flush against its left.
    ///
    /// Live-updates `self.summary_width` directly on every drag-move frame
    /// (immediate visual feedback, no `Settings` round trip mid-drag);
    /// release (or a double-click reset) emits [`SummaryWidthChanged`] so
    /// the shell — which owns `Settings` — can persist it (this workspace
    /// has no access to the settings store itself).
    fn render_summary_resize_handle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = cx.theme().primary;
        div()
            .id("summary-resize-handle")
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(-3.))
            .w(px(6.))
            .occlude()
            .cursor_col_resize()
            .group("summary-resize-handle")
            .on_drag(SummaryResizeDrag, |_, _, _, cx| cx.new(|_| EmptyView))
            .on_drag_move::<SummaryResizeDrag>(cx.listener(
                |this, event: &DragMoveEvent<SummaryResizeDrag>, window, cx| {
                    let viewport_w = f32::from(window.viewport_size().width);
                    let mouse_x = f32::from(event.event.position.x);
                    this.summary_dragging = true;
                    this.summary_width =
                        (viewport_w - mouse_x).clamp(SUMMARY_WIDTH_MIN, SUMMARY_WIDTH_MAX);
                    cx.notify();
                },
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    // Double-click resets to the default width.
                    if event.click_count >= 2 {
                        this.summary_width = crate::settings::DEFAULT_SUMMARY_WIDTH;
                        cx.emit(SummaryWidthChanged(this.summary_width));
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if std::mem::take(&mut this.summary_dragging) {
                        cx.emit(SummaryWidthChanged(this.summary_width));
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    // The common case: a drag almost always ends with the
                    // cursor well outside this 6px strip. Gated on the drag
                    // flag — up_out fires for EVERY outside release.
                    if std::mem::take(&mut this.summary_dragging) {
                        cx.emit(SummaryWidthChanged(this.summary_width));
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
                    .group_hover("summary-resize-handle", move |el| el.bg(accent)),
            )
    }

    /// The bottom of the summary panel: either the Phase-3 GitHub submit
    /// flow (`self.submit`, once the review is PR-linked and a verdict was
    /// clicked — see [`SubmitFlow`]) or the pre-existing local-only
    /// "Finish review" verdict bar / "Submitted" caption. A local-only
    /// review's behavior here is untouched: `self.submit` never becomes
    /// `Some` for it (see [`Self::on_verdict_clicked`]).
    fn render_verdict_area(
        &self,
        submitted: Option<dv_core::Verdict>,
        border: Hsla,
        success: Hsla,
        muted: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use gpui_component::Sizable as _;
        use gpui_component::button::{Button, ButtonVariants as _};

        if let Some(flow) = &self.submit {
            return self.render_submit_flow(flow, border, success, muted, cx);
        }

        match submitted {
            Some(verdict) => h_flex()
                .p_3()
                .border_t_1()
                .border_color(border)
                .gap_2()
                .text_sm()
                .child(
                    div()
                        .text_color(success)
                        .child(format!("Submitted \u{b7} {}", verdict_label(verdict))),
                )
                .into_any_element(),
            None => v_flex()
                .p_3()
                .gap_2()
                .border_t_1()
                .border_color(border)
                .child(div().text_sm().child("Finish review"))
                .child(
                    // `w_full()` + `flex_wrap()` (review finding P3-1): at
                    // the panel's w320, three buttons — one of them
                    // "Request changes" — don't reliably fit on one row at
                    // the default button size; `small()` buys back most of
                    // that width, and the wrap is a safety net so a button
                    // spills onto a second line instead of clipping off the
                    // right edge if it still doesn't fit (longer verdict
                    // labels, a wider font, ...).
                    h_flex()
                        .w_full()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            Button::new("verdict-comment")
                                .small()
                                .ghost()
                                .label("Comment")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.on_verdict_clicked(dv_core::Verdict::Comment, cx)
                                })),
                        )
                        .child(
                            Button::new("verdict-approve")
                                .small()
                                .primary()
                                .label("Approve")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.on_verdict_clicked(dv_core::Verdict::Approve, cx)
                                })),
                        )
                        .child(
                            Button::new("verdict-request")
                                .small()
                                .danger()
                                .label("Request changes")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.on_verdict_clicked(dv_core::Verdict::RequestChanges, cx)
                                })),
                        ),
                )
                .into_any_element(),
        }
    }

    /// The six [`SubmitFlow`] stages, rendered as the summary panel's
    /// bottom section in place of the plain local verdict bar.
    fn render_submit_flow(
        &self,
        flow: &SubmitFlow,
        border: Hsla,
        success: Hsla,
        muted: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use gpui_component::Sizable as _;
        use gpui_component::button::{Button, ButtonVariants as _};
        let danger = cx.theme().danger;

        let dismiss_button = |id: &'static str, cx: &mut Context<Self>| {
            Button::new(id)
                .ghost()
                .small()
                .label("Dismiss")
                .on_click(cx.listener(|this, _, _, cx| {
                    // Routed through `cancel_submit_flow` (not a bare
                    // `self.submit = None`) so this also bumps
                    // `submit_epoch` like every other UI-initiated
                    // transition (review finding P1-1).
                    this.cancel_submit_flow(cx);
                }))
        };

        match flow {
            SubmitFlow::Validating { .. } => h_flex()
                .p_3()
                .border_t_1()
                .border_color(border)
                .text_sm()
                .text_color(muted)
                .child("Checking the PR and your comments\u{2026}")
                .into_any_element(),

            SubmitFlow::Blocked { violations, .. } => v_flex()
                .p_3()
                .gap_2()
                .border_t_1()
                .border_color(border)
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(danger)
                        .child(format!(
                            "Can't submit \u{2014} {} problem{} found",
                            violations.len(),
                            if violations.len() == 1 { "" } else { "s" },
                        )),
                )
                .child(
                    v_flex()
                        .id("submit-violations")
                        .gap_1()
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .children(violations.iter().map(|v| {
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("{}:{} \u{2014} {}", v.path, v.lines, v.message))
                        })),
                )
                .child(dismiss_button("submit-dismiss-blocked", cx))
                .into_any_element(),

            SubmitFlow::Confirming { verdict, prep } => {
                let count = prep.submission.comments.len();
                v_flex()
                    .p_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(border)
                    .child(div().text_sm().child(format!(
                        "Submit to GitHub: {} \u{b7} {count} comment{} to PR #{}",
                        verdict_label(*verdict),
                        if count == 1 { "" } else { "s" },
                        prep.pr_number,
                    )))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("submit-confirm")
                                    .primary()
                                    .small()
                                    .label("Submit to GitHub")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.on_submit_click(cx)),
                                    ),
                            )
                            .child(
                                Button::new("submit-cancel")
                                    .ghost()
                                    .small()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        // See the Dismiss button above: must
                                        // bump `submit_epoch` too.
                                        this.cancel_submit_flow(cx);
                                    })),
                            ),
                    )
                    .into_any_element()
            }

            SubmitFlow::Submitting { .. } => h_flex()
                .p_3()
                .border_t_1()
                .border_color(border)
                .text_sm()
                .text_color(muted)
                .child("Submitting to GitHub\u{2026}")
                .into_any_element(),

            SubmitFlow::Done { verdict, url } => v_flex()
                .p_3()
                .gap_1()
                .border_t_1()
                .border_color(border)
                .text_sm()
                .child(div().text_color(success).child(format!(
                    "Submitted \u{b7} {} \u{2713}",
                    verdict_label(*verdict)
                )))
                .child(div().text_xs().text_color(muted).child(url.clone()))
                .into_any_element(),

            SubmitFlow::Failed { message, .. } => v_flex()
                .p_3()
                .gap_2()
                .border_t_1()
                .border_color(border)
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(danger)
                        .child("Submission failed"),
                )
                .child(
                    div()
                        .id("submit-failed-message")
                        .text_xs()
                        .text_color(muted)
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .child(message.clone()),
                )
                .child(dismiss_button("submit-dismiss-failed", cx))
                .into_any_element(),
        }
    }

    /// One display row: a diff row (per view mode), an inline comment
    /// thread, or the comment editor.
    fn render_display_row(&self, display_ix: usize, cx: &mut Context<Self>) -> AnyElement {
        match self.display.get(display_ix) {
            Some(&DisplayRow::Diff(row)) => match self.view_mode {
                ViewMode::Unified => self.render_diff_row(row, cx).into_any_element(),
                ViewMode::Split => self.render_split_row(row, cx).into_any_element(),
            },
            Some(&DisplayRow::Thread(comment_ix)) => {
                self.render_thread(comment_ix, cx).into_any_element()
            }
            Some(&DisplayRow::Editor) => self.render_editor(cx).into_any_element(),
            None => div().into_any_element(),
        }
    }

    /// An inline comment thread card under its anchor line.
    fn render_thread(&self, comment_ix: usize, cx: &mut Context<Self>) -> Div {
        use gpui_component::Sizable as _;
        use gpui_component::button::{Button, ButtonVariants as _};
        let theme = cx.theme();
        let Some(comment) = self
            .review
            .as_ref()
            .and_then(|r| r.comments.get(comment_ix))
        else {
            return div();
        };
        let resolved = comment.status == dv_core::CommentStatus::Resolved;
        let id = comment.id.clone();
        let id_for_delete = comment.id.clone();
        let id_for_reply = comment.id.clone();
        let id_for_edit = comment.id.clone();
        // Body is editable in place; showing the input replaces the body.
        let editing = self
            .thread_input
            .as_ref()
            .filter(|ti| ti.comment_id == comment.id)
            .map(|ti| (ti.mode, ti.saving));
        let lines = if comment.start_line == comment.end_line {
            format!("line {}", comment.start_line)
        } else {
            format!("lines {}–{}", comment.start_line, comment.end_line)
        };
        let side = match comment.side {
            dv_core::Side::Old => "old",
            dv_core::Side::New => "new",
        };

        div().w_full().px_4().py_3().child(
            v_flex()
                .w_full()
                .max_w(px(720.))
                .p_3()
                .gap_3()
                .bg(theme.popover)
                .border_1()
                .border_color(if resolved {
                    theme.border
                } else {
                    theme.primary.opacity(0.5)
                })
                .rounded_lg()
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        // Safety net for a narrow summary/thread column (same
                        // reasoning as `render_verdict_area`'s own
                        // `.flex_wrap()`: "Unresolve" is long enough that the
                        // action-button group can outgrow what's left of the
                        // row next to the author/status text) — the group
                        // spills onto its own line instead of clipping off
                        // the right edge.
                        .flex_wrap()
                        .text_sm()
                        .child(div().font_semibold().child(comment.author.clone()))
                        .child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child(format!("{side} · {lines}")),
                        )
                        .when(resolved, |el| {
                            el.child(div().text_color(theme.success).child("✓ resolved"))
                        })
                        .when(self.stale.contains(&comment.id), |el| {
                            el.child(
                                div()
                                    .text_color(theme.warning)
                                    .child("⚠ stale — the anchored content changed"),
                            )
                        })
                        .child(div().flex_1())
                        // Action-button group: tighter internal spacing than
                        // the metadata cluster to its left, so the four
                        // actions read as one cohesive group (GitHub's
                        // thread-card action-row feel) rather than just more
                        // items on the same row.
                        .child(
                            h_flex()
                                .flex_none()
                                .gap_1()
                                .child(
                                    Button::new(("resolve", comment_ix))
                                        .ghost()
                                        .small()
                                        .label(if resolved { "Unresolve" } else { "Resolve" })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let status = if resolved {
                                                dv_core::CommentStatus::Open
                                            } else {
                                                dv_core::CommentStatus::Resolved
                                            };
                                            this.set_comment_status(id.clone(), status, cx);
                                        })),
                                )
                                .child(
                                    Button::new(("reply", comment_ix))
                                        .ghost()
                                        .small()
                                        .label("Reply")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.open_thread_input(
                                                id_for_reply.clone(),
                                                ThreadInputMode::Reply,
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    Button::new(("edit", comment_ix))
                                        .ghost()
                                        .small()
                                        .label("Edit")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.open_thread_input(
                                                id_for_edit.clone(),
                                                ThreadInputMode::EditBody,
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    Button::new(("delete", comment_ix))
                                        .danger()
                                        .small()
                                        .label("Delete")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.delete_comment(id_for_delete.clone(), cx);
                                        })),
                                ),
                        ),
                )
                .map(|el| {
                    // Editing swaps the body text for the input; otherwise
                    // the body renders normally.
                    if editing.is_some_and(|(mode, _)| mode == ThreadInputMode::EditBody) {
                        el
                    } else {
                        el.child(div().text_sm().child(comment.body.clone()))
                    }
                })
                .when(!comment.replies.is_empty(), |el| {
                    // One indentation rail + tighter internal spacing for
                    // the whole reply run, distinct from the looser
                    // header/body/replies/editor rhythm above (`gap_3`) —
                    // consecutive replies are more tightly related to each
                    // other than to the sections around them.
                    el.child(
                        v_flex()
                            .gap_2()
                            .pl_3()
                            .border_l_2()
                            .border_color(theme.border)
                            .children(comment.replies.iter().map(|reply| {
                                h_flex()
                                    .gap_2()
                                    .text_sm()
                                    .child(div().font_semibold().child(reply.author.clone()))
                                    .child(div().child(reply.body.clone()))
                            })),
                    )
                })
                .when_some(editing, |el, (_, saving)| {
                    let input = self
                        .thread_input
                        .as_ref()
                        .map(|ti| gpui_component::input::Input::new(&ti.input));
                    el.children(input).child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new(("ti-cancel", comment_ix))
                                    .ghost()
                                    .small()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_thread_input(window, cx)
                                    })),
                            )
                            .child(
                                Button::new(("ti-submit", comment_ix))
                                    .primary()
                                    .small()
                                    .label(if saving { "Saving…" } else { "Save" })
                                    .disabled(saving)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_thread_input(window, cx)
                                    })),
                            ),
                    )
                }),
        )
    }

    /// The inline comment editor card.
    fn render_editor(&self, cx: &mut Context<Self>) -> Div {
        use gpui_component::Sizable as _;
        use gpui_component::button::{Button, ButtonVariants as _};
        let theme = cx.theme();
        let Some(editor) = &self.editor else {
            return div();
        };
        let saving = editor.saving;

        div().w_full().px_4().py_3().child(
            v_flex()
                .w_full()
                .max_w(px(720.))
                .p_3()
                .gap_3()
                .bg(theme.popover)
                .border_1()
                .border_color(theme.primary.opacity(0.7))
                .rounded_lg()
                .child(gpui_component::input::Input::new(&editor.input))
                .child(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("cancel-comment")
                                .ghost()
                                .small()
                                .label("Cancel")
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.close_editor(window, cx)
                                    }),
                                ),
                        )
                        .child(
                            Button::new("submit-comment")
                                .primary()
                                .small()
                                .label(if saving { "Saving…" } else { "Comment" })
                                .disabled(saving)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_comment(window, cx)
                                })),
                        ),
                ),
        )
    }

    fn render_diff_row(&self, row_index: usize, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let Some(diff) = self.selected.and_then(|i| self.diffs.get(&i)) else {
            return div();
        };
        let mono = theme.mono_font_family.clone();

        match &diff.unified[row_index] {
            Row::HunkHeader {
                label,
                hunk,
                expandable,
            } => {
                let (label, hunk, expandable) = (label.clone(), *hunk, *expandable);
                div().child(self.render_hunk_header(label, hunk, expandable, cx))
            }
            Row::Binary => div()
                .w_full()
                .p_4()
                .text_color(theme.muted_foreground)
                .child("binary file — no textual diff"),
            Row::NoChanges => div()
                .w_full()
                .p_4()
                .text_color(theme.muted_foreground)
                .child("no line changes to display (empty file, or a mode/rename-only change)"),
            Row::Line {
                kind,
                old_line,
                new_line,
                text,
                runs,
            } => {
                let (marker, bg) = match kind {
                    LineKind::Added => ("+", Some(theme.success.opacity(0.14))),
                    LineKind::Removed => ("-", Some(theme.danger.opacity(0.14))),
                    LineKind::Context => (" ", None),
                };
                let num = |n: &Option<u32>| -> SharedString {
                    n.map(|v| v.to_string()).unwrap_or_default().into()
                };
                let content = if runs.is_empty() {
                    div().whitespace_nowrap().child(text.clone())
                } else {
                    div()
                        .whitespace_nowrap()
                        .child(StyledText::new(text.clone()).with_highlights(runs.iter().cloned()))
                };

                let anchor = Self::row_anchor(*old_line, *new_line);
                let selected_bg = anchor
                    .filter(|&(side, line)| {
                        self.selection
                            .as_ref()
                            .is_some_and(|sel| sel.contains(side, line))
                    })
                    .map(|_| theme.primary.opacity(0.18));

                // The number gutter is the comment handle: press to start a
                // line selection, drag/shift-click to widen it.
                let gutter = h_flex()
                    .id(("gutter", row_index))
                    .flex_none()
                    .cursor_pointer()
                    .hover(|el| el.bg(theme.accent.opacity(0.5)))
                    .when_some(anchor, |el, (side, line)| {
                        el.on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                this.gutter_down(side, line, ev.modifiers.shift, cx);
                                cx.stop_propagation();
                            }),
                        )
                    })
                    .child(
                        div()
                            .w(px(gutter_width(self.font_size)))
                            .flex_none()
                            .pr_1()
                            .text_right()
                            .text_color(theme.muted_foreground.opacity(0.8))
                            .child(num(old_line)),
                    )
                    .child(
                        div()
                            .w(px(gutter_width(self.font_size)))
                            .flex_none()
                            .pr_2()
                            .text_right()
                            .text_color(theme.muted_foreground.opacity(0.8))
                            .child(num(new_line)),
                    );

                h_flex()
                    .w_full()
                    .h(px(row_height(self.font_size)))
                    .font_family(mono)
                    .text_size(px(self.font_size))
                    .when_some(bg, |el, bg| el.bg(bg))
                    .when_some(selected_bg, |el, bg| el.bg(bg))
                    .when_some(anchor, |el, (side, line)| {
                        el.on_mouse_move(cx.listener(move |this, _, _, cx| {
                            this.gutter_drag_over(side, line, cx);
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| this.gutter_up(window, cx)),
                        )
                    })
                    .child(gutter)
                    .child(div().w_4().flex_none().child(marker))
                    .child(content)
            }
        }
    }

    fn render_split_row(&self, row_index: usize, cx: &mut Context<Self>) -> Div {
        // Copy the few colors out as owned values so no `&Theme` borrow of
        // `cx` is held across the `&mut cx` calls to render_split_cell.
        let mono = cx.theme().mono_font_family.clone();
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;

        let Some(diff) = self.selected.and_then(|i| self.diffs.get(&i)) else {
            return div();
        };

        match &diff.split[row_index] {
            SplitRow::HunkHeader {
                label,
                hunk,
                expandable,
            } => {
                let (label, hunk, expandable) = (label.clone(), *hunk, *expandable);
                div().child(self.render_hunk_header(label, hunk, expandable, cx))
            }
            SplitRow::Binary => div()
                .w_full()
                .p_4()
                .text_color(muted)
                .child("binary file — no textual diff"),
            SplitRow::NoChanges => div()
                .w_full()
                .p_4()
                .text_color(muted)
                .child("no line changes to display (empty file, or a mode/rename-only change)"),
            SplitRow::Pair { left, right } => {
                let left = self.render_split_cell(left.clone(), DiffSide::Old, row_index, cx);
                let right = self.render_split_cell(right.clone(), DiffSide::New, row_index, cx);
                h_flex()
                    .w_full()
                    .h(px(row_height(self.font_size)))
                    .items_stretch()
                    .font_family(mono)
                    .text_size(px(self.font_size))
                    .child(left.flex_1().min_w(px(0.)))
                    .child(div().w(px(1.)).flex_none().bg(border))
                    .child(right.flex_1().min_w(px(0.)))
            }
        }
    }

    /// One side of a split row. `None` renders a faint filler (no such line
    /// on this side — the opposite side was an insertion or deletion).
    /// `side` is the column this cell renders in (left = old, right = new),
    /// which is what a gutter selection anchors to.
    fn render_split_cell(
        &self,
        cell: Option<SplitCell>,
        side: DiffSide,
        row_index: usize,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let Some(cell) = cell else {
            return div().h_full().bg(theme.muted.opacity(0.25));
        };
        let (marker, bg) = match cell.kind {
            LineKind::Added => ("+", Some(theme.success.opacity(0.14))),
            LineKind::Removed => ("-", Some(theme.danger.opacity(0.14))),
            LineKind::Context => (" ", None),
        };
        let selected_bg = cell
            .line
            .filter(|&line| {
                self.selection
                    .as_ref()
                    .is_some_and(|sel| sel.contains(side, line))
            })
            .map(|_| theme.primary.opacity(0.18));
        let number: SharedString = cell.line.map(|v| v.to_string()).unwrap_or_default().into();
        // Clip long lines at the cell edge — without this they render on
        // under the other column's text (backlog: proper h-scroll later).
        let content = if cell.runs.is_empty() {
            div()
                .whitespace_nowrap()
                .overflow_hidden()
                .child(cell.text.clone())
        } else {
            div().whitespace_nowrap().overflow_hidden().child(
                StyledText::new(cell.text.clone()).with_highlights(cell.runs.iter().cloned()),
            )
        };

        let gutter = div()
            .id((
                "split-gutter",
                row_index * 2 + (side == DiffSide::New) as usize,
            ))
            .w(px(gutter_width(self.font_size)))
            .flex_none()
            .pr_2()
            .text_right()
            .text_color(theme.muted_foreground.opacity(0.8))
            .cursor_pointer()
            .hover(|el| el.bg(theme.accent.opacity(0.5)))
            .when_some(cell.line, |el, line| {
                el.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        this.gutter_down(side, line, ev.modifiers.shift, cx);
                        cx.stop_propagation();
                    }),
                )
            })
            .child(number);

        h_flex()
            .w_full()
            .h_full()
            .overflow_hidden()
            .font_family(mono)
            .when_some(bg, |el, bg| el.bg(bg))
            .when_some(selected_bg, |el, bg| el.bg(bg))
            .when_some(cell.line, |el, line| {
                el.on_mouse_move(cx.listener(move |this, _, _, cx| {
                    this.gutter_drag_over(side, line, cx);
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.gutter_up(window, cx)),
                )
            })
            .child(gutter)
            .child(div().w_4().flex_none().child(marker))
            .child(content)
    }
}

/// Rewrite a `base...head` (merge-base) range to a concrete two-dot range
/// anchored at the actual merge base, resolved with one `git merge-base`
/// call. Other sources pass through unchanged.
fn resolve_source(repo: &GitRepo, source: DiffSource) -> anyhow::Result<DiffSource> {
    match source {
        DiffSource::Range {
            base,
            head,
            merge_base: true,
        } => {
            let merged = repo.merge_base(&base, &head)?;
            Ok(DiffSource::Range {
                base: merged,
                head,
                merge_base: false,
            })
        }
        other => Ok(other),
    }
}

fn compute_diff(
    repo: &GitRepo,
    source: &DiffSource,
    file: &ChangedFile,
    hl: &HighlightInputs,
    expand: &HashSet<usize>,
    context_lines: u32,
) -> anyhow::Result<RenderedDiff> {
    let old_path = file.old_path.as_deref().unwrap_or(&file.path);

    let old_spec = match (source, file.status) {
        (_, ChangeStatus::Added) => None,
        (DiffSource::WorkingTree | DiffSource::Staged, _) => Some(BlobSpec::Rev {
            rev: "HEAD".into(),
            path: old_path.into(),
        }),
        (DiffSource::Range { base, .. }, _) => Some(BlobSpec::Rev {
            rev: base.clone(),
            path: old_path.into(),
        }),
        (DiffSource::Commit(sha), _) => Some(BlobSpec::Rev {
            rev: format!("{sha}^"),
            path: old_path.into(),
        }),
    };
    let new_spec = match (source, file.status) {
        (_, ChangeStatus::Deleted) => None,
        (DiffSource::WorkingTree, _) => Some(BlobSpec::Working {
            path: file.path.clone(),
        }),
        (DiffSource::Staged, _) => Some(BlobSpec::Index {
            path: file.path.clone(),
        }),
        (DiffSource::Range { head, .. }, _) => Some(BlobSpec::Rev {
            rev: head.clone(),
            path: file.path.clone(),
        }),
        (DiffSource::Commit(sha), _) => Some(BlobSpec::Rev {
            rev: sha.clone(),
            path: file.path.clone(),
        }),
    };

    let old_bytes = match &old_spec {
        Some(spec) => repo.blob_bytes(spec)?,
        None => None,
    };
    let new_bytes = match &new_spec {
        Some(spec) => repo.blob_bytes(spec)?,
        None => None,
    };

    let diff = dv_core::diff::diff_blobs(
        old_bytes.as_deref(),
        new_bytes.as_deref(),
        &DiffOptions {
            context_lines,
            ..DiffOptions::default()
        },
    );

    // Syntax-highlight each side's full text once (tree-sitter needs whole-file
    // context), then attach per-line runs while assembling rows.
    let old_text = old_bytes
        .as_deref()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let new_text = new_bytes
        .as_deref()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let old_runs = highlight::highlight_file(&old_text, old_path, &hl.theme);
    let new_runs = highlight::highlight_file(&new_text, &file.path, &hl.theme);

    Ok(build_rows(
        &diff, &old_runs, &new_runs, hl, &new_text, expand,
    ))
}

/// A diff line with its display text and merged style runs computed once,
/// then fed into both the unified and split row builders.
struct PreparedLine {
    kind: LineKind,
    old_line: Option<u32>,
    new_line: Option<u32>,
    text: SharedString,
    runs: Vec<(Range<usize>, HighlightStyle)>,
}

impl PreparedLine {
    fn into_cell(self) -> SplitCell {
        let line = match self.kind {
            LineKind::Removed => self.old_line,
            LineKind::Added => self.new_line,
            // Only reached via the context path, which passes the correct
            // side's number explicitly; keep new as a sensible default.
            LineKind::Context => self.new_line,
        };
        SplitCell {
            kind: self.kind,
            line,
            text: self.text,
            runs: self.runs,
        }
    }

    fn context_cell(&self, line: Option<u32>) -> SplitCell {
        SplitCell {
            kind: LineKind::Context,
            line,
            text: self.text.clone(),
            runs: self.runs.clone(),
        }
    }
}

/// The context gap hidden between the previous hunk (or file start) and
/// hunk `i`: `(first_old, first_new, len)` in 1-based line numbers.
///
/// Anchors follow the unified convention: a zero-count side's start is the
/// line *before* the (empty) range, so the first actual line is start+1.
fn gap_above(hunks: &[dv_core::Hunk], i: usize) -> (u32, u32, u32) {
    let first = |start: u32, count: u32| if count == 0 { start + 1 } else { start };
    let (prev_old_end, prev_new_end) = if i == 0 {
        (1, 1)
    } else {
        let prev = &hunks[i - 1];
        (
            first(prev.old_start, prev.old_count) + prev.old_count,
            first(prev.new_start, prev.new_count) + prev.new_count,
        )
    };
    let hunk = &hunks[i];
    let old_first = first(hunk.old_start, hunk.old_count);
    let new_first = first(hunk.new_start, hunk.new_count);
    // The gap is pure context, so it must be the same length on both
    // sides; a mismatch would mean the anchor math is off — expose it as
    // "no gap" rather than rendering wrong line numbers.
    let old_len = old_first.saturating_sub(prev_old_end);
    let new_len = new_first.saturating_sub(prev_new_end);
    let len = if old_len == new_len { new_len } else { 0 };
    (prev_old_end, prev_new_end, len)
}

fn build_rows(
    diff: &FileDiff,
    old_runs: &LineRuns,
    new_runs: &LineRuns,
    hl: &HighlightInputs,
    new_text: &str,
    expand: &HashSet<usize>,
) -> RenderedDiff {
    let mut unified = Vec::new();
    let mut split = Vec::new();
    let mut hunk_rows_unified = Vec::new();
    let mut hunk_rows_split = Vec::new();
    if diff.is_binary {
        unified.push(Row::Binary);
        split.push(SplitRow::Binary);
        return RenderedDiff {
            unified,
            split,
            hunk_rows_unified,
            hunk_rows_split,
            error: false,
        };
    }
    if diff.hunks.is_empty() {
        unified.push(Row::NoChanges);
        split.push(SplitRow::NoChanges);
        return RenderedDiff {
            unified,
            split,
            hunk_rows_unified,
            hunk_rows_split,
            error: false,
        };
    }

    // Gap expansion pulls its lines from the new side (gaps are identical
    // on both sides by definition).
    let new_lines: Vec<&str> = new_text.split('\n').collect();

    let empty: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for (i, hunk) in diff.hunks.iter().enumerate() {
        let (gap_old_first, gap_new_first, gap_len) = gap_above(&diff.hunks, i);
        let gap_available = gap_len > 0
            // All gap lines must exist in the new blob (they won't for a
            // deleted file, where the new side is empty).
            && (gap_new_first + gap_len - 1) as usize <= new_lines.len();

        if gap_available && expand.contains(&i) {
            // Reveal the gap: context rows instead of this hunk's header.
            let gap: Vec<PreparedLine> = (0..gap_len)
                .map(|k| {
                    let new_line = gap_new_first + k;
                    let old_line = gap_old_first + k;
                    // Strip at most ONE trailing CR — the same convention as
                    // dv-core's strip_ending and highlight's bucket_by_line.
                    // A rogue "\r\r\n" line must keep its inner CR, or the
                    // syntax runs (computed against the bucketed content)
                    // overrun the display text and StyledText asserts.
                    let raw = new_lines[(new_line - 1) as usize];
                    let text = raw.strip_suffix('\r').unwrap_or(raw);
                    let runs = if text.len() > highlight::MAX_HIGHLIGHT_LINE {
                        Vec::new()
                    } else {
                        // Route through merge_line_runs (empty intraline) for
                        // the same end-clamping regular diff rows get.
                        let syntax = new_runs.get(&new_line).map(Vec::as_slice).unwrap_or(&[]);
                        highlight::merge_line_runs(text.len(), syntax, &[], hl.intra_added)
                    };
                    PreparedLine {
                        kind: LineKind::Context,
                        old_line: Some(old_line),
                        new_line: Some(new_line),
                        text: text.to_owned().into(),
                        runs,
                    }
                })
                .collect();
            for p in &gap {
                unified.push(Row::Line {
                    kind: p.kind,
                    old_line: p.old_line,
                    new_line: p.new_line,
                    text: p.text.clone(),
                    runs: p.runs.clone(),
                });
            }
            build_split_rows(gap, &mut split);
            // n/p target the hunk's own lines, not the top of the gap.
            hunk_rows_unified.push(unified.len());
            hunk_rows_split.push(split.len());
        } else {
            hunk_rows_unified.push(unified.len());
            hunk_rows_split.push(split.len());
            let label: SharedString = format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            )
            .into();
            let expandable = gap_available.then_some(gap_len);
            unified.push(Row::HunkHeader {
                label: label.clone(),
                hunk: i,
                expandable,
            });
            split.push(SplitRow::HunkHeader {
                label,
                hunk: i,
                expandable,
            });
        }

        let prepared: Vec<PreparedLine> = hunk
            .lines
            .iter()
            .map(|line| {
                // Removed lines highlight from the old side; added and
                // context (identical both sides) from the new side.
                let (syntax, intra_bg) = match line.kind {
                    LineKind::Removed => (
                        line.old_line
                            .and_then(|n| old_runs.get(&n))
                            .unwrap_or(&empty),
                        hl.intra_removed,
                    ),
                    LineKind::Added => (
                        line.new_line
                            .and_then(|n| new_runs.get(&n))
                            .unwrap_or(&empty),
                        hl.intra_added,
                    ),
                    LineKind::Context => (
                        line.new_line
                            .and_then(|n| new_runs.get(&n))
                            .unwrap_or(&empty),
                        hl.intra_added,
                    ),
                };
                let runs = if line.text.len() > highlight::MAX_HIGHLIGHT_LINE {
                    Vec::new()
                } else {
                    highlight::merge_line_runs(line.text.len(), syntax, &line.intraline, intra_bg)
                };
                PreparedLine {
                    kind: line.kind,
                    old_line: line.old_line,
                    new_line: line.new_line,
                    text: line.text.clone().into(),
                    runs,
                }
            })
            .collect();

        for p in &prepared {
            unified.push(Row::Line {
                kind: p.kind,
                old_line: p.old_line,
                new_line: p.new_line,
                text: p.text.clone(),
                runs: p.runs.clone(),
            });
        }
        build_split_rows(prepared, &mut split);
    }
    RenderedDiff {
        unified,
        split,
        hunk_rows_unified,
        hunk_rows_split,
        error: false,
    }
}

/// Turn a hunk's interleaved [context, removed…, added…] lines into aligned
/// side-by-side rows: context spans both columns; within a change region the
/// i-th removed line pairs with the i-th added line, and any surplus on one
/// side leaves the other column blank (a pure add/delete).
fn build_split_rows(prepared: Vec<PreparedLine>, out: &mut Vec<SplitRow>) {
    let mut removed: Vec<PreparedLine> = Vec::new();
    let mut added: Vec<PreparedLine> = Vec::new();

    fn flush(
        removed: &mut Vec<PreparedLine>,
        added: &mut Vec<PreparedLine>,
        out: &mut Vec<SplitRow>,
    ) {
        let pairs = removed.len().max(added.len());
        let mut r = removed.drain(..);
        let mut a = added.drain(..);
        for _ in 0..pairs {
            out.push(SplitRow::Pair {
                left: r.next().map(PreparedLine::into_cell),
                right: a.next().map(PreparedLine::into_cell),
            });
        }
    }

    for p in prepared {
        match p.kind {
            LineKind::Removed => removed.push(p),
            LineKind::Added => added.push(p),
            LineKind::Context => {
                flush(&mut removed, &mut added, out);
                out.push(SplitRow::Pair {
                    left: Some(p.context_cell(p.old_line)),
                    right: Some(p.context_cell(p.new_line)),
                });
            }
        }
    }
    flush(&mut removed, &mut added, out);
}

/// Emitted whenever the workspace's review state changes (loads, saves,
/// watcher reloads) — the shell listens to keep sidebar badges live.
pub struct ReviewChanged;

impl EventEmitter<ReviewChanged> for Workspace {}

/// Emitted once, carrying the final clamped width, when the review-summary
/// panel's resize handle (`render_summary_resize_handle`) releases a drag
/// or resets via double-click — the shell listens (`AppShell::open_review`)
/// to persist it into `Settings`, which the workspace itself has no access
/// to (Phase 4 deliverable 5). Deliberately *not* emitted per drag-move
/// frame — only `self.summary_width` (and `cx.notify()`) update live during
/// the drag itself, matching "write settings.json on release, not
/// per-pixel".
pub struct SummaryWidthChanged(pub f32);

impl EventEmitter<SummaryWidthChanged> for Workspace {}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_component::Selectable as _;
        use gpui_component::Sizable as _;
        use gpui_component::button::{Button, ButtonVariants as _};
        let summary = self.render_summary(cx);
        let theme = cx.theme();
        let mode = self.view_mode;

        let body: Div = match &self.status {
            Status::Loading => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(match self.pr_loading {
                    Some(number) => format!("opening PR #{number}…"),
                    None => "opening repository…".to_string(),
                }),
            Status::Failed(err) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.danger)
                .child(err.clone()),
            Status::Ready if self.files.is_empty() => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child("no changes"),
            Status::Ready => div().size_full().flex().flex_row().child(
                h_flex()
                    .size_full()
                    .items_start()
                    .child(
                        v_flex()
                            .h_full()
                            .w(px(320.))
                            .flex_none()
                            .border_r_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{} changed files", self.files.len())),
                            )
                            .child(
                                uniform_list(
                                    "file-list",
                                    self.files.len(),
                                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                        range
                                            .map(|i| this.render_file_row(i, cx))
                                            .collect::<Vec<_>>()
                                    }),
                                )
                                .track_scroll(&self.file_scroll)
                                .size_full(),
                            ),
                    )
                    .child(div().h_full().flex_1().min_w(px(0.)).child({
                        let this = cx.weak_entity();
                        list(self.diff_list.clone(), move |ix, _window, cx| {
                            this.update(cx, |this, cx| this.render_display_row(ix, cx))
                                .unwrap_or_else(|_| div().into_any_element())
                        })
                        .size_full()
                    }))
                    .children(summary),
            ),
        };

        // While the palette or the comment editor is open the workspace node
        // carries an extra identifier, flipping which key bindings apply
        // (see `init`).
        let mut key_context = KEY_CONTEXT.to_string();
        if self.palette.is_some() {
            key_context.push(' ');
            key_context.push_str(PALETTE_CONTEXT);
        }
        if self.editor.is_some() || self.thread_input.is_some() {
            key_context.push(' ');
            key_context.push_str(EDITOR_CONTEXT);
        }
        if self.pr_picker.is_some() {
            key_context.push(' ');
            key_context.push_str(PR_PICKER_CONTEXT);
        }

        v_flex()
            .size_full()
            .relative()
            .track_focus(&self.focus_handle)
            .key_context(key_context.as_str())
            .on_action(cx.listener(Self::on_next_file))
            .on_action(cx.listener(Self::on_prev_file))
            .on_action(cx.listener(Self::on_next_hunk))
            .on_action(cx.listener(Self::on_prev_hunk))
            .on_action(cx.listener(Self::on_toggle_split))
            .on_action(cx.listener(Self::on_toggle_summary))
            .on_action(cx.listener(Self::on_jump_to_file))
            .on_action(cx.listener(Self::on_clear_selection))
            .on_action(cx.listener(Self::on_cancel_comment))
            .on_action(cx.listener(Self::on_palette_next))
            .on_action(cx.listener(Self::on_palette_prev))
            .on_action(cx.listener(Self::on_palette_close))
            .on_action(cx.listener(Self::on_open_pr_picker))
            .on_action(cx.listener(Self::on_pr_picker_next))
            .on_action(cx.listener(Self::on_pr_picker_prev))
            .on_action(cx.listener(Self::on_pr_picker_close))
            .on_action(cx.listener(Self::on_pr_picker_choose))
            .on_action(cx.listener(Self::on_refresh_pr))
            .child(
                // Per-review header strip (the window title bar is the
                // shell's; this shows which review is active).
                h_flex()
                    .flex_none()
                    .w_full()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(self.head.clone()),
                    )
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(self.source_desc.clone()),
                    )
                    .when_some(self.pr_error.clone(), |el, err| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(theme.danger)
                                .truncate()
                                .child(format!("PR: {err}")),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("pr-picker-hint")
                            .ghost()
                            .small()
                            .label("PRs · ctrl-g")
                            .on_click(cx.listener(|this, _, window, cx| {
                                // Same guard the `ctrl-g` keybinding gets
                                // for free from its `!EditorOpen` key
                                // context: a click mustn't steal focus
                                // from a focused comment/thread input —
                                // the picker would open but sit
                                // keyboard-dead behind it.
                                if this.editor.is_some() || this.thread_input.is_some() {
                                    return;
                                }
                                this.on_open_pr_picker(&OpenPrPicker, window, cx)
                            })),
                    )
                    .child(
                        Button::new("view-toggle")
                            .ghost()
                            .small()
                            .label(match mode {
                                ViewMode::Unified => "unified · s",
                                ViewMode::Split => "split · s",
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_toggle_split(&ToggleSplit, window, cx)
                            })),
                    )
                    .child(
                        Button::new("summary-toggle")
                            .ghost()
                            .small()
                            .selected(self.summary_open)
                            .label("review · r")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_toggle_summary(&ToggleSummary, window, cx)
                            })),
                    ),
            )
            .children(self.render_pr_header(cx))
            .child(body)
            .children(self.render_palette(cx))
            .children(self.render_pr_picker(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChecksSummary, PrHeader, PrMeta, PrState, PreparedLine, SplitRow, SubmissionOutcome,
        SubmitFlow, SubmitPrep, Violation, ViolationKind, build_split_rows,
        cancel_submit_flow_outcome, gap_above, pick_review, reconcile_file_selection,
        review_adopts_pr, submit_flow_from_submission, submit_flow_from_validation,
        trim_trailing_newlines, verdict_automation_word, verdict_label, violation_kind_word,
    };
    use dv_core::{
        ChangeStatus, ChangedFile, DiffSource, LineKind, RemoteRef, Review, ReviewState,
    };

    fn hunk(old_start: u32, old_count: u32, new_start: u32, new_count: u32) -> dv_core::Hunk {
        dv_core::Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines: Vec::new(),
        }
    }

    #[test]
    fn gap_above_first_hunk_counts_from_file_start() {
        // Hunk starts at line 14 on both sides → 13 hidden lines (1..=13).
        let hunks = [hunk(14, 17, 14, 44)];
        assert_eq!(gap_above(&hunks, 0), (1, 1, 13));
    }

    #[test]
    fn gap_above_first_hunk_at_top_is_empty() {
        let hunks = [hunk(1, 5, 1, 7)];
        assert_eq!(gap_above(&hunks, 0), (1, 1, 0));
    }

    #[test]
    fn gap_between_hunks_uses_previous_hunk_end() {
        // Hunk 0 covers old 10..20 / new 10..22; hunk 1 starts at old 32 /
        // new 34 → gap is old 20..32 = new 22..34 = 12 lines.
        let hunks = [hunk(10, 10, 10, 12), hunk(32, 4, 34, 4)];
        assert_eq!(gap_above(&hunks, 1), (20, 22, 12));
    }

    #[test]
    fn gap_with_zero_count_side_respects_unified_anchor_convention() {
        // A pure-insertion hunk: old side is empty, old_start anchors to the
        // line *before* (unified convention), so the next real old line is
        // old_start + 1.
        let hunks = [hunk(5, 0, 6, 3), hunk(10, 2, 14, 2)];
        // prev_old_end = 5+1+0 = 6; prev_new_end = 6+3 = 9.
        // gap: old 6..10 = 4 lines, new 9..14 = 5 lines → mismatch is
        // impossible for real diffs with these anchors; equal-length check
        // guards regardless.
        let (_, _, len) = gap_above(&hunks, 1);
        assert!(
            len == 0 || len == 4,
            "mismatched gap must not fabricate lines"
        );
    }

    #[test]
    fn mismatched_gap_lengths_disable_expansion() {
        // Deliberately inconsistent anchors → gap reported as absent.
        let hunks = [hunk(10, 5, 10, 8), hunk(20, 2, 20, 2)];
        let (_, _, len) = gap_above(&hunks, 1);
        assert_eq!(len, 0);
    }

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>) -> PreparedLine {
        PreparedLine {
            kind,
            old_line: old,
            new_line: new,
            text: "x".into(),
            runs: Vec::new(),
        }
    }

    /// (has_left, left_line, has_right, right_line) for each Pair row.
    fn shape(rows: &[SplitRow]) -> Vec<(bool, Option<u32>, bool, Option<u32>)> {
        rows.iter()
            .map(|r| match r {
                SplitRow::Pair { left, right } => (
                    left.is_some(),
                    left.as_ref().and_then(|c| c.line),
                    right.is_some(),
                    right.as_ref().and_then(|c| c.line),
                ),
                _ => (false, None, false, None),
            })
            .collect()
    }

    #[test]
    fn split_pairs_a_modification() {
        let mut out = Vec::new();
        build_split_rows(
            vec![
                line(LineKind::Removed, Some(1), None),
                line(LineKind::Added, None, Some(1)),
            ],
            &mut out,
        );
        assert_eq!(shape(&out), vec![(true, Some(1), true, Some(1))]);
    }

    #[test]
    fn split_leaves_blank_for_unequal_blocks() {
        // 2 removed vs 1 added: row 2 has no right side.
        let mut out = Vec::new();
        build_split_rows(
            vec![
                line(LineKind::Removed, Some(1), None),
                line(LineKind::Removed, Some(2), None),
                line(LineKind::Added, None, Some(1)),
            ],
            &mut out,
        );
        assert_eq!(
            shape(&out),
            vec![(true, Some(1), true, Some(1)), (true, Some(2), false, None)]
        );
    }

    #[test]
    fn split_pure_addition_has_blank_left() {
        let mut out = Vec::new();
        build_split_rows(
            vec![
                line(LineKind::Added, None, Some(1)),
                line(LineKind::Added, None, Some(2)),
            ],
            &mut out,
        );
        assert_eq!(
            shape(&out),
            vec![(false, None, true, Some(1)), (false, None, true, Some(2))]
        );
    }

    #[test]
    fn split_context_spans_both_and_flushes_region() {
        // context, then a modify region, then context.
        let mut out = Vec::new();
        build_split_rows(
            vec![
                line(LineKind::Context, Some(1), Some(1)),
                line(LineKind::Removed, Some(2), None),
                line(LineKind::Added, None, Some(2)),
                line(LineKind::Context, Some(3), Some(3)),
            ],
            &mut out,
        );
        assert_eq!(
            shape(&out),
            vec![
                (true, Some(1), true, Some(1)),
                (true, Some(2), true, Some(2)),
                (true, Some(3), true, Some(3)),
            ]
        );
    }

    // ---- pick_review (review finding P1-b) ---------------------------

    fn fake_review(id: &str, state: ReviewState, remote: Option<RemoteRef>) -> Review {
        Review {
            v: 1,
            id: id.to_string(),
            source: DiffSource::WorkingTree,
            state,
            created_ms: 0,
            updated_ms: 0,
            comments: Vec::new(),
            remote,
        }
    }

    fn fake_remote(pr: u64, slug: &str) -> RemoteRef {
        RemoteRef {
            provider: "github".to_string(),
            slug: slug.to_string(),
            pr,
            url: String::new(),
            submitted_review_id: None,
            submitted_url: None,
        }
    }

    #[test]
    fn pick_review_with_no_pr_open_prefers_the_latest_draft() {
        let reviews = vec![
            fake_review(
                "r-submitted",
                ReviewState::Submitted {
                    verdict: dv_core::Verdict::Approve,
                    at_ms: 0,
                },
                None,
            ),
            fake_review("r-draft", ReviewState::Draft, None),
        ];
        let picked = pick_review(reviews, None, None);
        assert_eq!(picked.map(|r| r.id), Some("r-draft".to_string()));
    }

    /// The exact P1-b repro: the workspace is on PR A's linked review, and
    /// the store's newest entry is an unrelated PR B draft the CLI just
    /// created (e.g. `dv comment add` with no `--review`, targeting
    /// whatever is newest). With PR A open, the newest-draft rule must not
    /// win.
    #[test]
    fn pick_review_with_pr_open_ignores_a_newer_unrelated_draft() {
        let pr_a = fake_remote(5, "github.com/acme/widgets");
        let reviews = vec![
            // Newest-created (store.list() is newest-first) — but for a
            // different PR entirely.
            fake_review(
                "r-b-newest",
                ReviewState::Draft,
                Some(fake_remote(99, "github.com/acme/widgets")),
            ),
            fake_review("r-a-current", ReviewState::Draft, Some(pr_a.clone())),
        ];
        let picked = pick_review(reviews, Some("r-a-current"), Some(&pr_a));
        assert_eq!(picked.map(|r| r.id), Some("r-a-current".to_string()));
    }

    #[test]
    fn pick_review_with_pr_open_matches_slug_case_insensitively() {
        let current_pr = fake_remote(5, "GitHub.com/Acme/Widgets");
        let reviews = vec![fake_review(
            "r-a",
            ReviewState::Draft,
            Some(fake_remote(5, "github.com/acme/widgets")),
        )];
        let picked = pick_review(reviews, None, Some(&current_pr));
        assert_eq!(picked.map(|r| r.id), Some("r-a".to_string()));
    }

    /// No draft (or any review) is linked to the open PR yet — must keep
    /// showing whatever's already on screen rather than fall back to an
    /// unrelated draft.
    #[test]
    fn pick_review_with_pr_open_and_no_match_keeps_current_not_unrelated_draft() {
        let current_pr = fake_remote(5, "github.com/acme/widgets");
        let reviews = vec![
            fake_review("r-current", ReviewState::Draft, None),
            fake_review(
                "r-unrelated",
                ReviewState::Draft,
                Some(fake_remote(99, "github.com/other/repo")),
            ),
        ];
        let picked = pick_review(reviews, Some("r-current"), Some(&current_pr));
        assert_eq!(picked.map(|r| r.id), Some("r-current".to_string()));
    }

    /// Same, but there isn't even a current review to fall back to (e.g.
    /// the very first watcher tick after an external store wipe) — must
    /// come back empty rather than adopt the unrelated draft.
    #[test]
    fn pick_review_with_pr_open_and_no_match_and_no_current_is_none() {
        let current_pr = fake_remote(5, "github.com/acme/widgets");
        let reviews = vec![fake_review(
            "r-unrelated",
            ReviewState::Draft,
            Some(fake_remote(99, "github.com/other/repo")),
        )];
        let picked = pick_review(reviews, None, Some(&current_pr));
        assert!(picked.is_none());
    }

    // ---- review_adopts_pr / load_pr find-or-create (review finding P1) ----
    //
    // `load_pr` itself does real git/store I/O and can't run headless, but
    // its find-or-create is just `reviews.iter().find(|r|
    // review_adopts_pr(r, slug, pr))` — exercised directly here against
    // the exact matrix the fix promises: a submitted-only match must miss
    // (so the caller creates a fresh draft), a draft match must hit, and
    // with both present the draft must win over the submitted one.

    #[test]
    fn review_adopts_pr_rejects_a_submitted_matching_review() {
        let remote = fake_remote(7, "github.com/fake/fake");
        let submitted = fake_review(
            "r-submitted",
            ReviewState::Submitted {
                verdict: dv_core::Verdict::Approve,
                at_ms: 0,
            },
            Some(remote.clone()),
        );
        assert!(!review_adopts_pr(&submitted, &remote.slug, remote.pr));
    }

    #[test]
    fn review_adopts_pr_accepts_a_draft_matching_review() {
        let remote = fake_remote(7, "github.com/fake/fake");
        let draft = fake_review("r-draft", ReviewState::Draft, Some(remote.clone()));
        assert!(review_adopts_pr(&draft, &remote.slug, remote.pr));
    }

    #[test]
    fn review_adopts_pr_rejects_a_draft_for_a_different_pr_or_slug() {
        let remote = fake_remote(7, "github.com/fake/fake");
        let other_pr = fake_review(
            "r-other-pr",
            ReviewState::Draft,
            Some(fake_remote(8, "github.com/fake/fake")),
        );
        let other_slug = fake_review(
            "r-other-slug",
            ReviewState::Draft,
            Some(fake_remote(7, "github.com/other/repo")),
        );
        assert!(!review_adopts_pr(&other_pr, &remote.slug, remote.pr));
        assert!(!review_adopts_pr(&other_slug, &remote.slug, remote.pr));
    }

    #[test]
    fn find_or_create_with_only_a_submitted_match_finds_none_so_a_fresh_draft_is_created() {
        let remote = fake_remote(7, "github.com/fake/fake");
        let reviews = [fake_review(
            "r-submitted",
            ReviewState::Submitted {
                verdict: dv_core::Verdict::Approve,
                at_ms: 0,
            },
            Some(remote.clone()),
        )];
        let found = reviews
            .iter()
            .find(|r| review_adopts_pr(r, &remote.slug, remote.pr));
        assert!(
            found.is_none(),
            "a submitted-only match must be treated as no-match, so \
             load_pr's caller creates a fresh draft instead of adopting it"
        );
    }

    #[test]
    fn find_or_create_with_a_draft_match_adopts_it() {
        let remote = fake_remote(7, "github.com/fake/fake");
        let reviews = [fake_review(
            "r-draft",
            ReviewState::Draft,
            Some(remote.clone()),
        )];
        let found = reviews
            .iter()
            .find(|r| review_adopts_pr(r, &remote.slug, remote.pr));
        assert_eq!(found.map(|r| r.id.as_str()), Some("r-draft"));
    }

    #[test]
    fn find_or_create_with_both_submitted_and_draft_adopts_the_draft() {
        let remote = fake_remote(7, "github.com/fake/fake");
        let submitted = fake_review(
            "r-submitted",
            ReviewState::Submitted {
                verdict: dv_core::Verdict::Approve,
                at_ms: 0,
            },
            Some(remote.clone()),
        );
        let draft = fake_review("r-draft", ReviewState::Draft, Some(remote.clone()));
        // store.list() returns newest-created first; put the submitted one
        // (which would be newer, from a real re-review) ahead of the draft
        // to make sure the state filter — not just the newest-first order
        // — is what picks the draft.
        let reviews = [submitted, draft];
        let found = reviews
            .iter()
            .find(|r| review_adopts_pr(r, &remote.slug, remote.pr));
        assert_eq!(found.map(|r| r.id.as_str()), Some("r-draft"));
    }

    // ---- trim_trailing_newlines (review finding P3-a) ----------------------

    #[test]
    fn trim_trailing_newlines_strips_one_or_many_trailing_newlines() {
        assert_eq!(trim_trailing_newlines("hello".to_string()), "hello");
        assert_eq!(trim_trailing_newlines("hello\n".to_string()), "hello");
        assert_eq!(trim_trailing_newlines("hello\n\n\n".to_string()), "hello");
        assert_eq!(trim_trailing_newlines(String::new()), "");
    }

    #[test]
    fn trim_trailing_newlines_preserves_interior_newlines_and_trailing_spaces() {
        assert_eq!(
            trim_trailing_newlines("line one\nline two  \n".to_string()),
            "line one\nline two  ",
            "only a trailing newline should go — trailing spaces before it \
             (e.g. inside a fenced code block) must survive"
        );
        assert_eq!(
            trim_trailing_newlines("trailing spaces   ".to_string()),
            "trailing spaces   "
        );
    }

    // --- SubmitFlow state machine (docs/phase-3-github.md deliverable 2/4) --
    //
    // The three completion points (`start_submit_validation`,
    // `on_submit_click`, `cancel_submit_flow`) are thin `cx.spawn`/`Context`
    // wrappers around the pure functions below — covered directly here
    // since a full `Workspace` needs a running gpui `Context` this crate's
    // test setup doesn't build. The render output of each `SubmitFlow`
    // panel (`render_submit_flow`) is NOT covered by these tests — that's
    // pixels, verified only by the live `--automation` acceptance run
    // (docs/phase-3-github.md's Verification steps), a residual gap noted
    // in the phase report.

    fn dummy_pr_meta(number: u64) -> PrMeta {
        PrMeta {
            number,
            title: "t".to_string(),
            body: String::new(),
            url: format!("https://github.com/o/r/pull/{number}"),
            state: PrState::Open,
            is_draft: false,
            base_ref: "main".to_string(),
            head_ref: "feature".to_string(),
            base_oid: "a".repeat(40),
            head_oid: "b".repeat(40),
            author: "someone".to_string(),
            review_decision: None,
            checks: ChecksSummary::None,
        }
    }

    // --- PrHeader::from(PrMeta) (review finding P3-b) ----------------------

    #[test]
    fn pr_header_from_meta_carries_every_field() {
        let mut meta = dummy_pr_meta(42);
        meta.is_draft = true;
        meta.review_decision = Some(dv_core::ReviewDecision::ChangesRequested);
        let header: PrHeader = meta.clone().into();

        assert_eq!(header.number, meta.number);
        assert_eq!(header.title.to_string(), meta.title);
        assert_eq!(header.author.to_string(), meta.author);
        assert_eq!(header.state, meta.state);
        assert_eq!(header.is_draft, meta.is_draft);
        assert_eq!(header.base_ref.to_string(), meta.base_ref);
        assert_eq!(header.head_ref.to_string(), meta.head_ref);
        assert_eq!(header.checks, meta.checks);
        assert_eq!(header.review_decision, meta.review_decision);
        assert_eq!(header.url.to_string(), meta.url);
        assert_eq!(header.body.to_string(), meta.body);
    }

    fn dummy_submission(verdict: dv_core::Verdict) -> dv_core::ReviewSubmission {
        dv_core::ReviewSubmission {
            commit_id: "b".repeat(40),
            body: String::new(),
            event: match verdict {
                dv_core::Verdict::Comment => dv_core::ReviewEvent::Comment,
                dv_core::Verdict::Approve => dv_core::ReviewEvent::Approve,
                dv_core::Verdict::RequestChanges => dv_core::ReviewEvent::RequestChanges,
            },
            comments: Vec::new(),
        }
    }

    fn dummy_violation(comment_id: &str) -> Violation {
        Violation {
            comment_id: comment_id.to_string(),
            path: "a.rs".to_string(),
            lines: "5".to_string(),
            kind: ViolationKind::StaleAnchor,
            message: format!("comment {comment_id} has a stale anchor"),
        }
    }

    // --- submit_flow_from_validation -------------------------------------

    #[test]
    fn submit_flow_from_validation_ready_becomes_confirming_with_prep() {
        let meta = dummy_pr_meta(7);
        let submission = dummy_submission(dv_core::Verdict::Approve);
        let outcome: anyhow::Result<(PrMeta, SubmissionOutcome)> =
            Ok((meta, SubmissionOutcome::Ready(submission)));

        let flow =
            submit_flow_from_validation(dv_core::Verdict::Approve, "r-1".to_string(), outcome);
        match flow {
            SubmitFlow::Confirming { verdict, prep } => {
                assert_eq!(verdict, dv_core::Verdict::Approve);
                assert_eq!(prep.review_id, "r-1");
                assert_eq!(prep.pr_number, 7);
                assert_eq!(prep.pr_url, "https://github.com/o/r/pull/7");
            }
            _ => panic!("expected Confirming, got a different SubmitFlow variant"),
        }
    }

    #[test]
    fn submit_flow_from_validation_blocked_carries_violations() {
        let meta = dummy_pr_meta(7);
        let violations = vec![dummy_violation("c-1"), dummy_violation("c-2")];
        let outcome: anyhow::Result<(PrMeta, SubmissionOutcome)> =
            Ok((meta, SubmissionOutcome::Blocked(violations)));

        let flow =
            submit_flow_from_validation(dv_core::Verdict::Comment, "r-1".to_string(), outcome);
        match flow {
            SubmitFlow::Blocked {
                verdict,
                violations,
            } => {
                assert_eq!(verdict, dv_core::Verdict::Comment);
                assert_eq!(violations.len(), 2);
                assert_eq!(violations[0].comment_id, "c-1");
            }
            _ => panic!("expected Blocked"),
        }
    }

    #[test]
    fn submit_flow_from_validation_hard_error_becomes_failed_with_message() {
        let outcome: anyhow::Result<(PrMeta, SubmissionOutcome)> =
            Err(anyhow::anyhow!("gh: not authenticated"));

        let flow = submit_flow_from_validation(
            dv_core::Verdict::RequestChanges,
            "r-1".to_string(),
            outcome,
        );
        match flow {
            SubmitFlow::Failed { verdict, message } => {
                assert_eq!(verdict, dv_core::Verdict::RequestChanges);
                assert!(message.contains("not authenticated"), "message: {message}");
            }
            _ => panic!("expected Failed"),
        }
    }

    // --- submit_flow_from_submission --------------------------------------

    #[test]
    fn submit_flow_from_submission_success_returns_review_and_done() {
        let review = Review {
            v: dv_core::review::SCHEMA_VERSION,
            id: "r-test".to_string(),
            source: DiffSource::WorkingTree,
            state: ReviewState::Draft,
            created_ms: 0,
            updated_ms: 0,
            comments: Vec::new(),
            remote: None,
        };
        let result: Result<(Review, String), String> = Ok((
            review,
            "https://github.com/o/r/pull/7#pullrequestreview-1".to_string(),
        ));

        let (fresh, flow) = submit_flow_from_submission(dv_core::Verdict::Approve, result);
        assert!(fresh.is_some(), "success must hand back the fresh review");
        match flow {
            SubmitFlow::Done { verdict, url } => {
                assert_eq!(verdict, dv_core::Verdict::Approve);
                assert_eq!(url, "https://github.com/o/r/pull/7#pullrequestreview-1");
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn submit_flow_from_submission_failure_returns_no_review_and_failed() {
        let result: Result<(Review, String), String> =
            Err("GitHub ACCEPTED the review ... Do NOT re-run".to_string());

        let (fresh, flow) = submit_flow_from_submission(dv_core::Verdict::Comment, result);
        assert!(
            fresh.is_none(),
            "failure must not overwrite the workspace's review"
        );
        match flow {
            SubmitFlow::Failed { verdict, message } => {
                assert_eq!(verdict, dv_core::Verdict::Comment);
                assert!(message.contains("Do NOT re-run"));
            }
            _ => panic!("expected Failed"),
        }
    }

    // --- cancel_submit_flow_outcome (also covers submit_epoch — P1-1) ------

    #[test]
    fn cancel_submit_flow_outcome_leaves_submitting_untouched() {
        let flow = Some(SubmitFlow::Submitting {
            verdict: dv_core::Verdict::Approve,
        });
        let (next, changed, epoch) = cancel_submit_flow_outcome(flow, 5);
        assert!(!changed, "Submitting must not be cancelled");
        assert!(matches!(next, Some(SubmitFlow::Submitting { .. })));
        assert_eq!(
            epoch, 5,
            "nothing was cancelled, so the epoch must not move"
        );
    }

    #[test]
    fn cancel_submit_flow_outcome_resets_confirming_and_blocked_and_bumps_epoch() {
        let confirming = Some(SubmitFlow::Confirming {
            verdict: dv_core::Verdict::Comment,
            prep: SubmitPrep {
                review_id: "r-1".to_string(),
                pr_number: 1,
                pr_url: "https://example.invalid/pull/1".to_string(),
                submission: dummy_submission(dv_core::Verdict::Comment),
            },
        });
        let (next, changed, epoch) = cancel_submit_flow_outcome(confirming, 5);
        assert!(changed);
        assert!(next.is_none());
        assert_eq!(
            epoch, 6,
            "cancelling must bump the epoch so an in-flight completion discards itself"
        );

        let blocked = Some(SubmitFlow::Blocked {
            verdict: dv_core::Verdict::Comment,
            violations: vec![dummy_violation("c-1")],
        });
        let (next, changed, epoch) = cancel_submit_flow_outcome(blocked, 6);
        assert!(changed);
        assert!(next.is_none());
        assert_eq!(epoch, 7);
    }

    #[test]
    fn cancel_submit_flow_outcome_resets_done_and_failed_too_and_bumps_epoch() {
        let done = Some(SubmitFlow::Done {
            verdict: dv_core::Verdict::Approve,
            url: "https://example.invalid".to_string(),
        });
        let (next, changed, epoch) = cancel_submit_flow_outcome(done, 1);
        assert!(changed);
        assert!(next.is_none());
        assert_eq!(epoch, 2);

        let failed = Some(SubmitFlow::Failed {
            verdict: dv_core::Verdict::Approve,
            message: "boom".to_string(),
        });
        let (next, changed, epoch) = cancel_submit_flow_outcome(failed, 2);
        assert!(changed);
        assert!(next.is_none());
        assert_eq!(epoch, 3);
    }

    #[test]
    fn cancel_submit_flow_outcome_noop_when_already_idle() {
        let (next, changed, epoch) = cancel_submit_flow_outcome(None, 3);
        assert!(!changed);
        assert!(next.is_none());
        assert_eq!(epoch, 3, "idle -> idle must not bump the epoch either");
    }

    // --- word helpers -------------------------------------------------------

    #[test]
    fn verdict_words_cover_every_variant() {
        for verdict in [
            dv_core::Verdict::Comment,
            dv_core::Verdict::Approve,
            dv_core::Verdict::RequestChanges,
        ] {
            assert!(!verdict_label(verdict).is_empty());
            assert!(!verdict_automation_word(verdict).is_empty());
        }
        assert_eq!(
            verdict_automation_word(dv_core::Verdict::RequestChanges),
            "request_changes"
        );
        assert_eq!(
            verdict_label(dv_core::Verdict::RequestChanges),
            "request changes"
        );
    }

    #[test]
    fn violation_kind_words_cover_every_variant() {
        for kind in [
            ViolationKind::StaleAnchor,
            ViolationKind::Unanchored,
            ViolationKind::RenameUnverifiable,
            ViolationKind::NotInDiff,
            ViolationKind::NothingToSubmit,
        ] {
            assert!(!violation_kind_word(kind).is_empty());
        }
    }

    // --- reconcile_file_selection (review finding P1-1) --------------------

    fn cf(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.to_string(),
            old_path: None,
            status: ChangeStatus::Modified,
        }
    }

    #[test]
    fn reconcile_stable_when_paths_and_order_are_unchanged() {
        let old = [cf("a.txt"), cf("b.txt"), cf("c.txt")];
        let new = [cf("a.txt"), cf("b.txt"), cf("c.txt")];
        assert_eq!(
            reconcile_file_selection(&old, &new, Some(1)),
            (Some(1), false)
        );
        // Even with nothing selected, an unchanged list needs no invalidation.
        assert_eq!(reconcile_file_selection(&old, &new, None), (None, false));
    }

    #[test]
    fn reconcile_empty_new_list_clears_selection_and_invalidates() {
        // The original crash repro: 3 files, the third (index 2) selected,
        // an external commit empties the list entirely.
        let old = [cf("a.txt"), cf("b.txt"), cf("c.txt")];
        let new: [ChangedFile; 0] = [];
        assert_eq!(reconcile_file_selection(&old, &new, Some(2)), (None, true));
    }

    #[test]
    fn reconcile_shrink_drops_selection_when_selected_file_is_removed() {
        let old = [cf("a.txt"), cf("b.txt"), cf("c.txt")];
        let new = [cf("a.txt"), cf("b.txt")];
        assert_eq!(reconcile_file_selection(&old, &new, Some(2)), (None, true));
        // An unrelated file's removal that leaves the selected file in
        // place still needs invalidation (the list itself changed).
        assert_eq!(
            reconcile_file_selection(&old, &new, Some(0)),
            (Some(0), true)
        );
    }

    #[test]
    fn reconcile_shift_relocates_the_same_file_to_its_new_index() {
        // A new file ("aa.txt") sorts in before the previously selected
        // "c.txt", pushing it from index 2 to index 3.
        let old = [cf("a.txt"), cf("b.txt"), cf("c.txt")];
        let new = [cf("a.txt"), cf("aa.txt"), cf("b.txt"), cf("c.txt")];
        assert_eq!(
            reconcile_file_selection(&old, &new, Some(2)),
            (Some(3), true)
        );
    }

    #[test]
    fn reconcile_shift_at_same_length_still_invalidates_reused_indices() {
        // Same length, but a different file occupies index 0 now — even
        // though the selected file ("z.txt") is still at index 1, index 0's
        // cached diff (if any) is now for a DIFFERENT file and must not be
        // reused silently.
        let old = [cf("b.txt"), cf("z.txt")];
        let new = [cf("a.txt"), cf("z.txt")];
        assert_eq!(
            reconcile_file_selection(&old, &new, Some(1)),
            (Some(1), true)
        );
    }

    #[test]
    fn reconcile_nothing_selected_falls_through_to_none() {
        let old = [cf("a.txt")];
        let new = [cf("a.txt"), cf("b.txt")];
        assert_eq!(reconcile_file_selection(&old, &new, None), (None, true));
    }
}
