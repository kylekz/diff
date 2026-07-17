use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use dv_cli::submit::{self, SubmissionOutcome, Violation};
// Consumed by `violation_kind_word` (automation-only, see below) in
// production, and by this module's own `#[cfg(test)]` fixtures
// (`dummy_violation`) regardless of feature — so it's needed whenever
// either is compiled in.
#[cfg(any(feature = "automation", test))]
use dv_cli::submit::ViolationKind;
use dv_core::{
    BlobSpec, ChangeStatus, ChangedFile, ChecksSummary, DiffOptions, DiffSource, FileDiff, GhSide,
    GitRepo, GithubClient, LineKind, PrMeta, PrState, PrSummary, RemoteRef, RemoteThread,
    RepoLocation, RepoSlug, ReviewDecision, repo_label,
};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::highlighter::HighlightTheme;
use gpui_component::{ActiveTheme, Disableable as _, StyledExt as _, h_flex, v_flex};

use crate::highlight::{self, LineRuns};
use crate::settings::{SUMMARY_WIDTH_MAX, SUMMARY_WIDTH_MIN, ViewModeSetting};

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
        RefreshPr,
        // S8f (docs/phase-8-lsp-and-polish.md § LSP): read-only target-viewer
        // navigation. `NavBack`/`NavForward` walk `Workspace::nav_stack`
        // (populated by go-to-definition jumps); `CloseTargetViewer` closes
        // the overlay `render_target_viewer` paints. Not gated behind the
        // `automation` feature — this is real product surface, not a test
        // seam (mirrors `JumpToFile`/`OpenPrPicker` sitting in this same
        // ungated list).
        NavBack,
        NavForward,
        CloseTargetViewer
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
/// Width of the +/- marker column, split out from the number gutter(s) —
/// the reference design's unified-row anatomy is three segments
/// (44+44+28px at its fixed 13px mono/22px row: two number columns then
/// the marker column). This used to be a bare `w_4()` (16px), fixed
/// regardless of the font-size setting, the same pre-restyle problem
/// [`gutter_width`]'s own doc comment describes for the number columns.
/// `2.0` lands close to the reference 28/44 ≈ 0.64 ratio against
/// `gutter_width`'s `3.5` (2.0/3.5 ≈ 0.57) on a clean multiplier.
fn marker_width(font_size: f32) -> f32 {
    font_size * 2.0
}
/// Sidebar file-tree row height (24px, per the reference design's file
/// tree). Unlike [`row_height`] above, this is a plain UI-chrome
/// constant — like `shell::SIDEBAR_ROW_HEIGHT` — rather than one
/// scaled off the `mono_font_size` setting: the file tree renders at the
/// regular UI text size, not the diff pane's mono font, so it isn't the
/// settings knob the row-height-scaling guarantee (CLAUDE.md
/// cross-cutting risks) is protecting.
const FILE_TREE_ROW_HEIGHT: f32 = 24.0;
/// Blank leading spacer that aligns a comment thread/editor card's left
/// edge with where a diff row's CODE TEXT begins, rather than with its
/// line numbers (the reference design's "72px blank gutter spacer" at
/// its fixed 13px/28px numbers — one number-gutter column plus the
/// marker column). A comment always anchors to a single
/// side regardless of view mode, so this uses the single-side width even
/// under the unified view (which shows two number columns) rather than
/// double-counting a comment card's indent against both.
fn thread_gutter_width(font_size: f32) -> f32 {
    gutter_width(font_size) + marker_width(font_size)
}

/// Byte offset → UTF-16 code-unit count, up to (and excluding) `byte_offset`
/// — LSP's `Position.character` is defined in UTF-16 code units regardless
/// of source encoding (the client never advertised `positionEncoding:
/// "utf-8"` in `initialize`, so the server defaults to UTF-16; see
/// `dv_core::lsp::client`'s `initialize` params). Byte offset and UTF-16
/// count coincide for plain ASCII (the overwhelming common case for a
/// clicked identifier), but this converts correctly regardless.
fn utf16_column(text: &str, byte_offset: usize) -> u32 {
    let clamped = byte_offset.min(text.len());
    match text.get(..clamped) {
        Some(prefix) => prefix.encode_utf16().count() as u32,
        // `byte_offset` landed mid-character (shouldn't happen — it comes
        // from `hit_test_byte_column`'s glyph-index lookup, which only ever
        // returns a char-boundary-aligned byte offset) — back up to the
        // nearest valid boundary rather than panic.
        None => {
            let mut boundary = clamped;
            while boundary > 0 && !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            text[..boundary].encode_utf16().count() as u32
        }
    }
}
/// Extra identifier stamped onto the workspace node while the jump-to-file
/// palette is open. Single-char bindings are scoped to `!PaletteOpen` so
/// they keep bubbling into the palette's text input instead of firing.
const PALETTE_CONTEXT: &str = "PaletteOpen";
/// Same mechanism as [`PALETTE_CONTEXT`], for the PR picker (`ctrl-g`).
const PR_PICKER_CONTEXT: &str = "PrPickerOpen";
/// Stamped onto the workspace node while the S8f read-only target viewer
/// (`Workspace::target_viewer`) is open — scopes its `escape`-to-close
/// binding the same way [`PALETTE_CONTEXT`] scopes the palette's.
const TARGET_VIEWER_CONTEXT: &str = "TargetViewerOpen";

pub fn init(cx: &mut App) {
    let browse =
        Some("Workspace && !PaletteOpen && !EditorOpen && !PrPickerOpen && !TargetViewerOpen");
    let palette = Some("Workspace && PaletteOpen");
    let editor = Some("Workspace && EditorOpen");
    let pr_picker = Some("Workspace && PrPickerOpen");
    let target_viewer = Some("Workspace && TargetViewerOpen");
    // Nav back/forward (S8f) stay live both while browsing the diff AND
    // while the target viewer itself is open (jumping BACK from a target
    // viewer to the diff is the common case) — everything `browse` excludes
    // except `TargetViewerOpen`.
    let nav = Some("Workspace && !PaletteOpen && !EditorOpen && !PrPickerOpen");
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
        // cmd-p registered first: gpui's mac menu builder picks a menu
        // item's NSMenu key equivalent via the FIRST binding whose predicate
        // matches (`bindings_for_action(...).find_or_first(...)`, keymap.rs)
        // — all three of these share the same `browse` predicate, so
        // registration order alone decides. Putting the bare "f" binding
        // first would hand the Go > "Jump to File" menu item the key
        // equivalent "f" with an EMPTY modifier mask, which on macOS
        // intercepts every plain "f" keystroke app-wide (performKeyEquivalent
        // beats normal text-input delivery). cmd-p first keeps the display
        // AND the interception target on ⌘P, harmless as a menu accelerator.
        KeyBinding::new("cmd-p", JumpToFile, browse),
        KeyBinding::new("ctrl-p", JumpToFile, browse),
        KeyBinding::new("f", JumpToFile, browse),
        // cmd- twin added S8i (docs/phase-8-lsp-and-polish.md §macOS) —
        // mirrors the cmd-p/ctrl-p pair just above. Additive only.
        KeyBinding::new("cmd-g", OpenPrPicker, browse),
        KeyBinding::new("ctrl-g", OpenPrPicker, browse),
        KeyBinding::new("escape", ClearSelection, browse),
        KeyBinding::new("down", PaletteNext, palette),
        KeyBinding::new("up", PalettePrev, palette),
        KeyBinding::new("escape", PaletteClose, palette),
        KeyBinding::new("down", PrPickerNext, pr_picker),
        KeyBinding::new("up", PrPickerPrev, pr_picker),
        KeyBinding::new("escape", PrPickerClose, pr_picker),
        KeyBinding::new("enter", PrPickerChoose, pr_picker),
        KeyBinding::new("escape", CloseTargetViewer, target_viewer),
        KeyBinding::new("cmd-[", NavBack, nav),
        KeyBinding::new("ctrl-[", NavBack, nav),
        KeyBinding::new("cmd-]", NavForward, nav),
        KeyBinding::new("ctrl-]", NavForward, nav),
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
    /// Added/removed line counts and binary-ness, tallied once here (in
    /// `build_rows`/`error_diff`) rather than re-walking `unified` on every
    /// `Workspace::render` frame — `Self::render_file_diff_header`'s own
    /// per-file diffstat used to do exactly that walk unconditionally on
    /// the hot render path (review finding: new O(rows) work per frame,
    /// growing with file size, on a path Phase 7 worked to keep clean).
    added: u32,
    removed: u32,
    is_binary: bool,
}

impl RenderedDiff {
    /// Estimated heap footprint of this one cached diff — text bytes plus a
    /// rough per-row/per-run overhead, not an exact allocator accounting.
    /// Feeds [`Workspace::estimated_diff_bytes`], the S7-3 workspace LRU's
    /// memory budget. Counts BOTH `unified` AND `split` (cross-cutting risk
    /// F: split view doubles the per-file footprint — old side and new side
    /// each carry their own text/runs — so a diff viewed in split mode
    /// costs roughly twice what unified-only would suggest).
    fn estimated_bytes(&self) -> usize {
        let unified: usize = self
            .unified
            .iter()
            .map(|r| match r {
                Row::Line { text, runs, .. } => {
                    text.len() + runs.len() * std::mem::size_of::<(Range<usize>, HighlightStyle)>()
                }
                Row::HunkHeader { label, .. } => label.len(),
                Row::Binary | Row::NoChanges => 0,
            })
            .sum::<usize>()
            + self.unified.len() * std::mem::size_of::<Row>();
        let split: usize = self
            .split
            .iter()
            .map(|r| match r {
                SplitRow::Pair { left, right } => [left, right]
                    .iter()
                    .flat_map(|c| c.iter())
                    .map(|c| {
                        c.text.len()
                            + c.runs.len() * std::mem::size_of::<(Range<usize>, HighlightStyle)>()
                    })
                    .sum::<usize>(),
                SplitRow::HunkHeader { label, .. } => label.len(),
                SplitRow::Binary | SplitRow::NoChanges => 0,
            })
            .sum::<usize>()
            + self.split.len() * std::mem::size_of::<SplitRow>();
        unified + split + (self.hunk_rows_unified.len() + self.hunk_rows_split.len()) * 8
    }
}

/// Maximum number of PRs' worth of resolved file-list + rendered diffs kept
/// in [`Workspace::pr_diff_cache`] at once (Phase 7 D3). Small on purpose —
/// this is a "toggling back and forth over the last couple of PRs" cache,
/// not a general-purpose store — and its bytes are folded into the S7-3
/// workspace byte budget (`Workspace::estimated_diff_bytes`), so a generous
/// cap here would just eat into that ceiling for little benefit.
const MAX_PR_DIFF_ENTRIES: usize = 3;

/// Content-addressed cache entry for one PR's resolved diff, keyed by
/// `(merge_base, head_oid)` in [`Workspace::pr_diff_cache`]. `diffs` is
/// index-aligned with `files` (cross-cutting risk E) — the two are always
/// stashed and restored together, never independently, so a restore can
/// never pair one PR's file list with another's diffs. `expanded` is a
/// render INPUT baked into `diffs` (gap-expansion state) and is index-keyed
/// the same way, so it travels with the other two rather than being left
/// for `Workspace::expanded` to be cleared out from under the rows it
/// produced (P3 finding: a cache hit used to restore expansion-baked rows
/// while wiping the expansion map, silently collapsing a previously-open
/// gap the next time a *different* gap was expanded).
struct CachedPrDiff {
    files: Vec<ChangedFile>,
    diffs: HashMap<usize, Arc<RenderedDiff>>,
    expanded: HashMap<usize, HashSet<usize>>,
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
    /// Index into `self.remote_threads` (docs/phase-6-review-navigator.md
    /// deliverable 6) — a read-only GitHub-side thread, interleaved
    /// alongside local ones under whichever anchor row it shares (or at
    /// the end, unanchored, same as a local thread with no matching row).
    RemoteThread(usize),
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
    /// Snapshot of `Workspace::pr_picker_epoch` at the moment this picker
    /// was opened. The list-fetch spawned in `on_open_pr_picker` captures
    /// the same value and only applies its result if this still matches on
    /// completion — otherwise a stale fetch from a closed-then-reopened
    /// picker (escape, then `ctrl-g` again before the first fetch lands)
    /// would clobber the reopened picker's fresher state/timing. Mirrors
    /// `source_epoch`'s discard-if-superseded pattern.
    generation: u64,
    /// When the currently-shown list was fetched — drives the "updated Nm
    /// ago" freshness hint and `--automation`'s `age_ms` (Phase 7 D2). Only
    /// `None` during a first-ever `Loading` (nothing cached yet to paint).
    fetched_at: Option<std::time::Instant>,
    /// A background revalidation over an already-shown cached list is in
    /// flight (stale-while-revalidate) — the list stays visible + usable
    /// the whole time. `false` while a first-ever (uncached) load is still
    /// `Loading`, since there's nothing shown yet to call "refreshing".
    refreshing: bool,
    /// Last background-refresh failure, if the most recent revalidation
    /// over an already-shown list failed. The stale list stays on screen
    /// with this as a warning — stale-but-present beats correct-but-empty
    /// for a picker the user is actively trying to act on (docs/
    /// phase-7-performance.md deliverable 2).
    refresh_error: Option<String>,
}

enum PrPickerState {
    Loading,
    Loaded(Vec<PrSummary>),
    Error(String),
}

/// The S8f read-only "jumped to a definition" overlay, while open — plain
/// text, no syntax highlighting or editing (docs/phase-8-lsp-and-polish.md
/// § LSP: "open target file read-only at the location"). Distinct from the
/// diff pane's own `RenderedDiff`: this shows a WHOLE file's current
/// worktree content, not a diff against anything.
struct TargetViewer {
    /// Repo-relative path, for the header + re-resolving on a future nav
    /// hop.
    path: String,
    /// The file's lines, already split (no trailing `\n` per entry) — capped
    /// at `MAX_TARGET_VIEWER_LINES` so a pathologically large target (a
    /// generated `.d.ts`, say) can't make this plain, unvirtualized-list-free
    /// render pathologically slow. `uniform_list` below IS virtualized
    /// (only visible rows render), so the cap is a memory/scroll-length
    /// safety net, not a render-cost one.
    lines: Vec<SharedString>,
    /// The file's REAL line count, captured before `Self::open_target_at`
    /// pushes the "… (file truncated)" notice row onto `lines` — the header
    /// reports this, not `lines.len()`, so a truncated file's header doesn't
    /// count the notice row as if it were file content (P3 finding).
    total_lines: usize,
    /// 0-based line to highlight and scroll to on open (LSP's own
    /// coordinate system — see `crate::lsp::Location`).
    highlight_line: u32,
    scroll: UniformListScrollHandle,
    /// Set once, right after construction, so the very first render scrolls
    /// to `highlight_line` — subsequent renders leave the user's own scroll
    /// position alone. Also pre-set `true` (skipping the scroll entirely)
    /// when `highlight_line` itself falls beyond `MAX_TARGET_VIEWER_LINES`:
    /// `scroll_to_item` would otherwise silently clamp to the last row with
    /// no highlight and no explanation (P3 finding) — see
    /// `Self::open_target_at`.
    scrolled_to_highlight: bool,
}

/// Files larger than this are truncated in the target viewer with a notice
/// rather than rendered whole — mirrors `highlight::MAX_HIGHLIGHT_LINE`'s
/// "don't choke on a pathological file" posture, just for line COUNT here
/// rather than a single line's length.
const MAX_TARGET_VIEWER_LINES: usize = 20_000;

/// How long after a go-to-definition session first reaches `Ready` an EMPTY
/// `definition` result is treated as "vtsls is still warming up" rather
/// than "genuinely no definition" and retried (see
/// `Workspace::run_definition_request`'s warm-up retry — P2 finding).
/// vtsls's project load can legitimately take several seconds; a warm
/// session's genuinely-empty answer stays instant once this window has
/// passed.
const LSP_WARMUP_WINDOW: std::time::Duration = std::time::Duration::from_secs(8);
/// Bound on extra empty-result retries `run_definition_request` performs
/// inside `LSP_WARMUP_WINDOW` before giving up and reporting "no
/// definition found" for real.
const LSP_WARMUP_RETRIES: u32 = 6;
/// Delay between each of `LSP_WARMUP_RETRIES`'s retries.
const LSP_WARMUP_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// A go-to-definition request stashed while `Workspace::lsp_session` is
/// still `Spawning` — see `Workspace::lsp_pending_definition`'s doc comment
/// for the two races this fixes. Carries exactly the arguments
/// `Workspace::run_definition_request` needs, captured at click time.
struct PendingDefinition {
    repo: Arc<GitRepo>,
    rel_path: String,
    uri: String,
    language_id: &'static str,
    position: lsp_types::Position,
    from: crate::lsp::Location,
    range_head: Option<String>,
    epoch: u64,
}

/// How long a mouse-move over an eligible token waits, with no further
/// move, before `Self::on_symbol_hover` actually issues a `textDocument/hover`
/// round trip (S8g) — mirrors gpui-component's own hover-tooltip debounce so
/// a mouse sweep across a line doesn't fire one request per pixel (this
/// slice's gpui gotcha). Deliberately much shorter than
/// [`LSP_WARMUP_WINDOW`]'s multi-second scale: this is a per-move
/// UI-responsiveness debounce, not a server-warm-up wait.
const HOVER_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

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
    /// `dv_cli::submit::build_submission`. No `gh` write has happened yet.
    Validating {
        // Only read by `automation_state`'s JSON dump — the render match
        // below matches this variant with `{ .. }` since the panel text is
        // the same regardless of verdict. Field stays unconditionally
        // constructed (see `Self::begin_submit_flow`), so it's `allow`ed
        // rather than `cfg`'d out, under `--no-default-features`.
        #[cfg_attr(not(feature = "automation"), allow(dead_code))]
        verdict: dv_core::Verdict,
    },
    /// Validation found problems — a stale/unanchored comment, or one that
    /// fell outside the PR's diff. Nothing was sent; nothing can be until
    /// the underlying comments (or the PR itself) change and the verdict is
    /// clicked again.
    Blocked {
        #[cfg_attr(not(feature = "automation"), allow(dead_code))]
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
    Submitting {
        #[cfg_attr(not(feature = "automation"), allow(dead_code))]
        verdict: dv_core::Verdict,
    },
    /// GitHub accepted the review and the local writeback succeeded.
    Done {
        verdict: dv_core::Verdict,
        url: String,
    },
    /// Either the `gh` submission itself failed, or (rarer, and far worse)
    /// it succeeded but the local writeback then failed — `message` is
    /// `dv_cli::submit::writeback_failure_message`'s text in that case,
    /// which warns against ever retrying.
    Failed {
        #[cfg_attr(not(feature = "automation"), allow(dead_code))]
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
    /// A weak handle to the owning `AppShell`, captured at construction.
    /// Read-only here — used solely by [`Self::on_open_pr_picker`] to ask
    /// `AppShell::overlay_open` whether a shell-level overlay (theme picker
    /// / settings panel) is genuinely open right now (review finding P1,
    /// refined by finding P3: the original guard used
    /// `shell_focus_handle.is_focused()` as a proxy for "an overlay is
    /// open", but ordinary sidebar-chrome clicks — e.g. a group header,
    /// with no `on_click`/`track_focus` of their own — also bubble focus
    /// onto the shell handle with no overlay open at all, so that proxy
    /// silently declined legitimate mouse-triggered opens). Declining to
    /// open the PR picker while an overlay genuinely is open (mirroring
    /// `on_open_theme_picker` declining while `settings_panel` is open)
    /// prevents the two overlays from ever stacking, which is what let a
    /// still-open theme picker end up keyboard-trapped behind the PR picker
    /// once this workspace's own `escape` bindings started outranking it.
    shell: WeakEntity<crate::shell::AppShell>,
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
    /// Memoizes [`Self::diffstat`]'s row walk (review finding P3-5: that
    /// walk is O(total loaded rows) over every `self.diffs` entry, and
    /// `render_header` — plus `automation_state`'s `state` dump — calls it
    /// on every render, an unbounded per-frame cost as a large review
    /// accumulates loaded files). `None` means dirty; every explicit
    /// `self.diffs` mutation site (the `request_diff` completion, all
    /// `self.diffs.clear()` sites, and the PR-cache restore/miss paths)
    /// resets it to `None`, and `diffstat` repopulates it lazily on next
    /// read. A `Cell` rather than a plain field because `diffstat` is
    /// called from `&self` render/automation paths that can't take
    /// `&mut self` just to memoize.
    diffstat_cache: Cell<Option<(u32, u32)>>,
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
    /// Set when a theme/context-lines change staled this workspace's cached
    /// `diffs` while it was PARKED with no live host (WSL, no dv-host — see
    /// `invalidate_diff_cache`'s non-eager path, which can't recompute then
    /// without booting the distro). `revalidate` rebakes the selected file's
    /// diff on reactivation when this is set, so it never paints under the
    /// old theme's baked syntax colors (capstone P2).
    diffs_theme_stale: bool,
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
    /// An explicit sidebar-row pick (docs/phase-6-review-navigator.md S6c —
    /// a card click via `AppShell::open_review_row`, or
    /// `{"cmd":"select_review"}`), threaded in at construction from
    /// `AppShell::open_review`'s `pinned_review_id` param. Takes precedence
    /// over `pick_review`'s own auto-selection at every call site (initial
    /// load and every watcher-driven reload) — including a SUBMITTED
    /// review, so an explicit pick isn't silently un-pinned by the next
    /// external edit. `None` for every open that isn't an explicit pick (a
    /// brand-new repo, `dv pr <n>`'s `pending_pr`, a plain re-open); an
    /// explicit [`Self::open_pr`] clears it too — a PR open is itself a
    /// different explicit selection.
    pinned_review_id: Option<String>,
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
    /// GitHub-side review threads for the currently-open PR, read-only
    /// (docs/phase-6-review-navigator.md deliverable 6) — fetched by
    /// [`Self::refresh_remote_threads`], interleaved into `self.display`
    /// alongside local threads by [`Self::reset_diff_list`]. Always empty
    /// with no PR open; cleared and re-fetched on every `open_pr`.
    remote_threads: Vec<RemoteThread>,
    /// Local comment ids whose matching own-submitted-review GitHub thread
    /// (`review_database_id == pr_remote.submitted_review_id`) is resolved
    /// on github.com — recomputed by [`Self::reset_diff_list`] from
    /// `remote_threads` on every rebuild. For a thread `reset_diff_list`
    /// manages to position-match to a local comment (and dedupes out of the
    /// read-only `RemoteThread` render as a result — it would double up
    /// with the local `Thread` card for the same comment), this is how its
    /// resolved state still reaches the UI: `render_thread` and `render_
    /// summary` OR it into their "resolved" check alongside the local
    /// `CommentStatus` (review finding: the dedup used to just discard the
    /// resolved bit, defeating deliverable 6's headline case for exactly
    /// the threads `submitted_review_id` exists to identify — "a review
    /// resolved on github.com shows resolved in dv"). An own thread that
    /// CAN'T be position-matched (outdated/line-drifted, or carrying a
    /// GitHub-only reply) isn't dropped either — `reset_diff_list` leaves
    /// it out of the dedup instead, so it still renders as its own
    /// read-only `RemoteThread` card (review finding: those were silently
    /// invisible everywhere).
    github_resolved: HashSet<String>,
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
    /// Last `gh pr list` result for this workspace's repo, reused to paint
    /// the picker instantly on reopen instead of a spinner (Phase 7 D2).
    /// In-memory only; dies with the workspace today, and is kept alive
    /// across sidebar switches for free once the S7-3 workspace LRU reuses
    /// the entity. Tiny (`Vec<PrSummary>`: ints + short strings), scoped to
    /// this one workspace/repo — not part of the S7-3 byte budget (that
    /// tracks rendered-diff bytes, which this isn't). No TTL: `ctrl-g`
    /// always revalidates in the background regardless of age (see
    /// `on_open_pr_picker`); the `Instant` here is purely the freshness
    /// hint's/`age_ms`'s timestamp, not an expiry.
    pr_list_cache: Option<(Vec<PrSummary>, std::time::Instant)>,
    /// `pr_picker_epoch` generation of the fetch that most recently wrote
    /// `pr_list_cache`. The cache write in `on_open_pr_picker`'s completion
    /// is deliberately unconditional on picker liveness (a closed, or
    /// closed-then-reopened, picker must not discard a successful fetch —
    /// review finding), but two in-flight fetches can still land
    /// out-of-order; this lets the completion accept only a fetch at least
    /// as new as the one that already wrote the cache, so an older result
    /// can't clobber a fresher one. `None` until the first fetch lands.
    pr_list_cache_generation: Option<u64>,
    /// Bumped every `on_open_pr_picker` call; stamped into the new
    /// `PrPicker::generation` and captured by that open's list-fetch spawn
    /// so a stale fetch from a since-closed-and-reopened picker can be
    /// told apart from the current one on completion (see `PrPicker::
    /// generation`'s doc comment).
    pr_picker_epoch: u64,
    /// Wall-clock of the most recent `ctrl-g` PR-picker open, from dispatch
    /// to the list leaving `Loading` (`Loaded`/`Error`) on a cold (no cache)
    /// open. Phase 7 D4 instrumentation (mirrors `last_diff_ms`), the cold
    /// baseline the S7-2 picker cache asserts a warm reopen against — a
    /// warm open stamps this near-instantly instead (see
    /// `on_open_pr_picker`), since there's no `Loading` frame to wait out.
    last_pr_list_ms: Option<u64>,
    /// Wall-clock of the most recent `open_pr`, from dispatch to its
    /// current-epoch completion (`Status::Ready` either way, success or
    /// error) — Phase 7 D4 instrumentation. NOT a warm-vs-cold signal for
    /// the S7-5 PR-reopen cache (review finding, P3): this only times
    /// `load_pr` (`gh pr view` + fetch + merge-base + `changed_files`),
    /// which runs identically whether or not `pr_diff_cache` has a hit — the
    /// work the cache actually skips (the per-file tree-sitter
    /// `compute_diff` pass) happens later, inside `select_file`/
    /// `request_diff`, and is timed separately into `last_diff_ms` on a
    /// miss (never invoked at all on a hit). Use `last_pr_open_cache_hit`
    /// to assert warm vs. cold, not a `<<` comparison on this field.
    last_pr_open_ms: Option<u64>,
    /// Content-addressed cache of a PR's resolved file-list + rendered
    /// diffs, keyed by [`pr_source_key`] — Phase 7 D3. Reopening the same
    /// `(merge_base, head_oid)` pair reuses this instead of re-running
    /// `changed_files` + a tree-sitter `compute_diff` pass per file (the
    /// real per-recon cost). `pr_meta` (state/decision/CI/title) is NEVER
    /// stored here — `open_pr`'s `load_pr` call fetches it fresh on every
    /// open regardless of a diff-cache hit, so a force-push or a status
    /// change is always caught even when the diff itself is reused. Cleared
    /// wholesale by `invalidate_diff_cache` (a theme/context-lines change
    /// re-bakes diff colors/hunk structure, so cached rows would otherwise
    /// go stale under the new theme) — that single choke point is why no
    /// per-entry theme fingerprint is needed here. Its bytes are folded
    /// into `estimated_diff_bytes`, the S7-3 workspace LRU's memory budget
    /// (cross-cutting risk F).
    pr_diff_cache: HashMap<(String, String), CachedPrDiff>,
    /// Front = most-recently used, back = evict next — bounds
    /// `pr_diff_cache` at [`MAX_PR_DIFF_ENTRIES`] entries.
    pr_diff_lru: VecDeque<(String, String)>,
    /// Whether the most recent `open_pr` reused a cached diff
    /// (`pr_diff_cache` hit, `true`) or recomputed from scratch (`false`) —
    /// Phase 7 D3 automation assertion. `None` before the first `open_pr`.
    last_pr_open_cache_hit: Option<bool>,
    /// Comment/reply author, resolved once in the background at load
    /// (`dv_cli::author::resolve_author`). `None` until that resolves —
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
    /// The most recent [`Self::switch_source_and_jump`] failure, if any
    /// (docs/phase-6-review-navigator.md deliverable 5) — a comment whose
    /// file isn't reachable in *any* known source (should be impossible for
    /// a well-formed comment) or a background fetch error (e.g. a rebased-
    /// away oid). Deliberately separate from `Status::Failed` for the same
    /// reason `pr_error` is: the workspace stays exactly as usable as it was
    /// before the attempt. Cleared on the next attempt.
    source_switch_error: Option<String>,
    /// S8f (docs/phase-8-lsp-and-polish.md § LSP) go-to-definition session,
    /// spawned at most once for this workspace's lifetime — see
    /// `crate::lsp`'s module doc for the lazy-spawn/kill-on-drop contract.
    /// `Workspace` never has more than one live [`dv_core::lsp::LspHandle`]
    /// clone at a time; dropping the workspace drops this field, which
    /// drops the last `Arc` and kills the vtsls child.
    lsp_session: crate::lsp::LspSessionState,
    /// Bumped on every event that supersedes whatever go-to-definition
    /// round trip is currently in flight — a fresh symbol click, a
    /// `NavBack`/`NavForward` press, a file switch, or closing the target
    /// viewer. Every async LSP completion (`Self::run_definition_request`'s
    /// definition round trip, `Self::open_target_at`'s blob read) captures
    /// the epoch at the moment it was kicked off and re-checks it before
    /// mutating `lsp_session`/`nav_stack`/`target_viewer`, discarding the
    /// result silently on a mismatch (P3 findings: two rapid `NavBack`
    /// presses both peeking the same stack entry and both committing —
    /// desyncing history; and a slow completion reopening the target-viewer
    /// modal after the user closed it or moved to another file/click).
    lsp_request_epoch: u64,
    /// Back/forward history over target-viewer jumps — see
    /// `crate::lsp::NavStack`'s doc comment.
    nav_stack: crate::lsp::NavStack,
    /// The read-only "jumped to a definition" overlay, if one is open.
    target_viewer: Option<TargetViewer>,
    /// Human-readable reason the most recent go-to-definition attempt
    /// didn't land (no session, request failed, empty result, non-honest
    /// view, non-TS file, …) — surfaced as a small status line rather than
    /// a hard error (docs/phase-8-lsp-and-polish.md § LSP: "surface a
    /// gentle warning ... don't fail"). Cleared on every successful jump
    /// and on every file switch (`Self::select_file_inner`) so it can't go
    /// stale.
    lsp_status: Option<SharedString>,
    /// A go-to-definition request received while `lsp_session` is still
    /// `Spawning` — the click that FIRST triggers `Unattempted -> Spawning`
    /// stashes its own request here too (rather than closing over it
    /// directly), so every completion path replays whatever request is
    /// CURRENTLY stashed, not necessarily the one that happened to kick the
    /// spawn off. Fixes two related P3 races: (a) a second click arriving
    /// while still `Spawning` used to just show "starting…" and be dropped,
    /// requiring a third click once the session came up; (b) parking mid-
    /// spawn and reactivating with a fresh click could let the FIRST spawn
    /// attempt land, install `Ready`, and then discard the reactivated
    /// click's own request because its epoch no longer matched the first
    /// attempt's. Only one request is ever stashed at a time — a later click
    /// simply replaces it, the same superseding posture `lsp_request_epoch`
    /// uses everywhere else in this module. Taken (and re-checked against
    /// the current epoch) by whichever spawn attempt completes first.
    lsp_pending_definition: Option<PendingDefinition>,
    /// Count of go-to-definition async round trips currently in flight —
    /// `Self::run_definition_request`'s definition lookup and
    /// `Self::open_target_at`'s target-file read each increment this right
    /// before `cx.spawn`-ing and decrement it the moment their completion
    /// closure runs, epoch match or not (phase-8 capstone integration
    /// review, P3). `Self::automation_settled` treats a nonzero count the
    /// same as `Spawning`/`lsp_pending_definition.is_some()`: without this,
    /// `wait_ready` returned "settled" while a ctrl/cmd-click's definition
    /// lookup or the subsequent target-viewer file read was still
    /// in-flight, making `state.lsp.target_viewer` a race between the
    /// script and vtsls rather than a deterministic read.
    lsp_inflight_requests: u32,
    /// Bumped every time a NEW vtsls spawn attempt is kicked off (the
    /// `Unattempted -> Spawning` transition in `Self::on_symbol_click`) or
    /// the session is parked (`Self::park_lsp_session`) — stamped onto that
    /// attempt's async completion closure and re-checked before installing
    /// `Ready`/`Unavailable`. A bare `matches!(lsp_session, Spawning)` check
    /// can't tell two overlapping attempts apart: park+reactivate (or two
    /// rapid clicks racing the `Unattempted` guard) can have a STALE first
    /// attempt's completion land after a second attempt has already started
    /// — under the old `Spawning`-only guard, the stale attempt's `Err`
    /// could install `Unavailable` and clear the second attempt's stashed
    /// click, even though the second attempt goes on to succeed (whose
    /// handle then gets silently discarded) (P3 finding). Only the attempt
    /// whose captured generation still matches this field when it completes
    /// is allowed to mutate `lsp_session`/`lsp_pending_definition`; every
    /// other attempt's result (`Ok` or `Err`) is discarded outright.
    lsp_spawn_generation: u64,
    /// Stamped the moment `lsp_session` first reaches `Ready` for the
    /// current spawn attempt (and cleared on `Self::park_lsp_session`) —
    /// `Self::run_definition_request` uses it to tell "vtsls is still
    /// warming up" apart from "genuinely no definition" for an empty
    /// result: vtsls's project load can legitimately take several seconds
    /// after `didOpen`, and a session that JUST reached `Ready` is exactly
    /// the case where the very first click races that load (docs/
    /// phase-8-lsp-and-polish.md § LSP.1; P2 finding — this used to map any
    /// empty result straight to "no definition found", including that
    /// common cold-first-click case, with no retry or loading state).
    lsp_ready_since: Option<std::time::Instant>,
    /// Set once, right after a session first reaches `Ready`, when this
    /// repo's `node_modules` couldn't be found — package-symbol lookups
    /// (anything resolving into a dependency) silently return an empty
    /// result without it installed, which otherwise reads to the user as
    /// "no definition found" rather than "the project isn't installed"
    /// (docs/phase-8-lsp-and-polish.md § LSP.1: "surface a gentle warning
    /// when absent, don't fail" — P2 finding: this warning didn't exist at
    /// all). Unlike `lsp_status`, this is a persistent, session-level note
    /// (not cleared by a successful jump or a file switch) — it stays until
    /// the session itself is reset (`Self::park_lsp_session`).
    lsp_node_modules_warning: Option<SharedString>,
    /// The worktree's current `HEAD` oid, refreshed off-thread on initial
    /// load and every [`Self::revalidate`] pass — the cache
    /// `Self::lsp_view_is_honest` compares a `DiffSource::Range`'s `head`
    /// against (the plan's key_signature: "WorkingTree || New-side head oid
    /// == worktree HEAD") without a synchronous git round trip per click.
    /// `None` until the first successful `git rev-parse HEAD` (never-fail-
    /// hard: a resolve failure just leaves `Range` views ungated, not a
    /// crash).
    worktree_head_oid: Option<String>,
    /// S8g (docs/phase-8-lsp-and-polish.md § LSP) hover popover, if one is
    /// currently shown — reuses `lsp_session`'s already-`Ready` handle (see
    /// `Self::on_symbol_hover`'s doc comment: hover never spawns its own
    /// session). `None` covers both "nothing hovered yet" and "the last
    /// hover answer was empty/failed/gated".
    hover_popover: Option<crate::lsp::HoverPopover>,
    /// Bumped on every mouse-move over an eligible (New-side) diff token —
    /// the debounce+supersede counterpart to `lsp_request_epoch`, kept
    /// separate so a hover sweep never cancels an in-flight go-to-definition
    /// round trip (and vice versa: opening the target viewer doesn't need
    /// to invalidate a hover that happens to still be in flight elsewhere).
    /// A debounced hover request re-checks this after its sleep, and again
    /// after the round trip itself, discarding silently on any mismatch —
    /// same posture as every other epoch in this file.
    hover_request_epoch: u64,
    /// The diff line `hover_request_epoch`'s current value was bumped FOR —
    /// i.e. which row owns the in-flight debounced request, if any. Exists
    /// so `Self::on_symbol_hover_leave` can tell "the epoch I'd invalidate is
    /// still mine" apart from "a fresher hover (on some other row) already
    /// superseded it" (P3 finding): gpui dispatches bubble-phase mouse
    /// listeners in reverse PAINT order (`window.rs`'s mouse dispatch
    /// `.rev()`s the bubble pass), and rows paint top-to-bottom, so a single
    /// `MouseMoveEvent` jumping the cursor DOWN from a hovered row to a row
    /// painted later fires the destination row's `on_mouse_move` (which
    /// bumps the epoch and claims this field) BEFORE the origin row's own
    /// `.on_hover` leave listener runs. Without this guard the leave's own
    /// unconditional bump would re-supersede the epoch the destination row
    /// JUST claimed, discarding its own answer once the debounce elapses —
    /// silently, direction-dependent, and only self-healing on a SECOND
    /// mouse move (which a one-shot automation `mouse_move` command never
    /// sends).
    hover_request_line: Option<u32>,
    /// This workspace's own root element's on-screen bounds, refreshed every
    /// paint via a zero-size `canvas` probe in `Self::render` — the nearest
    /// positioned (`.relative()`) ancestor an `.absolute()` hover-popover
    /// child resolves against. `Self::render_hover_popover` subtracts this
    /// origin from `HoverPopover::anchor` (a WINDOW-relative point, the only
    /// kind a raw `MouseMoveEvent` carries) to get a position relative to
    /// THIS root, mirroring how `Self::wrap_symbol_click_target`'s own probe
    /// turns a window-relative click into a row-relative column.
    root_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
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
/// `pinned_id` (docs/phase-6-review-navigator.md S6c) takes precedence over
/// everything below it: an explicit sidebar-row pick (a card click via
/// `AppShell::open_review_row`, or `{"cmd":"select_review"}`) wins outright
/// when it still resolves to a review in this listing — ANY state,
/// including SUBMITTED (the whole point: a submitted review is otherwise
/// unreachable once it's not the newest draft and no PR is open). A vanished
/// pin (the review was deleted between the click and this listing) falls
/// through to the logic below rather than returning `None` and blanking the
/// workspace — same "never go blank over a stale reference" posture the
/// PR-arm's own fallback already has.
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
    pinned_id: Option<&str>,
) -> Option<dv_core::Review> {
    if let Some(pin) = pinned_id
        && let Some(pinned) = reviews.iter().find(|r| r.id == pin)
    {
        return Some(pinned.clone());
    }
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

/// What `pinned_review_id` should become right after a `pick_review` result
/// lands. A pin only makes sense pointing at the review actually being
/// shown — if it didn't resolve (the pinned review was deleted between the
/// click and this listing, and `pick_review` fell through to something
/// else per its own "never go blank over a stale reference" posture), the
/// pin is dropped rather than left dangling. A stale pin left pointing at a
/// vanished id would otherwise still read as `is_some()` and permanently
/// block new comments (`Workspace::new_comment_blocked`) on a review
/// nobody explicitly selected (review finding, docs/phase-6-review-
/// navigator.md S6c).
fn resolved_pin(pinned_id: Option<String>, review: &Option<dv_core::Review>) -> Option<String> {
    pinned_id.filter(|pin| review.as_ref().is_some_and(|r| &r.id == pin))
}

/// The bordered "LSP chip" (title-bar spec: `border color@0.45, bg
/// color@0.1, rounded_sm`, 11px text) — the reference design's
/// LSP-status recipe, adopted
/// verbatim per the spec rather than forced through `shell::state_pill`'s
/// `Tag::custom` recipe (`.15`/`.4` opacity), which is a visually distinct
/// pill meant for PR/file/review state. `Self::render_header` renders one of
/// these per populated LSP field (`lsp_status`, `lsp_node_modules_warning`)
/// — both can show at once, capped/truncating so a long message can't blow
/// out the header's right cluster. `flex_shrink_1` + `min_w(0)` rather than
/// `flex_none` (review finding P3-4): at `max_w`, up to two of these plus an
/// unbounded refs label could together exceed a narrow (1280px) window's
/// width with nothing able to give, pushing the trailing action buttons off
/// screen — letting the chip itself compress under pressure (it still never
/// grows past `max_w`) keeps the buttons on screen instead.
fn lsp_chip(text: SharedString, color: Hsla) -> impl IntoElement {
    div()
        .flex_shrink_1()
        .flex_grow_0()
        .min_w(px(0.))
        .max_w(px(280.))
        .truncate()
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(color.opacity(0.45))
        .bg(color.opacity(0.1))
        .text_size(px(11.))
        .text_color(color)
        .child(text)
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
        added: 0,
        removed: 0,
        is_binary: false,
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

/// `pub(crate)` (docs/phase-6-review-navigator.md S6b) so `shell.rs`'s
/// review-index automation dump can describe an `IndexEntry`'s source with
/// the exact same words this workspace's own `automation_state.source`
/// uses, rather than growing a near-duplicate match arm over there.
pub(crate) fn source_label(source: &DiffSource) -> &'static str {
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
/// small, deliberately duplicated copies of `crates/cli/src/pr_cmd.rs`'s
/// private equivalents (that module isn't `pub`, and these are one match
/// arm each). `pub(crate)` so `shell.rs`'s sidebar badge dump
/// (deliverable 3) reuses these instead of growing a third copy — that
/// module isn't a descendant of this one the way `dv_cli`'s `pr_cmd` fails
/// to be, so plain visibility is enough.
#[cfg(feature = "automation")]
pub(crate) fn pr_state_word(state: PrState) -> &'static str {
    match state {
        PrState::Open => "open",
        PrState::Closed => "closed",
        PrState::Merged => "merged",
    }
}

#[cfg(feature = "automation")]
pub(crate) fn checks_word(checks: ChecksSummary) -> &'static str {
    match checks {
        ChecksSummary::Passing => "passing",
        ChecksSummary::Failing => "failing",
        ChecksSummary::Pending => "pending",
        ChecksSummary::None => "none",
    }
}

#[cfg(feature = "automation")]
pub(crate) fn review_decision_word(decision: ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::Approved => "approved",
        ReviewDecision::ChangesRequested => "changes_requested",
        ReviewDecision::ReviewRequired => "review_required",
    }
}

/// `--automation`'s word for a [`GhSide`], matching this file's existing
/// `DiffSide` vocabulary ("old"/"new") rather than GitHub's own LEFT/RIGHT
/// spelling — `automation_state`'s `selection.side` already uses "old"/
/// "new", and a remote thread's side means the exact same diff-side concept
/// (docs/phase-6-review-navigator.md deliverable 6).
#[cfg(feature = "automation")]
fn gh_side_word(side: GhSide) -> &'static str {
    match side {
        GhSide::Left => "old",
        GhSide::Right => "new",
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
#[cfg(feature = "automation")]
fn verdict_automation_word(verdict: dv_core::Verdict) -> &'static str {
    match verdict {
        dv_core::Verdict::Comment => "comment",
        dv_core::Verdict::Approve => "approve",
        dv_core::Verdict::RequestChanges => "request_changes",
    }
}

/// snake_case word for a [`ViolationKind`], for `--automation`'s JSON.
#[cfg(feature = "automation")]
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

/// The content-addressed key `Workspace::pr_diff_cache` is keyed by, when
/// `source` is a PR's resolved diff range: `(merge_base, head_oid)`. `None`
/// for every other `DiffSource` variant (`WorkingTree`/`Staged`/`Commit`
/// never go through the PR-reopen cache). `load_pr` sets `base` to the
/// already-resolved merge-base tip (not a symbolic ref), so this pair is
/// stable and safe to cache against regardless of elapsed time — it only
/// changes on a force-push (new `head_oid`) or a rebase/merge of the base
/// branch (new `merge_base`), both of which are exactly the cases that
/// should miss and recompute.
fn pr_source_key(source: &DiffSource) -> Option<(String, String)> {
    match source {
        DiffSource::Range { base, head, .. } => Some((base.clone(), head.clone())),
        _ => None,
    }
}

/// Fetch a PR's metadata, make sure its diff range is available locally,
/// reload the changed-file list against it, and find-or-create the draft
/// review it links to — everything [`Workspace::open_pr`] needs, done
/// off-thread in one shot so the UI only ever sees a finished outcome.
fn load_pr(repo: &GitRepo, number: u64, location: RepoLocation) -> anyhow::Result<PrOpenOutcome> {
    let client = GithubClient::for_repo(repo)?;
    client.preflight()?;
    let meta = client.pr_meta(number)?;
    let range = dv_cli::pr::prepare_pr(repo, &meta)?;
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
        pinned_review_id: Option<String>,
        view_mode_default: ViewModeSetting,
        context_lines: u32,
        font_size: f32,
        summary_width: f32,
        shell: WeakEntity<crate::shell::AppShell>,
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
            shell,
            title: location.display_name().into(),
            source_desc: source_label(&source).into(),
            location: location.clone(),
            pinned_review_id: pinned_review_id.clone(),
            review: None,
            editor: None,
            display: Vec::new(),
            diff_to_display: Vec::new(),
            source: source.clone(),
            source_epoch: 0,
            highlight_epoch: 0,
            diffs_theme_stale: false,
            status: Status::Loading,
            repo: None,
            head: "".into(),
            files: Vec::new(),
            selected: None,
            diffs: HashMap::new(),
            diffstat_cache: Cell::new(None),
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
            remote_threads: Vec::new(),
            github_resolved: HashSet::new(),
            pr_details_open: false,
            pr_loading: None,
            pr_error: None,
            pr_picker: None,
            pr_list_cache: None,
            pr_list_cache_generation: None,
            pr_picker_epoch: 0,
            last_pr_list_ms: None,
            last_pr_open_ms: None,
            pr_diff_cache: HashMap::new(),
            pr_diff_lru: VecDeque::new(),
            last_pr_open_cache_hit: None,
            author: None,
            submit: None,
            submit_epoch: 0,
            source_switch_error: None,
            lsp_session: crate::lsp::LspSessionState::Unattempted,
            lsp_request_epoch: 0,
            nav_stack: crate::lsp::NavStack::default(),
            target_viewer: None,
            lsp_status: None,
            lsp_pending_definition: None,
            lsp_inflight_requests: 0,
            lsp_spawn_generation: 0,
            lsp_ready_since: None,
            lsp_node_modules_warning: None,
            worktree_head_oid: None,
            hover_popover: None,
            hover_request_epoch: 0,
            hover_request_line: None,
            root_bounds: Rc::new(Cell::new(None)),
        };

        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while watch_rx.next().await.is_some() {
                // Coalesce event bursts (temp write + rename fire separately)
                // into one reload.
                while watch_rx.try_recv().is_ok() {}
                // The normalized location (set by the load task), the
                // review currently shown so it isn't dropped when a submit
                // leaves no draft behind, the PR this workspace is scoped to
                // (if any) so a reload can't adopt some other PR's draft out
                // from under it (review finding P1-b), and the explicit
                // sidebar-row pin (if any — docs/phase-6-review-navigator.md
                // S6c) so a reload can't silently un-pin a SUBMITTED review
                // the user explicitly selected.
                let Ok((location, current_id, current_pr, pinned_id, start_epoch, pr_in_flight)) =
                    this.update(cx, |this, _| {
                        (
                            this.location.clone(),
                            this.review.as_ref().map(|r| r.id.clone()),
                            this.pr_remote.clone(),
                            this.pinned_review_id.clone(),
                            this.source_epoch,
                            this.pr_loading.is_some(),
                        )
                    })
                else {
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
                            pinned_id.as_deref(),
                        )
                    })
                    .await;
                let alive = this.update(cx, |this, cx| {
                    // Discard-if-superseded: this reload's `pick_review` inputs
                    // (`current_id`/`current_pr`) were snapshotted before an
                    // explicit navigation moved the workspace. `open_pr` and
                    // `switch_source_and_jump` bump `source_epoch` at dispatch,
                    // so a changed epoch means a newer source is (being) loaded;
                    // and a PR already loading at snapshot time (its epoch bump
                    // is before this snapshot, so the epoch check alone can't
                    // see it) would let this reload clobber the just-loaded PR's
                    // review with the pre-load one — later comments would then
                    // save against the wrong review (capstone P1/P2). Either
                    // way drop this stale reload; the watcher re-fires on the
                    // next event, and the navigation owns whichever review it
                    // lands on.
                    if this.source_epoch != start_epoch || pr_in_flight {
                        return;
                    }
                    // Shared with `Self::revalidate`'s review half and the
                    // worktree-watch consumer's sibling loop below — see
                    // `apply_review_reload`'s doc comment (fingerprint
                    // reconciliation + the monotonicity guard against a
                    // faster concurrent writer).
                    if this.apply_review_reload(review, cx) {
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
                    // Shared with `Self::revalidate`'s worktree half — see
                    // `apply_worktree_reload`'s doc comment (index-keyed
                    // reconciliation via `reconcile_file_selection`, never a
                    // blind swap).
                    if let Ok(files) = files {
                        this.apply_worktree_reload(files, cx);
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
                    // Full HEAD oid, cached for `Self::lsp_view_is_honest`'s
                    // Range-view comparison (never-fail-hard: an unresolved
                    // HEAD — e.g. a brand-new repo with no commits yet —
                    // just leaves Range views ungated for LSP, not a load
                    // failure).
                    let worktree_head_oid = repo.resolve("HEAD").ok();
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
                        pinned_review_id.as_deref(),
                    );
                    // Resolved here (off-thread: it may hit a `gh` subprocess
                    // on first use) so new comments/replies stamp the real
                    // author from the very first one, not just once some
                    // later save happens to trigger it.
                    let author = dv_cli::author::resolve_author(&repo);
                    anyhow::Ok((
                        Arc::new(repo),
                        head,
                        worktree_head_oid,
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
                    Ok((
                        repo,
                        head,
                        worktree_head_oid,
                        files,
                        source,
                        review,
                        store_location,
                        author,
                    )) => {
                        this.repo = Some(repo);
                        this.head = head.into();
                        this.worktree_head_oid = worktree_head_oid;
                        this.files = files;
                        this.source = source;
                        this.review = review;
                        this.pinned_review_id =
                            resolved_pin(this.pinned_review_id.take(), &this.review);
                        // A PR-linked review reached directly (sidebar
                        // click, not `open_pr`) still needs `pr_remote`
                        // populated so `refresh_remote_threads` has a
                        // slug/pr to act on when the user hits the manual
                        // `RefreshBadges` gesture (review finding:
                        // sidebar-opened submitted PR reviews never synced
                        // remote threads because this was left `None`).
                        // `refresh_pr`'s own no-op (it keys off `self.pr`,
                        // the PR *header*, which nothing populates on this
                        // path) is a separate, pre-existing gap — fixing it
                        // needs an initial header fetch, a bigger change
                        // out of scope for this slice's remote-thread sync.
                        // Skipped when `pending_pr` is set — `open_pr` right
                        // below assigns its own (possibly different)
                        // `pr_remote` from the PR it's about to load, which
                        // would immediately overwrite this anyway.
                        if pending_pr.is_none() {
                            this.pr_remote = this.review.as_ref().and_then(|r| r.remote.clone());
                        }
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

        // P3 finding: a shown hover popover has no other way to clear when
        // the window loses OS focus (alt-tab away, clicking another app) —
        // no further in-window `MouseMoveEvent` ever arrives to run
        // `Self::on_symbol_hover_leave`, so without this it would sit
        // rendered over a backgrounded window until focus returns AND the
        // pointer happens to cross an eligible row again. Registered once,
        // for this workspace's whole lifetime — the SubscriberSet drops it
        // on its own once `view.update` starts failing (workspace gone).
        cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                // Bump the epoch and release the claimed row UNCONDITIONALLY
                // (not just when a popover is already shown) — an in-flight
                // debounced hover request that hasn't landed yet must still
                // be superseded here, or its answer can land after focus
                // returns and pop a stale popover the deactivation was
                // supposed to prevent (P2 finding). `cx.notify()` stays
                // conditional: nothing to repaint if no popover was showing.
                this.hover_request_epoch += 1;
                this.hover_request_line = None;
                if this.hover_popover.take().is_some() {
                    cx.notify();
                }
            }
        })
        .detach();

        this
    }

    /// The active review, if one has loaded yet (`None` until the initial
    /// load's `ReviewChanged` — see [`Self::new`] — and again briefly
    /// during a watcher-driven reload). `pub(crate)` (docs/phase-6-review-
    /// navigator.md S6b) so `shell.rs`'s `ReviewChanged` subscription can
    /// fold this workspace's review into the cross-repo index without
    /// reaching into a private field.
    pub(crate) fn review(&self) -> Option<&dv_core::Review> {
        self.review.as_ref()
    }

    /// This workspace's repo location — normalized to the store's true
    /// toplevel once the initial load completes (see [`Self::new`]'s
    /// `this.location = store_location` reassignment), the raw argument
    /// before that. Same `pub(crate)` reasoning as [`Self::review`].
    pub(crate) fn location(&self) -> &RepoLocation {
        &self.location
    }

    /// Estimated bytes of every cached [`RenderedDiff`] this workspace is
    /// holding right now — what the S7-3 `AppShell::WorkspaceCache` LRU
    /// budgets against (`pub(crate)` so `shell.rs` can read it without a
    /// getter round trip through a private field). Counts both `self.diffs`
    /// (the currently-open source's diffs) AND `self.pr_diff_cache` (Phase
    /// 7 D3's parked-PR diffs) — cross-cutting risk F: leaving either out
    /// would silently undercount the budget a workspace this size actually
    /// costs.
    pub(crate) fn estimated_diff_bytes(&self) -> usize {
        let live: usize = self.diffs.values().map(|d| d.estimated_bytes()).sum();
        let pr_cached: usize = self
            .pr_diff_cache
            .values()
            .flat_map(|entry| entry.diffs.values())
            .map(|d| d.estimated_bytes())
            .sum();
        live + pr_cached
    }

    /// Count of `self.diffs` entries that are a genuine rendered diff, NOT
    /// an [`error_diff`] placeholder (inserted when a per-file diff compute
    /// fails, e.g. a host connection drop mid-`request_diff` — see the
    /// `Err` arm that populates `self.diffs`). Backs both `Self::
    /// render_header`'s `· n/N files` qualifier and `Self::automation_
    /// state`'s `diffstat.files_loaded`: counting an errored placeholder as
    /// "loaded" would let `files_loaded == files_total` once every file has
    /// merely been *visited*, even though one contributed nothing to `+N`/
    /// `−N` (an `error_diff` has no `Row::Line`s) — silently hiding the
    /// partial-total qualifier `Self::diffstat`'s own doc comment exists to
    /// keep honest (review finding P3-1).
    fn files_loaded(&self) -> usize {
        self.diffs.values().filter(|d| !d.error).count()
    }

    /// Added/removed line totals for the title-bar-anatomy header's `+N`/
    /// `−N` (R1c title-bar spec). **Deliberate scope
    /// limit**: this sums `Row::Line` kinds over whatever `self.diffs`
    /// already holds — the files diffed so far this session — NOT a true
    /// whole-review total. dv's per-file diffs load lazily (Phase 7;
    /// unlike a design that diffs every file eagerly at PR-load time
    /// and folds `additions`/`deletions` once), and a
    /// real whole-review number needs either eagerly diffing every file
    /// (reintroducing exactly the cold-open cost Phase 7 removed) or a
    /// cheap dv-core `--numstat`-style computation cached at index-hydration
    /// time — which is precisely the scope the restyle's phasing
    /// already carves out as R2 item 3 ("Sidebar total +/− diffstat per
    /// review card", an `IndexEntry` change) and R1c's own files_touched
    /// excludes dv-core. So: exact once every file has been visited (the
    /// common case for the small screenshot fixtures this slice verifies
    /// against), a partial running total otherwise — `Self::render_header`
    /// only shows it once `self.diffs` is non-empty, and
    /// `Self::automation_state`'s `diffstat.files_loaded` vs `files_total`
    /// makes the partial-vs-complete distinction assertable rather than
    /// silently misleading.
    fn diffstat(&self) -> (u32, u32) {
        if let Some(cached) = self.diffstat_cache.get() {
            return cached;
        }
        let mut added = 0u32;
        let mut removed = 0u32;
        for diff in self.diffs.values() {
            for row in &diff.unified {
                match row {
                    Row::Line {
                        kind: LineKind::Added,
                        ..
                    } => added += 1,
                    Row::Line {
                        kind: LineKind::Removed,
                        ..
                    } => removed += 1,
                    _ => {}
                }
            }
        }
        self.diffstat_cache.set(Some((added, removed)));
        (added, removed)
    }

    /// Evict least-recently-used `pr_diff_cache` entries until at most
    /// [`MAX_PR_DIFF_ENTRIES`] remain (Phase 7 D3) — the count cap this
    /// small cache uses instead of a byte budget of its own, since its
    /// bytes are already folded into `estimated_diff_bytes` and bounded by
    /// the S7-3 workspace-level ceiling.
    fn evict_pr_diff_cache(&mut self) {
        while self.pr_diff_lru.len() > MAX_PR_DIFF_ENTRIES {
            let Some(key) = self.pr_diff_lru.pop_back() else {
                break;
            };
            self.pr_diff_cache.remove(&key);
        }
    }

    /// Applies a freshly `pick_review`d snapshot to `this.review`,
    /// reconciling `pinned_review_id`/`pr_remote`/`remote_threads` and
    /// emitting `ReviewChanged` on a genuine change. Shared body for the
    /// store-watch consumer (`Self::new`, above), the worktree-watch
    /// consumer's sibling loop (which never calls this — it doesn't touch
    /// `this.review`), and [`Self::revalidate`]'s review half (review
    /// finding: these three used to fork this logic three ways; consolidated
    /// here so a future fix to the fingerprint/pr_remote reconciliation
    /// can't silently miss one call site).
    ///
    /// Monotonicity guard (review finding P3, S7-4): `review` is a snapshot
    /// read off-thread, so by completion time a *faster* concurrent writer
    /// — a GUI comment save, or another one of these three reload paths —
    /// may have already landed a newer version of the SAME review into
    /// `this.review`. Comparing only the `(id, updated_ms, comments.len())`
    /// fingerprint can't tell "genuinely different" apart from "an older
    /// snapshot of what's already current", so a same-id pick whose
    /// `updated_ms` is strictly older than what's already showing is
    /// discarded rather than applied — this pass must never move a review
    /// backwards in time. A different id (the pick genuinely landed on a
    /// different review, e.g. the pin fell through to another draft) always
    /// applies regardless of its `updated_ms`.
    ///
    /// Returns whether `this.review` actually changed — callers use this to
    /// decide whether to call `reset_diff_list`/`cx.notify()`, since
    /// [`Self::revalidate`] also has an independent worktree-half change to
    /// fold into the same repaint.
    fn apply_review_reload(
        &mut self,
        review: Option<dv_core::Review>,
        cx: &mut Context<Self>,
    ) -> bool {
        let stale_snapshot = matches!(
            (&self.review, &review),
            (Some(current), Some(picked))
                if current.id == picked.id && picked.updated_ms < current.updated_ms
        );
        if stale_snapshot {
            return false;
        }
        let fingerprint = |r: &Option<dv_core::Review>| {
            r.as_ref()
                .map(|r| (r.id.clone(), r.updated_ms, r.comments.len()))
        };
        if fingerprint(&self.review) == fingerprint(&review) {
            return false;
        }
        // `remote_threads` was fetched for whichever PR `pr_remote` pointed
        // at before this reload — key on (slug, pr number), the same
        // identity `refresh_remote_threads`'s own epoch check uses, to tell
        // whether that fetch still applies.
        let old_pr_key = self.pr_remote.as_ref().map(|r| (r.slug.clone(), r.pr));
        self.review = review;
        self.pinned_review_id = resolved_pin(self.pinned_review_id.take(), &self.review);
        // Keep `pr_remote` in sync with the review actually on screen — it's
        // the only place `own_submitted_review_id` (github-thread dedup +
        // resolved-sync) is read from, and a review reassignment here (e.g.
        // a CLI `dv review submit` picked up mid-session, or a reactivation
        // revalidation) is exactly as fresh a `remote` as the initial-load
        // path (review finding: this used to only happen at initial load /
        // `open_pr`, so an in-session submit left `pr_remote` permanently
        // stale).
        self.pr_remote = self.review.as_ref().and_then(|r| r.remote.clone());
        let new_pr_key = self.pr_remote.as_ref().map(|r| (r.slug.clone(), r.pr));
        if old_pr_key != new_pr_key {
            // The reload landed on a different PR (or none) than
            // `remote_threads` was fetched for — keeping it around would
            // misattribute a stale PR's read-only threads (including their
            // resolved badges) onto whatever review this reload landed on,
            // since the interleave in `reset_diff_list` matches purely by
            // (path, side, line) and isn't gated on which review is current
            // (review finding P1: this used to only get cleared in
            // `open_pr`'s success arm, so an external review change/delete
            // mid-session left `remote_threads` stale). A subsequent
            // `refresh_remote_threads` (explicit `RefreshBadges`, or the
            // next `open_pr`) repopulates it for whatever PR is actually
            // current; same-PR reloads (e.g. a CLI comment add) deliberately
            // keep the existing fetch rather than blanking the cards until
            // the next manual refresh.
            self.remote_threads.clear();
        }
        cx.emit(ReviewChanged);
        // A watcher-driven (or revalidation-driven) reload invalidates a
        // parked submit panel — it was built against the review as it stood
        // before this external change (review finding P1-3).
        self.cancel_submit_flow_if_parked(cx);
        true
    }

    /// Applies a freshly `changed_files`-listed file list to `this.files`,
    /// reconciling every index-keyed cache (`diffs`/`diff_pending`/
    /// `expanded`/`selected`) via [`reconcile_file_selection`] rather than a
    /// blind swap (cross-cutting risk E: `selected`/`diffs`/`diff_pending`/
    /// `expanded` are all keyed by INDEX into `this.files`, not by path — a
    /// naive replace either panics once the list has shrunk past the old
    /// selected index, or silently re-renders the wrong file's cached diff
    /// under a reused index once a new file sorts in earlier). Shared body
    /// for the worktree-watch consumer (`Self::new`, above) and
    /// [`Self::revalidate`]'s worktree half.
    fn apply_worktree_reload(&mut self, files: Vec<ChangedFile>, cx: &mut Context<Self>) {
        let old_files = std::mem::replace(&mut self.files, files);
        let (new_selected, needs_invalidation) =
            reconcile_file_selection(&old_files, &self.files, self.selected);
        if needs_invalidation {
            self.diffs.clear();
            self.diffstat_cache.set(None);
            self.diff_pending.clear();
            self.expanded.clear();
            self.pending_jump = None;
            match new_selected {
                Some(index) => {
                    // Same file as before, just relocated — preserve
                    // `current_hunk` and the diff list's scroll position
                    // (`reset_diff_list`, called by the caller, keeps the
                    // current offset).
                    self.selected = Some(index);
                    self.request_diff(index, cx);
                }
                None if !self.files.is_empty() => {
                    // The previously selected file is gone (or nothing was
                    // selected yet) — this is genuinely a different file, so
                    // reset hunk navigation same as a normal `select_file`.
                    self.selected = Some(0);
                    self.current_hunk = 0;
                    self.request_diff(0, cx);
                }
                None => {
                    // Nothing left to show — clear cleanly rather than leave
                    // a dangling index (the original crash: `self.files[2]`
                    // after an external commit emptied the list).
                    self.selected = None;
                }
            }
        }
    }

    /// One-shot revalidation of this workspace against its store + working
    /// tree, dispatched by `AppShell::revalidate_active` right after the
    /// `Self::open_review` cache-hit branch reactivates a parked entry
    /// (Phase 7 D1b — the stale-while-revalidate half of Deliverable 1 that
    /// S7-3 deliberately left as a no-op stub rather than land prematurely).
    /// A consolidation of the store-watch and worktree-watch consumer
    /// BODIES ([`Self::new`]'s two `cx.spawn` loops, above) as an on-demand
    /// pass rather than a new signal source: re-pick the review from a
    /// fresh store list (same as the store watcher), and — for a
    /// `WorkingTree` source only, since a PR/range/commit file list is
    /// fixed by its endpoints — re-list `changed_files` and reconcile
    /// index-keyed caches via [`reconcile_file_selection`] (same as the
    /// worktree watcher), forcing a staleness recheck either way. This is
    /// the case no parked watcher covers on its own: the review-store
    /// watcher only observes `.git/dv`, never the working tree, so a Local
    /// repo's working-tree edit made while parked is otherwise invisible
    /// until some unrelated trigger forces a rebuild.
    ///
    /// Calling the exact same helpers the two watch consumers call (rather
    /// than forking the logic) is also what makes racing the parked
    /// entity's still-live watcher for the same change safe (cross-cutting
    /// risk C): both funnel through the same fingerprint check and
    /// `reconcile_file_selection`, so whichever lands second is a no-op,
    /// never a double-apply.
    ///
    /// No-ops if a load or `open_pr` is already in flight
    /// (`Status::Loading` / `pr_loading.is_some()`) — there's nothing
    /// settled to revalidate against yet, and that in-flight completion
    /// already supersedes anything this pass could compute. Captures
    /// `source_epoch` up front and re-checks it on completion, discarding
    /// the result if it no longer matches — the user switched away again,
    /// or opened a PR, before this pass finished — same discard-if-
    /// superseded pattern `source_epoch`'s doc comment describes.
    ///
    /// Reactivation implies the repo (including a WSL distro) is live —
    /// the same reasoning `AppShell::open_review`'s non-WSL-gated
    /// `refresh_badge` call already relies on — so this may touch the
    /// host. It is invoked ONLY for the single reactivated entry; never
    /// sweep `AppShell::workspace_cache` in the background (cross-cutting
    /// risk D — that would boot a stopped WSL distro just for sitting in
    /// the cache).
    pub(crate) fn revalidate(&mut self, cx: &mut Context<Self>) {
        if matches!(self.status, Status::Loading) || self.pr_loading.is_some() {
            return;
        }
        // Rebake the selected file if its cached diff was baked under a
        // since-changed theme/context while this entity was parked without a
        // live host (`invalidate_diff_cache`'s non-eager path couldn't
        // recompute then). Reactivation implies the repo is live now, so this
        // is a normal recompute with the current theme's colors (capstone P2).
        if self.diffs_theme_stale {
            self.diffs_theme_stale = false;
            self.diffs.clear();
            self.diffstat_cache.set(None);
            self.diff_pending.clear();
            if let Some(index) = self.selected {
                self.request_diff(index, cx);
            }
            cx.notify();
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let location = self.location.clone();
        let source = self.source.clone();
        let is_working_tree = matches!(source, DiffSource::WorkingTree);
        let current_id = self.review.as_ref().map(|r| r.id.clone());
        let current_pr = self.pr_remote.clone();
        let pinned_id = self.pinned_review_id.clone();
        let epoch = self.source_epoch;
        // Snapshot the review as it stands NOW; if a concurrent reload (the
        // still-live store-watch loop, or a GUI/CLI write) moves this.review
        // — including to a DIFFERENT id — before this pass's off-thread
        // pick_review completes, that reload read the store more recently, so
        // our result is stale and its review half must be discarded (capstone
        // P3: apply_review_reload's same-id monotonicity guard can't catch an
        // id change).
        let review_fp = self
            .review
            .as_ref()
            .map(|r| (r.id.clone(), r.updated_ms, r.comments.len()));

        cx.spawn(async move |this, cx| {
            let (review, files, worktree_head_oid) = cx
                .background_executor()
                .spawn(async move {
                    let review = pick_review(
                        dv_core::ReviewStore::open(location)
                            .list()
                            .unwrap_or_default(),
                        current_id.as_deref(),
                        current_pr.as_ref(),
                        pinned_id.as_deref(),
                    );
                    let files = is_working_tree.then(|| repo.changed_files(&source));
                    // Refreshed unconditionally (not just for a `WorkingTree`
                    // source) — this is the cache `Self::lsp_view_is_honest`
                    // compares a `DiffSource::Range`'s `head` against, and
                    // reactivation is exactly the "git round trips already
                    // happen off-thread" moment to keep it current.
                    let worktree_head_oid = repo.resolve("HEAD").ok();
                    (review, files, worktree_head_oid)
                })
                .await;

            let alive = this.update(cx, |this, cx| {
                // Superseded — see doc comment above.
                if this.source_epoch != epoch {
                    return;
                }

                // ---- review half: `apply_review_reload` is the exact same
                // body the store-watch consumer (`Self::new`, above) calls,
                // including its `pr_remote`/`remote_threads` reconciliation
                // and the monotonicity guard against a faster concurrent
                // writer (review finding P3: this pass's `pick_review` read
                // is a snapshot that can complete after a newer write, e.g.
                // a GUI comment save, has already landed). ----
                // Discard the review half entirely if a concurrent reload
                // moved this.review since dispatch (capstone P3) — the
                // worktree half below is independent and still runs.
                let cur_fp = this
                    .review
                    .as_ref()
                    .map(|r| (r.id.clone(), r.updated_ms, r.comments.len()));
                let review_changed = if cur_fp == review_fp {
                    this.apply_review_reload(review, cx)
                } else {
                    false
                };

                if worktree_head_oid.is_some() {
                    this.worktree_head_oid = worktree_head_oid;
                }

                // ---- worktree half: `apply_worktree_reload` is the exact
                // same body the worktree-watch consumer (`Self::new`, above)
                // calls, including the unconditional staleness recheck below
                // (a worktree edit can drift a comment's anchor without the
                // file list itself changing). Only for a `WorkingTree`
                // source that's STILL current — `this.source` may have moved
                // on since this pass was dispatched even with a matching
                // epoch (review finding P2-1's exact reasoning, quoted in
                // the worktree consumer above). ----
                let mut worktree_pass = false;
                if matches!(this.source, DiffSource::WorkingTree)
                    && let Some(files) = files
                {
                    worktree_pass = true;
                    if let Ok(files) = files {
                        this.apply_worktree_reload(files, cx);
                    }
                    this.stale_checked = None;
                }

                if review_changed || worktree_pass {
                    this.reset_diff_list(cx);
                    cx.notify();
                }
            });
            alive.ok();
        })
        .detach();
    }

    /// Whether this workspace's own PR picker overlay is currently open.
    /// `pub(crate)` so `AppShell::on_open_theme_picker`/`on_open_settings`
    /// can decline opening a shell-level overlay on top of it (review
    /// finding P2: `on_open_pr_picker`'s own guard against the shell
    /// overlays — see its comment above — was one-directional; ctrl-shift-t
    /// / ctrl-, could still stack a shell overlay over an already-open PR
    /// picker, stranding it visually and, for the settings panel's modal
    /// backdrop, blocking it entirely).
    pub(crate) fn pr_picker_open(&self) -> bool {
        self.pr_picker.is_some()
    }

    /// Whether the active review is submitted — suppresses mutation of its
    /// EXISTING threads (docs/phase-6-review-navigator.md S6c doc-deviation
    /// #4: the thread card's reply/edit/resolve/delete) plus shows a banner
    /// in the summary panel ([`Self::render_summary`]). Deliberately
    /// narrower than a blanket lockdown — scroll/nav/the verdict caption
    /// stay live, matching the deviation's stated scope. `false` while no
    /// review has loaded yet (nothing to gate).
    ///
    /// Does NOT by itself gate *new* top-level comments — see
    /// [`Self::new_comment_blocked`], which is deliberately narrower still.
    fn review_is_readonly(&self) -> bool {
        self.review
            .as_ref()
            .is_some_and(|r| matches!(r.state, dv_core::ReviewState::Submitted { .. }))
    }

    /// Whether starting a NEW top-level comment (gutter selection → editor)
    /// is currently blocked. Narrower than [`Self::review_is_readonly`] on
    /// purpose: only an EXPLICITLY pinned submitted review (a sidebar-row
    /// pick or `select_review`, S6c) blocks new comments outright. A
    /// merely auto-selected submitted review — the newest review with no
    /// PR open, or the review `submit_review` just finished in-app with no
    /// pin — still accepts a new gutter comment, which lands in a fresh
    /// draft (`submit_comment`'s Submitted-state branch); that's the
    /// "continue reviewing" flow `pick_review`'s and `submit_review`'s own
    /// doc comments promise. Existing-thread mutation has no such
    /// exception — there's no "start a fresh draft" fallback for editing a
    /// comment that already belongs to a specific (now-submitted) review,
    /// so those stay gated on plain `review_is_readonly()` everywhere else.
    fn new_comment_blocked(&self) -> bool {
        self.pinned_review_id.is_some() && self.review_is_readonly()
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
        // Any successful selection — a plain sidebar click, an in-place
        // summary jump, or the landing select_file of a source switch —
        // supersedes a stale "Jump: ..." banner from an earlier failed
        // switch_source_and_jump (P3 finding: it used to persist across
        // unrelated successful navigation).
        self.source_switch_error = None;
        // A go-to-definition status banner belongs to the file/position it
        // was raised on — carrying it across a file switch would leave a
        // stale "code intelligence unavailable"/"no definition found" up
        // for a click that never happened on the new file (P2 finding).
        self.lsp_status = None;
        // Invalidate any in-flight go-to-def round trip: its completion
        // (definition request or target-file read) must not reopen the
        // target-viewer modal for a click on a file the user has since
        // navigated away from (P3 finding — see `lsp_request_epoch`'s doc
        // comment).
        self.lsp_request_epoch += 1;
        // Same reasoning for the S8g hover popover: it belongs to a token on
        // the file being left, not the one about to be shown.
        self.hover_popover = None;
        self.hover_request_epoch += 1;
        self.hover_request_line = None;
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
        // Word-tint tier (R1a's `DvTheme`):
        // `word_created_bg`/`word_deleted_bg` are `success`/
        // `danger` @ 0.28 — a second, more saturated alpha tier over the
        // existing ~12.5% row tint (`Row::Line`'s own `bg` in
        // `render_diff_row`/`render_split_cell`, untouched by this slice —
        // the row-tint tokens needed no new work). This used to be a bare `.opacity(0.32)` computed inline
        // here; routing it through `DvTheme` means a future per-theme `"dv"`
        // override to these hues is honored automatically.
        let dv = crate::themes::dv_theme(cx);
        let hl = HighlightInputs {
            theme: theme.highlight_theme.clone(),
            intra_added: dv.word_created_bg,
            intra_removed: dv.word_deleted_bg,
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
                        this.diffstat_cache.set(None);
                    }
                    Err(err) => {
                        let msg: SharedString = format!("failed to compute diff: {err:#}").into();
                        this.diffs.insert(index, Arc::new(error_diff(msg)));
                        this.diffstat_cache.set(None);
                    }
                }
                // Row indices may have shifted (gap expansion inserts rows
                // above); re-sync the list and re-anchor the viewport on
                // the current hunk so the content doesn't visually jump.
                if this.selected == Some(index) {
                    let jumped = this.reset_diff_list(cx);
                    if !jumped {
                        this.scroll_to_current_hunk(cx);
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
    /// `eager` distinguishes the ACTIVE workspace (always `true`) from a
    /// PARKED fan-out entry (always `false`) — see
    /// [`Self::invalidate_diff_cache`]'s doc comment for why the two must
    /// not share the same recompute policy.
    pub(crate) fn on_theme_changed(&mut self, eager: bool, cx: &mut Context<Self>) {
        self.invalidate_diff_cache(eager, cx);
    }

    /// Drop every cached [`RenderedDiff`] and recompute the currently
    /// selected file — shared by [`Self::on_theme_changed`] (colors baked
    /// stale) and [`Self::set_context_lines`] (hunk structure itself
    /// changed, so the cache is stale in a much more literal sense). Any
    /// other (currently unselected) file simply recomputes lazily under the
    /// new settings the next time it's picked, same as a first-ever view.
    ///
    /// This also runs for PARKED workspaces (`AppShell::apply_resolved_theme`/
    /// `set_context_lines` fan out to every cached entry, not just
    /// `self.active` — see those methods' doc comments), and the two cases
    /// need different policies:
    ///
    /// - `eager == true` (the ACTIVE workspace, on screen right now) always
    ///   clears and recomputes unconditionally, exactly as before this
    ///   cache existed. `request_diff` already falls back safely to a
    ///   per-command `wsl.exe -d <distro>` when there's no live host
    ///   connection — that's the same fallback every other selection-driven
    ///   diff load uses — so there is no boot-storm risk here: the user is
    ///   actively looking at this repo, so its distro is already live in
    ///   practice (review finding: gating this on `has_running_host` left
    ///   the ACTIVE diff pane silently blank/stale whenever the distro had
    ///   no *host* connection, even though ordinary git access worked fine).
    /// - `eager == false` (a parked fan-out entry nobody is looking at)
    ///   gates the recompute on the repo actually being reachable without
    ///   side effects: for a `RepoLocation::Wsl` whose distro has no live
    ///   host connection, `request_diff` would shell `wsl.exe -d <distro>`,
    ///   which boots a stopped distro as a side effect of a routine
    ///   theme/context-lines change on a workspace nobody is even looking at
    ///   (cross-cutting risk D — the same boot-storm hazard
    ///   `AppShell::refresh_all_badges` guards against via
    ///   `has_running_host`). When the host isn't reachable, the existing
    ///   cache is left in place (stale bake/hunks) rather than cleared with
    ///   nothing to replace it — a cache-hit reactivation
    ///   (`AppShell::open_review`'s pinned-key fast path) never re-selects
    ///   or otherwise retries the request, so clearing here without
    ///   recomputing would leave the pane permanently blank until the user
    ///   manually reselects the file (review finding). The stale-but-cached
    ///   rows get a real recompute the next time this entity is genuinely
    ///   reselected, same as any workspace that's simply never been
    ///   switched to since the theme/context change.
    fn invalidate_diff_cache(&mut self, eager: bool, cx: &mut Context<Self>) {
        self.highlight_epoch += 1;
        // Content-baked PR diffs (Phase 7 D3) are colors/hunk-structure just
        // like `self.diffs` — a theme/context-lines change stales them the
        // same way, so drop them unconditionally, on BOTH the eager and
        // non-eager (host-unreachable) paths below. This is cheap (no git/
        // recompute work, just dropping cached rows) and is the single
        // choke point that keeps a reopened PR from ever painting under a
        // since-changed theme (see `pr_diff_cache`'s doc comment).
        self.pr_diff_cache.clear();
        self.pr_diff_lru.clear();
        let host_reachable = eager
            || match &self.location {
                RepoLocation::Wsl { distro, .. } => {
                    dv_core::remote::manager::has_running_host(distro)
                }
                RepoLocation::Local(_) => true,
            };
        if host_reachable {
            self.diffs.clear();
            self.diffstat_cache.set(None);
            self.diff_pending.clear();
            self.diffs_theme_stale = false;
            if let Some(index) = self.selected {
                self.request_diff(index, cx);
            }
        } else {
            // Parked WSL entry with no live host: recomputing here would boot
            // the stopped distro (cross-cutting risk D), and clearing without
            // recomputing would blank the pane on reactivation (a cache-hit
            // reactivation re-selects nothing). So keep the now-stale-theme
            // rows and flag them — `revalidate` rebakes the selected file on
            // reactivation, when the host is live again (capstone P2).
            self.diffs_theme_stale = true;
        }
        cx.notify();
    }

    /// Settings-panel/`set_setting` live update for "Context lines" — see
    /// `settings::Settings::context_lines`. A no-op when unchanged, so a
    /// stepper click that hits a clamp boundary doesn't pay for a recompute.
    /// `eager` — see [`Self::on_theme_changed`]/[`Self::invalidate_diff_cache`].
    pub(crate) fn set_context_lines(
        &mut self,
        context_lines: u32,
        eager: bool,
        cx: &mut Context<Self>,
    ) {
        if self.context_lines == context_lines {
            return;
        }
        self.context_lines = context_lines;
        self.invalidate_diff_cache(eager, cx);
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
    #[cfg(feature = "automation")]
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
        self.scroll_to_current_hunk(cx);
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
        // An explicit PR open is itself an explicit selection — any earlier
        // sidebar-row pin (docs/phase-6-review-navigator.md S6c) no longer
        // applies once the source is moving to a different review entirely.
        self.pinned_review_id = None;

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

        // Phase 7 D3: capture the OUTGOING PR's resolved diff now — before
        // `source`/`files`/`diffs` get replaced below — so it CAN be
        // stashed into `pr_diff_cache`, but don't commit it yet. Guarded by
        // `pr_source_key` returning `Some`: only a `DiffSource::Range` (i.e.
        // an already-open PR) has anything worth content-addressing; a
        // `WorkingTree`/`Staged`/`Commit` source has no `(merge_base,
        // head_oid)` pair to key under. An empty `self.diffs` (nothing
        // selected yet) isn't worth caching either.
        //
        // Committing this into `pr_diff_cache` is deferred to the
        // completion's current-epoch success arm (mirrors `last_pr_open_ms`,
        // which likewise only stamps there) rather than done eagerly here —
        // two review findings against an eager entry-time insert: (a) a
        // failed fetch left this exact content doubly counted — once live in
        // `self.diffs` (untouched by the `Err` arm), once redundantly
        // stashed here — permanently inflating `estimated_diff_bytes` until
        // some later navigation overwrote the key; (b) the entry-time
        // insert's own eviction could pop the very `(merge_base, head_oid)`
        // key THIS SAME call is about to look up (a 4-distinct-PR toggle
        // pattern with `MAX_PR_DIFF_ENTRIES` full), turning an intended warm
        // reopen into a forced cold recompute. Deferring the commit to after
        // the target-key lookup in the success arm fixes both: nothing is
        // stashed on failure, and the lookup always runs before this
        // insert's eviction can touch the cache.
        //
        // Also captures `highlight_epoch` alongside the stash (review
        // finding, P2): a theme/context-lines change firing while this
        // fetch is in flight bumps `highlight_epoch` and, via
        // `invalidate_diff_cache`, wholesale-clears `pr_diff_cache` right
        // out from under us — but `self.diffs` was already snapshotted here
        // and would otherwise get unconditionally re-inserted by the
        // completion below, re-poisoning the cache with rows baked under
        // the old theme/context. Mirrors `request_diff`'s
        // `highlight_epoch`-capture-and-recheck pattern.
        let captured_highlight_epoch = self.highlight_epoch;
        let outgoing_pr_stash = pr_source_key(&self.source)
            .filter(|_| !self.diffs.is_empty())
            .map(|key| {
                (
                    key,
                    CachedPrDiff {
                        files: self.files.clone(),
                        diffs: self.diffs.clone(),
                        expanded: self.expanded.clone(),
                    },
                )
            });

        // Closing the picker here (rather than deferred to the completion
        // below, like the rest of the teardown) is still fine: it holds no
        // user data, and leaving it open over the "Loading" pane would
        // just look broken.
        if self.pr_picker.take().is_some() {
            window.focus(&self.focus_handle, cx);
        }

        self.pr_error = None;
        // A leftover "Jump: ..." banner from an earlier failed
        // switch_source_and_jump would otherwise outlive this unrelated,
        // successful source switch (P3 finding: it was cleared only at the
        // top of switch_source_and_jump itself, never by open_pr).
        self.source_switch_error = None;
        self.pr_loading = Some(number);
        self.status = Status::Loading;
        // Bumped before the fetch even starts, so any request_diff already
        // in flight (and this open_pr's own completion, below) can tell a
        // superseding open_pr apart from itself (see the field's doc
        // comment).
        self.source_epoch += 1;
        let epoch = self.source_epoch;
        cx.notify();

        // Phase 7 D4: dispatch-to-`Status::Ready` timing (mirrors
        // `last_diff_ms`/`last_pr_list_ms`) — this is `load_pr`'s latency
        // only, identical on a `pr_diff_cache` hit or miss (see
        // `last_pr_open_ms`'s field doc); it is NOT the S7-5 warm/cold
        // signal, that's `last_pr_open_cache_hit`.
        let started = std::time::Instant::now();
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
                // Current-epoch completion, success or error either way —
                // both set `Status::Ready` below, and a superseded call
                // already returned above without reaching here.
                this.last_pr_open_ms = Some(started.elapsed().as_millis() as u64);
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
                        // Phase 7 D3: a `pr_diff_cache` hit on THIS PR's
                        // `(merge_base, head_oid)` reuses its resolved file
                        // list + rendered diffs wholesale instead of the
                        // freshly-fetched `files` above — skipping the
                        // per-file tree-sitter `compute_diff` recompute
                        // that already happened the last time this exact
                        // content was open. `files` and `diffs` are
                        // restored TOGETHER from the same cache entry
                        // (cross-cutting risk E: `diffs` is keyed by index
                        // into `files`, so the two must never come from
                        // different sources) — content-addressing
                        // guarantees the cached pair is identical to what a
                        // fresh fetch would produce anyway. `pr_meta`
                        // (`this.pr`, set below) is never cached and is
                        // always this call's freshly-fetched value, so a
                        // diff-cache hit still shows fresh state/decision/
                        // CI.
                        let key = pr_source_key(&this.source);
                        let cache_hit = key.as_ref().and_then(|k| this.pr_diff_cache.remove(k));
                        if let Some(hit) = cache_hit {
                            if let Some(k) = &key {
                                this.pr_diff_lru.retain(|lk| lk != k);
                            }
                            this.files = hit.files;
                            this.diffs = hit.diffs;
                            this.diffstat_cache.set(None);
                            // `expanded` is a render input baked into the
                            // restored `diffs` (which hunk-gaps are open) —
                            // restore it alongside files/diffs rather than
                            // clearing it, or the map would drift out of
                            // sync with what's on screen and silently
                            // collapse this gap the next time a different
                            // one is expanded (P3 finding).
                            this.expanded = hit.expanded;
                            this.last_pr_open_cache_hit = Some(true);
                        } else {
                            this.files = files;
                            // The old file list's diffs are keyed by index
                            // into a now-replaced list — stale caches would
                            // render the wrong file's content under the
                            // right name.
                            this.diffs.clear();
                            this.diffstat_cache.set(None);
                            this.expanded.clear();
                            this.last_pr_open_cache_hit = Some(false);
                        }
                        // Now that the fetch actually succeeded, commit the
                        // OUTGOING PR's diff (captured at entry, above) into
                        // `pr_diff_cache` — after, not before, the
                        // target-key lookup right above, so this insert's
                        // own eviction can never pop the entry that lookup
                        // just served (see `outgoing_pr_stash`'s doc
                        // comment). Skipped if the outgoing key is the same
                        // as the target key (reopening the PR already on
                        // screen) — there's nothing meaningfully "outgoing"
                        // in that case, and stashing it would just overwrite
                        // whatever the lookup above already resolved. Also
                        // skipped if `highlight_epoch` moved since entry
                        // (review finding, P2): a theme/context-lines change
                        // mid-fetch already cleared `pr_diff_cache` via
                        // `invalidate_diff_cache`, and committing this
                        // old-theme-baked snapshot now would silently
                        // re-poison it right after that clear.
                        if let Some((out_key, out_entry)) = outgoing_pr_stash
                            && Some(&out_key) != key.as_ref()
                            && this.highlight_epoch == captured_highlight_epoch
                        {
                            this.pr_diff_cache.insert(out_key.clone(), out_entry);
                            this.pr_diff_lru.retain(|k| k != &out_key);
                            this.pr_diff_lru.push_front(out_key);
                            this.evict_pr_diff_cache();
                        }
                        this.diff_pending.clear();
                        this.stale.clear();
                        this.stale_checked = None;
                        this.selected = None;
                        this.pending_jump = None;
                        this.pr_remote = review.remote.clone();
                        // Belongs to the PR being left behind (if any) —
                        // keeping it around would flash the old PR's
                        // read-only threads under the new PR's files until
                        // `refresh_remote_threads`'s fetch lands (docs/
                        // phase-6-review-navigator.md deliverable 6).
                        this.remote_threads.clear();
                        this.review = Some(review);
                        cx.emit(ReviewChanged);
                        this.pr = Some(meta.into());
                        this.pr_details_open = false;
                        this.status = Status::Ready;
                        this.refresh_remote_threads(cx);
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
                        // included. `last_pr_open_cache_hit` does need
                        // resetting though (P3 finding): this attempt never
                        // reached the cache-check above, so leaving it at
                        // whatever the previous successful open recorded
                        // would misreport a failed, non-cached open as a
                        // stale hit/miss from an unrelated PR.
                        this.status = Status::Ready;
                        this.pr_error = Some(format!("{err:#}"));
                        this.last_pr_open_cache_hit = None;
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

    /// Fetch this workspace's PR-linked review's GitHub-side review threads
    /// off-thread (docs/phase-6-review-navigator.md deliverable 6) —
    /// resolved state, author/body, and which submitted review (if any)
    /// opened each thread. A no-op that clears any stale threads with no PR
    /// linked (`self.pr_remote` is `None`) — nothing to fetch, nothing to
    /// show. Called only from `open_pr`'s success arm and the sidebar's
    /// `RefreshBadges` gesture (cross-cutting risk B: never a startup/index
    /// walk — this is a network call per PR, gated to an explicit PR-open
    /// or an explicit "go sync with GitHub" action only; `gh` always runs
    /// host-side via `GithubClient`, so this never touches `dv-host`/
    /// `crates/host` even for a WSL-located repo). Epoch-guarded the same
    /// way `refresh_pr` is — a source switch (or a newer PR open) mid-fetch
    /// discards the result rather than clobbering whatever replaced it.
    pub(crate) fn refresh_remote_threads(&mut self, cx: &mut Context<Self>) {
        let Some(remote) = self.pr_remote.clone() else {
            if !self.remote_threads.is_empty() {
                self.remote_threads.clear();
                self.reset_diff_list(cx);
            }
            return;
        };
        let mut parts = remote.slug.splitn(3, '/');
        let (Some(host), Some(owner), Some(repo)) = (parts.next(), parts.next(), parts.next())
        else {
            return;
        };
        if host.is_empty() || owner.is_empty() || repo.is_empty() {
            return;
        }
        let slug = RepoSlug {
            host: host.to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
        };
        let pr = remote.pr;
        let epoch = self.source_epoch;
        cx.spawn(async move |this, cx| {
            let result: Result<Vec<RemoteThread>, dv_core::GhError> = cx
                .background_executor()
                .spawn(async move {
                    let client = GithubClient::for_slug(slug)?;
                    client.pr_review_threads(pr)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.source_epoch != epoch
                    || this.pr_remote.as_ref().map(|r| (r.slug.clone(), r.pr))
                        != Some((remote.slug.clone(), remote.pr))
                {
                    // A newer source switch (or PR open) superseded this
                    // fetch — discard rather than stamp a stale fetch's
                    // threads over whatever's showing now (cross-cutting
                    // risk C).
                    return;
                }
                // Silent on failure (gh missing, unauthenticated, network
                // down, rate-limited, ...) — matches `fetch_pr_badge`'s
                // posture: a PR with no fetched threads is a perfectly
                // good fallback for a read-only sync feature, not an error
                // the user needs to see.
                if let Ok(threads) = result {
                    this.remote_threads = threads;
                    this.reset_diff_list(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- PR picker -----------------------------------------------------

    fn on_open_pr_picker(&mut self, _: &OpenPrPicker, window: &mut Window, cx: &mut Context<Self>) {
        if self.pr_picker.is_some() {
            return;
        }
        // Decline while the comment editor or a thread-reply input is open
        // (review finding P3, hardening the same class of hole this
        // function's `overlay_open` guard below already closes for the
        // shell overlays): the macOS menu bar (S8i) dispatches `OpenPrPicker`
        // straight to this handler with no key-context gate at all, so a
        // `Go > Open PR...` click while a comment is mid-edit would stack
        // the picker over the editor with both key contexts live.
        if self.editor.is_some() || self.thread_input.is_some() {
            return;
        }
        // Decline while the S8f target viewer is open (phase-8 capstone
        // review, P3): the `browse` key-context predicate excludes
        // `TargetViewerOpen` for exactly this reason, but macOS menu
        // dispatch bypasses key contexts entirely (same gap this fn's
        // other guards above/below already exist to close for the PR
        // picker/editor/shell overlays) — without this, `Go > Open PR...`
        // opens the picker BEHIND the viewer's occluding backdrop (the
        // viewer renders after the picker in `render`'s child order),
        // invisibly stealing focus into its search input.
        if self.target_viewer.is_some() {
            return;
        }
        // Decline while a shell-level overlay (theme picker / settings
        // panel) is genuinely open, the same way `on_open_theme_picker`
        // declines if `settings_panel` is already up (review finding P1).
        // Ask `AppShell` directly via `overlay_open` rather than inferring
        // it from focus location (review finding P3): ordinary
        // sidebar-chrome clicks — a group header, the "REVIEWS" label,
        // empty padding, none of which carry their own `track_focus` —
        // also bubble focus onto the shell handle with no overlay open at
        // all, so `shell_focus_handle.is_focused()` was declining
        // legitimate mouse-triggered opens too. Without this guard, a
        // mouse click on the "PRs · ctrl-g" hint button (which calls this
        // as a plain method, bypassing the `browse` key-context gate a
        // keystroke would need) could stack the PR picker on top of an
        // already-open theme picker; closing the PR picker afterwards
        // would then strand the theme picker keyboard-trapped behind it,
        // since this workspace's own `escape` bindings outrank the
        // shell's once focus is back on this handle.
        if self
            .shell
            .upgrade()
            .is_some_and(|shell| shell.read(cx).overlay_open())
        {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        // The sidebar filter popover is a third shell-level overlay that
        // doesn't go through the `overlay_open` guard above (it's
        // mouse-only and never moves focus, so it can still be open here)
        // — close it so its full-window backdrop can't end up painted on
        // top of the picker (review finding: the two overlays stacking
        // swallows the picker's first click). Mirrors
        // `on_open_theme_picker`/`on_open_settings`'s matching guard.
        if let Some(shell) = self.shell.upgrade() {
            shell.update(cx, |shell, cx| shell.close_filter_popover(cx));
        }
        // Supersede any in-flight go-to-definition round trip (whole-phase
        // capstone review, P3): without this, a click stashed while
        // `lsp_session` is `Spawning` could still complete and commit the
        // target viewer AFTER the picker opens, painting its occluding
        // backdrop over the picker exactly the way the reverse direction
        // (opening the picker/palette/editor while the viewer is up) is
        // already guarded against below/in `on_jump_to_file`/`open_editor`/
        // `open_thread_input`. See `lsp_request_epoch`'s doc comment.
        self.lsp_request_epoch += 1;
        self.pr_picker_epoch += 1;
        let generation = self.pr_picker_epoch;
        let t0 = std::time::Instant::now();
        // Phase 7 D2: paint from `pr_list_cache` instantly instead of a
        // spinner if this workspace already has a list on hand — there is
        // no TTL gate here (always revalidate below, regardless of age);
        // the cache only decides what's painted for the FIRST frame.
        self.pr_picker = match self.pr_list_cache.clone() {
            Some((prs, fetched_at)) => Some(PrPicker {
                state: PrPickerState::Loaded(prs),
                selected: 0,
                generation,
                fetched_at: Some(fetched_at),
                refreshing: true,
                refresh_error: None,
            }),
            None => Some(PrPicker {
                state: PrPickerState::Loading,
                selected: 0,
                generation,
                fetched_at: None,
                refreshing: false,
                refresh_error: None,
            }),
        };
        // A warm open never shows `Loading` — it's `Loaded` in the same
        // frame it's constructed above — so there's no "leaving Loading"
        // moment for the completion below to time. Stamp `last_pr_list_ms`
        // right here instead: dispatch-to-list-rendered for a warm open is
        // this synchronous paint, and it should read near-zero (the whole
        // point of D2 — no spinner frame). A cold (uncached) open leaves
        // this untouched; its `last_pr_list_ms` is stamped by the
        // completion below once the list actually leaves `Loading`.
        if self
            .pr_picker
            .as_ref()
            .is_some_and(|p| p.fetched_at.is_some())
        {
            self.last_pr_list_ms = Some(t0.elapsed().as_millis() as u64);
        }
        // Capture focus onto the workspace's own handle while the picker is
        // open (Phase 7 D0 fix). `PrPickerChoose`'s Enter binding is scoped
        // `"Workspace && PrPickerOpen"` — that context is only on the
        // dispatch path if focus actually rests on this handle. This is the
        // mirror image of `on_open_theme_picker`/`on_open_settings` in
        // shell.rs, which capture the *shell's* handle because their
        // bindings are `"AppShell && ..."`; do not copy that pattern here.
        // Without this, a caller that opens the picker while focus rests
        // elsewhere (e.g. the mouse "PRs · ctrl-g" hint button, which calls
        // this as a plain method bypassing the browse-context gate) leaves
        // Enter a no-op — the regression this fixes. Mirrors
        // `close_pr_picker`, which already restores focus symmetrically.
        window.focus(&self.focus_handle, cx);
        cx.notify();

        // Always revalidate in the background — even on a warm paint — so
        // the list patches in place shortly after (stale-while-revalidate,
        // Phase 7 D2). Phase 7 D4: dispatch-to-settled timing for the COLD
        // path (mirrors `last_diff_ms`); captured here, not inside the
        // completion, so it covers the whole round trip including the
        // background hop.
        let started = std::time::Instant::now();
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
                // Apply and stamp only if this picker is still the one we
                // opened — checked by `generation`, not just presence,
                // because a closed-then-reopened picker (escape, then
                // `ctrl-g` again before this fetch lands) is also `Some`
                // and would otherwise look like a match. That reopened
                // picker has already started its own fresh fetch/timing;
                // this stale completion must not clobber its state or
                // stamp a bogus `last_pr_list_ms` over it.
                let is_current = this
                    .pr_picker
                    .as_ref()
                    .is_some_and(|p| p.generation == generation);
                // Warm `pr_list_cache` on ANY successful fetch, whether or
                // not the picker that triggered it is still open/current
                // (review finding: gating the cache write on `is_current`
                // silently discarded a successful fetch whenever the picker
                // was closed — e.g. `escape` or `open_pr`'s Enter, both of
                // which `take()` the picker — before the round trip
                // landed, defeating "always revalidate" for the single most
                // common interaction). Only guard against a genuinely
                // out-of-order completion (an older generation's fetch
                // landing after a newer one already wrote a fresher list)
                // via `pr_list_cache_generation`, never against picker
                // liveness.
                if let Ok(prs) = &result
                    && generation >= this.pr_list_cache_generation.unwrap_or(0)
                {
                    this.pr_list_cache = Some((prs.clone(), std::time::Instant::now()));
                    this.pr_list_cache_generation = Some(generation);
                }
                if is_current {
                    // Whether the picker was still showing its first-ever
                    // (uncached) `Loading` frame when this landed — only
                    // then does THIS completion own `last_pr_list_ms` (a
                    // warm open already stamped it synchronously above).
                    let was_cold_load = this
                        .pr_picker
                        .as_ref()
                        .is_some_and(|p| matches!(p.state, PrPickerState::Loading));
                    match result {
                        Ok(prs) => {
                            let now = std::time::Instant::now();
                            if let Some(picker) = &mut this.pr_picker {
                                // Preserve the user's navigation position
                                // across the in-place list patch (dropping
                                // the old unconditional `selected = 0`
                                // reset) by PR IDENTITY, not raw index — a
                                // revalidation that reorders or grows the
                                // list (e.g. a new PR opened upstream lands
                                // at the top) must not silently repoint the
                                // highlight at a different PR just because
                                // the index still fits. Re-find the
                                // previously-highlighted PR's number in the
                                // fresh list; if that PR is gone (closed/
                                // merged upstream since the cached list was
                                // shown), reset to the top rather than
                                // falling back to the same raw index, which
                                // would silently land on whatever unrelated
                                // PR shifted into that slot (review finding:
                                // a clamped-index fallback repoints the
                                // highlight without any visible jump, so
                                // Enter can open a PR the user never chose).
                                let prev_number = match &picker.state {
                                    PrPickerState::Loaded(old) => {
                                        old.get(picker.selected).map(|pr| pr.number)
                                    }
                                    _ => None,
                                };
                                picker.selected = match prev_number {
                                    Some(n) => {
                                        prs.iter().position(|pr| pr.number == n).unwrap_or(0)
                                    }
                                    None => 0,
                                };
                                picker.state = PrPickerState::Loaded(prs);
                                picker.fetched_at = Some(now);
                                picker.refreshing = false;
                                picker.refresh_error = None;
                            }
                        }
                        Err(err) => {
                            if let Some(picker) = &mut this.pr_picker {
                                if matches!(picker.state, PrPickerState::Loaded(_)) {
                                    // Stale-but-present beats correct-but-
                                    // empty (docs/phase-7-performance.md
                                    // deliverable 2): a revalidation over an
                                    // already-shown list keeps that list up
                                    // and just surfaces the failure as a
                                    // warning instead of blanking it.
                                    picker.refreshing = false;
                                    picker.refresh_error = Some(err);
                                } else {
                                    picker.state = PrPickerState::Error(err);
                                    picker.refreshing = false;
                                }
                            }
                        }
                    }
                    if was_cold_load {
                        this.last_pr_list_ms = Some(started.elapsed().as_millis() as u64);
                    }
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
    #[cfg(feature = "automation")]
    pub(crate) fn automation_state(&self) -> serde_json::Value {
        use serde_json::json;
        let status = match &self.status {
            Status::Loading => "loading".to_string(),
            Status::Ready => "ready".to_string(),
            Status::Failed(err) => format!("failed: {err}"),
        };
        let (diffstat_added, diffstat_removed) = self.diffstat();
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
            // Phase 7 D4 instrumentation: dispatch-to-settled wall time for
            // the PR-picker list fetch and the most recent `open_pr`.
            // `last_pr_list_ms` IS the S7-2 warm/cold signal (a warm picker
            // open skips the `Loading` frame entirely, so this stamps near-
            // instantly). `last_pr_open_ms` is NOT the S7-5 signal though —
            // it times `load_pr` only, which runs the same on a
            // `pr_diff_cache` hit or miss; use `last_pr_open_cache_hit`
            // below for that (see the field's doc comment).
            "last_pr_list_ms": self.last_pr_list_ms,
            "last_pr_open_ms": self.last_pr_open_ms,
            // Phase 7 D3: whether the most recent `open_pr` reused a cached
            // `(merge_base, head_oid)` diff (`pr_diff_cache` hit) instead of
            // recomputing — the S7-5 warm-reopen assertion.
            "last_pr_open_cache_hit": self.last_pr_open_cache_hit,
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
                // docs/phase-6-review-navigator.md S6c: whether comment
                // mutation (new/reply/edit/resolve/delete) is suppressed —
                // see `review_is_readonly`'s doc comment for the exact
                // scope (suppressed entry points + a banner, not a blanket
                // lockdown).
                "readonly": self.review_is_readonly(),
                // docs/phase-6-review-navigator.md deliverable 5: comment
                // ids anchored to a file that isn't in the currently open
                // `self.files` — the summary panel's off-diff marker, made
                // assertable without a screenshot.
                "off_diff": r.comments.iter()
                    .filter(|c| !self.files.iter().any(|f| f.path == c.path))
                    .map(|c| c.id.clone())
                    .collect::<Vec<_>>(),
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
            // R1c title-bar `+N`/`−N`:
            // `added`/`removed` are `Self::diffstat`'s running total over
            // `self.diffs` — see that method's doc comment for why this is
            // partial-until-`files_loaded == files_total`, not a true
            // whole-review number (that's R2 item 3, an `IndexEntry`
            // change). Exposed so a script can assert the header's rendered
            // digits against the exact numbers backing them, rather than
            // pixel-reading a screenshot. `files_loaded` uses `Self::
            // files_loaded` (excludes `error_diff` placeholders — review
            // finding P3-1) so a script asserting `files_loaded ==
            // files_total ⇒ exact` can't be fooled by a failed file that
            // was merely visited.
            "diffstat": {
                "added": diffstat_added,
                "removed": diffstat_removed,
                "files_loaded": self.files_loaded(),
                "files_total": self.files.len(),
            },
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
            // docs/phase-6-review-navigator.md deliverable 5:
            // `switch_source_and_jump`'s most recent failure, if any.
            "source_switch_error": self.source_switch_error,
            // docs/phase-6-review-navigator.md deliverable 6: read-only
            // GitHub-side threads fetched by `refresh_remote_threads` —
            // makes the interleaved rendering + resolved-state sync
            // assertable without a screenshot.
            "remote_threads": self.remote_threads.iter().map(|t| json!({
                "path": t.path,
                "line": t.line,
                "side": gh_side_word(t.side),
                "resolved": t.is_resolved,
                "comments": t.comments.len(),
                "review_database_id": t.review_database_id,
            })).collect::<Vec<_>>(),
            "pr_picker_open": self.pr_picker.is_some(),
            // Phase 7 D2: `refreshing`/`refresh_error`/`age_ms` make the
            // paint-from-cache-then-revalidate shape assertable without a
            // screenshot — a warm open is `loading: false` with
            // `refreshing: true` in the SAME frame it opens (no spinner),
            // and `age_ms` is how stale that instantly-painted list was.
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
                "refreshing": p.refreshing,
                "refresh_error": p.refresh_error,
                "age_ms": p.fetched_at.map(|at| at.elapsed().as_millis() as u64),
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
            // S8f (docs/phase-8-lsp-and-polish.md § LSP): go-to-definition
            // session/nav-stack/target-viewer state, so a script can assert
            // the degrade path (no node/vtsls → "unavailable", never a
            // crash) without a screenshot.
            "lsp": json!({
                "session": match &self.lsp_session {
                    crate::lsp::LspSessionState::Unattempted => "unattempted".to_string(),
                    crate::lsp::LspSessionState::Spawning => "spawning".to_string(),
                    crate::lsp::LspSessionState::Ready(_) => "ready".to_string(),
                    crate::lsp::LspSessionState::Unavailable(reason) => {
                        format!("unavailable: {reason}")
                    }
                },
                "status": self.lsp_status,
                "node_modules_warning": self.lsp_node_modules_warning,
                "nav_back": self.nav_stack.can_go_back(),
                "nav_forward": self.nav_stack.can_go_forward(),
                "target_viewer": self.target_viewer.as_ref().map(|v| json!({
                    "path": v.path,
                    "highlight_line": v.highlight_line,
                    "lines": v.lines.len(),
                })),
                // S8g: hover popover state, so a script can assert the
                // degrade path (old-side view / no session → never shown)
                // and the shown case (a non-empty `markdown`) without a
                // screenshot — see `Self::hover_popover_visible`'s doc
                // comment for why this reads that (not the raw field): a
                // stale hover answer sitting behind an open modal must
                // report as absent here too, matching what's on screen.
                "hover": self.hover_popover_visible().map(|h| json!({
                    "line": h.line,
                    "markdown": h.markdown,
                })),
            }),
        })
    }

    /// Bounds-checked selection for `--automation`: unlike the UI path
    /// (which silently ignores stale indices), scripts get a hard error so
    /// the response never lies about what happened.
    #[cfg(feature = "automation")]
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
    /// PR open settled (`open_pr` reuses `Status::Loading` for this), an
    /// open PR picker's `gh pr list` fetch finished loading, and no S8f
    /// go-to-definition work outstanding (phase-8 capstone integration
    /// review, P3 — added on top of the original set above, which predates
    /// LSP entirely): a vtsls session still `Spawning`, a click stashed in
    /// `lsp_pending_definition` behind that spawn, or a definition/target-
    /// file round trip in flight (`lsp_inflight_requests`). Without this, a
    /// scripted ctrl/cmd-click's `wait_ready` unblocked before vtsls's spawn
    /// (up to 30s) or the definition/target-read round trip actually
    /// landed, making `state.lsp.target_viewer` a race against the script
    /// rather than a deterministic read. `wait_ready` polls this.
    #[cfg(feature = "automation")]
    pub(crate) fn automation_settled(&self) -> bool {
        if self
            .pr_picker
            .as_ref()
            .is_some_and(|p| matches!(p.state, PrPickerState::Loading))
        {
            return false;
        }
        if matches!(self.lsp_session, crate::lsp::LspSessionState::Spawning)
            || self.lsp_pending_definition.is_some()
            || self.lsp_inflight_requests > 0
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
    /// append at the end so they're never silently hidden. Read-only
    /// GitHub-side threads (docs/phase-6-review-navigator.md deliverable 6)
    /// are woven in here too, at the same anchor row as any local thread
    /// they share — this is the ONLY place they enter `self.display`
    /// (display rows are rebuilt per selected file, so a one-time append
    /// elsewhere would vanish on the next file switch). Also recomputes
    /// `github_resolved` (dv's own submitted-review threads that GitHub
    /// reports resolved) every call, since that's the only path back to
    /// the local `Thread` card once its matching remote thread gets
    /// deduped out below.
    fn reset_diff_list(&mut self, cx: &mut Context<Self>) -> bool {
        // Every path into this fn (view-mode toggle, expand-hunk-gap
        // recompute, a watcher-driven reload, a summary jump) can move the
        // diff content under an already-shown hover popover — the same
        // defect class `on_scroll_wheel` already guards against for wheel
        // scrolls (P3 finding: this fn was one of the paths that missed
        // it). `Self::select_file_inner` already clears this explicitly
        // before calling in, so this is a harmless no-op there; every other
        // caller gets the fix here instead of needing its own copy.
        //
        // Bump the epoch/release the claimed row UNCONDITIONALLY — an
        // in-flight debounced hover request that hasn't answered yet must
        // also be superseded (not just an already-shown popover cleared),
        // or its answer can land after the reload and pop a stale popover
        // anchored to content that just moved/reflowed away (P2 finding).
        // `cx.notify()` stays conditional on there being an existing
        // popover to actually clear.
        self.hover_request_epoch += 1;
        self.hover_request_line = None;
        if self.hover_popover.take().is_some() {
            cx.notify();
        }

        let row_count = self.diff_row_count();
        let file_path = self.selected.map(|i| self.files[i].path.clone());

        // Anchor each of this file's comments (local, then remote) to a
        // diff row. Holding `DisplayRow` values directly (rather than raw
        // comment/thread indices) lets both kinds share one grouping map.
        let mut at_row: HashMap<usize, Vec<DisplayRow>> = HashMap::new();
        let mut unanchored: Vec<DisplayRow> = Vec::new();
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
                    Some(row) => at_row.entry(row).or_default().push(DisplayRow::Thread(ci)),
                    None => unanchored.push(DisplayRow::Thread(ci)),
                }
            }
        }
        // Our own submitted review's threads round-trip through GitHub too
        // (`refresh_remote_threads` fetches every thread on the PR, dv's
        // own included) — skip those below so a comment dv already shows as
        // an interactive local `Thread` card doesn't ALSO draw a redundant
        // read-only `RemoteThread` card for the identical author/body
        // (review finding: threads were doubling for a submitted-then-
        // refreshed review). `review_database_id` exists precisely to
        // identify "this is our submitted review's thread" — compare it
        // against the currently-linked review's own submitted id.
        let own_submitted_review_id = self.pr_remote.as_ref().and_then(|r| r.submitted_review_id);

        // Recompute which local comments have a resolved own-submitted-
        // review GitHub thread, so the dedup below doesn't silently lose
        // that resolved state (review finding: it used to just discard the
        // skipped thread's `is_resolved`, defeating deliverable 6's
        // headline case for exactly the threads `submitted_review_id`
        // exists to identify). Scanned over the WHOLE review — not just
        // the selected file — because `render_summary` lists every comment
        // regardless of which file is open, and this needs to stay
        // consistent with it; matching is by (path, side, line) alone, so
        // it doesn't need that file's diff to be loaded. This position
        // match is inherently fragile — a later commit can shift or null
        // out `thread.line` (GitHub marks the thread "outdated"), and two
        // local comments sharing one (path, side, line) would both match a
        // single thread — but it's the only anchor a local `Comment` (which
        // stores no GitHub comment/thread id) carries. When `thread.line`
        // has gone null, fall back to matching the opening comment's
        // (author, body) so an outdated-but-resolved own thread still
        // reaches `github_resolved` (review finding: it previously bailed
        // out via `let Some(line) = thread.line else { continue }` before
        // ever considering such a thread, so the local `Thread`/summary
        // card kept showing it unresolved even though the read-only
        // `RemoteThread` card — only visible while this thread's file is
        // selected — showed it correctly). That fallback only feeds
        // `github_resolved`, never `matched_own_threads`: an outdated
        // thread's position can't be trusted enough to fully dedupe the
        // read-only card away. `matched_own_threads` records exactly the
        // own threads this loop managed to join to a local comment BY
        // POSITION (with no GitHub-only replies beyond what's stored
        // locally); anything it DIDN'T match that way — outdated with no
        // (author, body) match, line-drifted, or carrying a colleague's
        // reply the local store lacks — is deliberately left out of the
        // dedup below so it still renders as a read-only `RemoteThread`
        // card instead of vanishing (review findings: resolved-but-
        // outdated/drifted own threads, and GitHub-only replies on an own
        // thread, were both silently lost).
        let mut matched_own_threads: HashSet<usize> = HashSet::new();
        self.github_resolved.clear();
        if let (Some(review), Some(own_id)) = (&self.review, own_submitted_review_id) {
            for (ti, thread) in self.remote_threads.iter().enumerate() {
                if thread.review_database_id != Some(own_id) {
                    continue;
                }
                let side = match thread.side {
                    GhSide::Left => DiffSide::Old,
                    GhSide::Right => DiffSide::New,
                };
                for comment in &review.comments {
                    let comment_side = match comment.side {
                        dv_core::Side::Old => DiffSide::Old,
                        dv_core::Side::New => DiffSide::New,
                    };
                    if comment.path != thread.path || comment_side != side {
                        continue;
                    }
                    let position_match = thread.line == Some(comment.end_line);
                    let outdated_match = thread.line.is_none()
                        && thread.comments.first().is_some_and(|opening| {
                            opening.author == comment.author && opening.body == comment.body
                        });
                    if !position_match && !outdated_match {
                        continue;
                    }
                    if thread.is_resolved {
                        self.github_resolved.insert(comment.id.clone());
                    }
                    // Only safe to fully dedupe the read-only card if the
                    // match was by position (an (author, body) fallback
                    // match is too weak to trust for that) and GitHub
                    // doesn't carry a reply the local store lacks (review
                    // finding: a colleague's github.com reply to an own
                    // thread was disappearing).
                    if position_match && thread.comments.len() <= 1 + comment.replies.len() {
                        matched_own_threads.insert(ti);
                    }
                }
            }
        }
        if let Some(path) = &file_path {
            for (ti, thread) in self.remote_threads.iter().enumerate() {
                if &thread.path != path {
                    continue;
                }
                if own_submitted_review_id.is_some()
                    && thread.review_database_id == own_submitted_review_id
                    && matched_own_threads.contains(&ti)
                {
                    continue;
                }
                // `diffSide` is GitHub's LEFT/RIGHT — map onto the diff's
                // Old/New (cross-cutting note, docs/phase-6-review-
                // navigator.md S6f).
                let side = match thread.side {
                    GhSide::Left => DiffSide::Old,
                    GhSide::Right => DiffSide::New,
                };
                let row = thread.line.and_then(|line| self.anchor_row(side, line));
                match row {
                    Some(row) => at_row
                        .entry(row)
                        .or_default()
                        .push(DisplayRow::RemoteThread(ti)),
                    None => unanchored.push(DisplayRow::RemoteThread(ti)),
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
            if let Some(rows) = at_row.get(&row) {
                display.extend(rows.iter().copied());
            }
            if editor_row == Some(row) {
                display.push(DisplayRow::Editor);
            }
        }
        display.extend(unanchored);
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

    fn scroll_to_current_hunk(&mut self, cx: &mut Context<Self>) {
        let current = self.current_hunk;
        if let Some(&row) = self.hunk_rows().and_then(|rows| rows.get(current))
            && let Some(&display_ix) = self.diff_to_display.get(row)
        {
            self.diff_list.scroll_to(ListOffset {
                item_ix: display_ix,
                offset_in_item: px(0.),
            });
        }
        // A hunk jump (keyboard next/prev-hunk, or a view-mode toggle
        // re-anchoring on the current hunk) moves the diff content under a
        // shown hover popover exactly like the wheel-scroll case
        // `on_scroll_wheel` already guards against (P3 finding: this path
        // was missed) — the popover's anchored row can end up scrolled away
        // or simply no longer under the pointer, and if it scrolled out of
        // the list viewport its `.on_hover` leave listener is gone too, so
        // nothing else would ever clear it on keyboard-only navigation.
        //
        // Bump the epoch/release the claimed row UNCONDITIONALLY — an
        // in-flight debounced hover request must also be superseded here,
        // not just an already-shown popover cleared, or its answer can land
        // after the jump and pop a stale popover over the new hunk (P2
        // finding). `cx.notify()` stays conditional on an existing popover.
        self.hover_request_epoch += 1;
        self.hover_request_line = None;
        if self.hover_popover.take().is_some() {
            cx.notify();
        }
    }

    fn on_next_hunk(&mut self, _: &NextHunk, _: &mut Window, cx: &mut Context<Self>) {
        let count = self.hunk_rows().map_or(0, <[usize]>::len);
        if count == 0 {
            return;
        }
        self.current_hunk = (self.current_hunk + 1).min(count - 1);
        self.scroll_to_current_hunk(cx);
        cx.notify();
    }

    fn on_prev_hunk(&mut self, _: &PrevHunk, _: &mut Window, cx: &mut Context<Self>) {
        if self.hunk_rows().is_none_or(<[usize]>::is_empty) {
            return;
        }
        self.current_hunk = self.current_hunk.saturating_sub(1);
        self.scroll_to_current_hunk(cx);
        cx.notify();
    }

    // ---- Gutter selection (comment anchoring) ------------------------

    /// Mouse pressed on a line's gutter: start (or shift-extend) a
    /// selection on that side. This is the sole entry point into the
    /// gutter-selection → editor pipeline (`Self::gutter_up` only opens the
    /// editor for a selection this created), so gating it here is enough to
    /// suppress new comments on a SUBMITTED review (docs/phase-6-review-
    /// navigator.md S6c doc-deviation #4) without touching `gutter_up`/
    /// `open_editor` individually.
    ///
    /// Gated on [`Self::new_comment_blocked`] — see its doc comment for why
    /// that's narrower than plain `review_is_readonly()`.
    fn gutter_down(&mut self, side: DiffSide, line: u32, shift: bool, cx: &mut Context<Self>) {
        if self.new_comment_blocked() {
            return;
        }
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
        // Supersede any in-flight go-to-definition round trip (whole-phase
        // capstone review, P3 — see `on_open_pr_picker`'s matching bump and
        // `lsp_request_epoch`'s doc comment): otherwise a click stashed
        // while `lsp_session` is `Spawning` could complete after this
        // editor opens and commit the target viewer on top of it mid-type.
        self.lsp_request_epoch += 1;
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
                    // comment auto-creates a fresh draft. An in-app finish
                    // is itself an explicit state transition out of
                    // "browsing a pinned submitted review" — clear any pin
                    // (mirrors `open_pr`'s clear) so `new_comment_blocked`
                    // doesn't dead-end that fresh-draft continuation for a
                    // review the user reached via a sidebar-row pick
                    // (review finding, docs/phase-6-review-navigator.md
                    // S6c).
                    Ok(review) => {
                        this.review = Some(review);
                        this.pinned_review_id = None;
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
    /// `pr_meta` + `prepare_pr` + `dv_cli::submit::build_submission` — no
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
                    let pr_range = dv_cli::pr::prepare_pr(&repo, &meta)?;
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
                    // `fresh.remote.submitted_review_id` is now `Some` —
                    // refresh `pr_remote` from it so the github-thread
                    // dedup/resolved-sync in `reset_diff_list` (which reads
                    // `own_submitted_review_id` off `pr_remote`, not
                    // `review`) sees the submission this same session
                    // (review finding: submitting from inside dv otherwise
                    // left `pr_remote` stale for the rest of the session).
                    this.pr_remote = this.review.as_ref().and_then(|r| r.remote.clone());
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
            // Off-diff (docs/phase-6-review-navigator.md deliverable 5): the
            // comment's file isn't part of the currently open source at
            // all. Switch to the review's own recorded source first, then
            // land the jump — instead of the previous silent no-op.
            self.switch_source_and_jump(comment_id, window, cx);
            return;
        };
        self.pending_jump = Some(comment_id);
        self.select_file(file, window, cx);
        // If the diff was already cached, the jump consumed inside
        // select_file's reset; otherwise it fires when the compute lands.
    }

    /// Reopens the comment's own review-recorded source and lands the jump
    /// on it, for a summary click on a comment whose file isn't in the
    /// currently open diff (docs/phase-6-review-navigator.md deliverable 5).
    ///
    /// Doc-deviation #3: this reopens `review.source` — the concrete
    /// `Range { base, head }` (or plain `WorkingTree`/`Staged`/`Commit`)
    /// already recorded on the comment's own review at anchor time — via
    /// the same local `resolve_source` + `changed_files` path the initial
    /// load uses, NOT a fresh `gh pr fetch`. Faster, works offline, and it
    /// means the jump targets exactly what the comment was anchored
    /// against rather than whatever the live PR range happens to be today.
    ///
    /// Mirrors `open_pr`'s teardown block exactly: `diffs`/`diff_pending`/
    /// `expanded`/`stale`/`stale_checked` are all index-keyed into
    /// `self.files`, so replacing the file list without clearing them would
    /// render the wrong file's cached content under a reused index. Guarded
    /// by the same save-in-flight refusal as `open_pr`. The completion
    /// re-checks BOTH `source_epoch` (a second switch, or `open_pr`,
    /// superseding this one) AND that `self.review`'s id hasn't moved out
    /// from under it (cross-cutting risk C / finding P2-1: an epoch-only
    /// check can miss a change that never bumps the epoch in the first
    /// place — here, a store-watcher reload landing mid-flight and picking
    /// a *different* review).
    fn switch_source_and_jump(
        &mut self,
        comment_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Same "never orphan an in-flight save" contract as `open_pr`.
        if self.editor.as_ref().is_some_and(|e| e.saving)
            || self.thread_input.as_ref().is_some_and(|t| t.saving)
            || self.submit_in_flight()
        {
            return;
        }
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let Some(comment) = review.comments.iter().find(|c| c.id == comment_id) else {
            return;
        };
        let target_source = review.source.clone();
        if target_source == self.source {
            // The comment's own review is already anchored against this
            // exact source — its file genuinely isn't part of this diff
            // (should be impossible for a well-formed comment). Nothing to
            // switch to; surface it rather than silently doing nothing.
            self.source_switch_error =
                Some("comment's file isn't part of this review's diff".into());
            cx.notify();
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let review_id = review.id.clone();
        let path = comment.path.clone();

        // A parked submit panel is scoped to the source being left behind
        // (same reasoning as `open_pr`); `Submitting` was already refused
        // above, so this can only cancel, never clobber an in-flight POST.
        self.cancel_submit_flow(cx);
        self.source_switch_error = None;
        self.status = Status::Loading;
        self.source_epoch += 1;
        let epoch = self.source_epoch;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let source = resolve_source(&repo, target_source)?;
                    let files = repo.changed_files(&source)?;
                    anyhow::Ok((source, files))
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                if this.source_epoch != epoch {
                    // A newer switch/`open_pr` superseded this one before
                    // this fetch finished — it already set `Loading` itself
                    // and owns transitioning back to `Ready` on its own
                    // completion (same as `open_pr`'s own epoch-mismatch
                    // arm, which leaves `status` alone entirely).
                    return;
                }
                if this.review.as_ref().map(|r| r.id.as_str()) != Some(review_id.as_str()) {
                    // The review changed out from under us via a
                    // store-watcher reload (finding P2-1's generalization:
                    // that reload never bumps `source_epoch`, so the epoch
                    // check above can't catch it) — no other in-flight
                    // operation is going to fix `status` for us, so reset it
                    // here or the workspace wedges in `Loading` forever.
                    this.status = Status::Ready;
                    cx.notify();
                    return;
                }
                this.status = Status::Ready;
                match outcome {
                    Ok((source, files)) => {
                        this.selection = None;
                        let dropped_editor = this.editor.take().is_some();
                        let dropped_input = this.thread_input.take().is_some();
                        if dropped_editor || dropped_input {
                            window.focus(&this.focus_handle, cx);
                        }
                        if matches!(this.source, DiffSource::WorkingTree) {
                            // Same one-way teardown `open_pr` does — there's
                            // no path back to `WorkingTree` from here either.
                            this._worktree_watcher = None;
                        }
                        this.source = source;
                        this.source_desc = source_label(&this.source).into();
                        // Index-keyed into the old file list — stale
                        // caches would render the wrong file under the
                        // right name (same as `open_pr`).
                        this.diffs.clear();
                        this.diffstat_cache.set(None);
                        this.diff_pending.clear();
                        this.expanded.clear();
                        this.stale.clear();
                        this.stale_checked = None;
                        // The GitHub remote threads (S6f) were fetched against
                        // the PR's LIVE range; this jump moves `source` to the
                        // review's own recorded range, where those line numbers
                        // don't apply. Clear them (as `open_pr`'s teardown does)
                        // so they can't misanchor or double-render own-comment
                        // cards against the wrong coordinates; a later `open_pr`
                        // refetches for the live range (capstone P3).
                        this.remote_threads.clear();
                        this.selected = None;
                        this.pending_jump = None;
                        this.files = files;
                        match this.files.iter().position(|f| f.path == path) {
                            Some(index) => {
                                this.pending_jump = Some(comment_id.clone());
                                this.select_file(index, window, cx);
                                // Consumed inside select_file's reset if the
                                // diff is already cached; otherwise it fires
                                // once the compute lands (same as a
                                // same-source jump).
                            }
                            None => {
                                // Should be impossible for a well-formed
                                // comment (its own review's recorded source
                                // doesn't contain its own path) — land on
                                // the switched source anyway rather than
                                // wedge, just without a jump target.
                                this.source_switch_error = Some(
                                    "comment's file isn't in its review's recorded source".into(),
                                );
                                if !this.files.is_empty() {
                                    this.selected = Some(0);
                                    this.current_hunk = 0;
                                    this.request_diff(0, cx);
                                }
                                this.reset_diff_list(cx);
                            }
                        }
                    }
                    Err(err) => {
                        // Leave source/files exactly as they were — same
                        // "don't tear down a working view over a failed
                        // attempt" posture as `open_pr`'s error arm.
                        this.source_switch_error = Some(format!("{err:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        // Resolved once at load (`dv_cli::author::resolve_author`); falls
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
    /// off-thread. Gated behind `review_is_readonly` (docs/phase-6-review-
    /// navigator.md S6c doc-deviation #4) — Resolve/Unresolve on a
    /// SUBMITTED review's thread card is a no-op. That synchronous check is
    /// only half the guard: the freshly-loaded review is re-checked below
    /// too, since a submit can land on disk in the window between this
    /// click and the fresh load completing (review finding, S6c).
    fn set_comment_status(
        &mut self,
        comment_id: String,
        status: dv_core::CommentStatus,
        cx: &mut Context<Self>,
    ) {
        if self.review_is_readonly() {
            return;
        }
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
                    // Re-check against the freshly-loaded state, not the
                    // stale clone the synchronous check above saw.
                    if matches!(review.state, dv_core::ReviewState::Submitted { .. }) {
                        return anyhow::Ok((review, false));
                    }
                    review.set_status(&comment_id, status)?;
                    store.save(&review)?;
                    anyhow::Ok((review, true))
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((review, applied)) => {
                        this.review = Some(review);
                        cx.emit(ReviewChanged);
                        if applied {
                            // Resolving/reopening a comment can invalidate a
                            // parked submit panel (review finding P1-3).
                            this.cancel_submit_flow_if_parked(cx);
                        }
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

    /// Delete a comment (by id), persisting off-thread. Gated behind
    /// `review_is_readonly` (docs/phase-6-review-navigator.md S6c
    /// doc-deviation #4) — Delete on a SUBMITTED review's thread card is a
    /// no-op. Re-checked again against the freshly-loaded review below —
    /// see `set_comment_status`'s doc comment for why the synchronous check
    /// alone isn't enough.
    fn delete_comment(&mut self, comment_id: String, cx: &mut Context<Self>) {
        if self.review_is_readonly() {
            return;
        }
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
                    if matches!(review.state, dv_core::ReviewState::Submitted { .. }) {
                        return anyhow::Ok((review, false));
                    }
                    review.comments.retain(|c| c.id != comment_id);
                    review.updated_ms = dv_core::review::now_ms();
                    store.save(&review)?;
                    anyhow::Ok((review, true))
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((review, applied)) => {
                        this.review = Some(review);
                        cx.emit(ReviewChanged);
                        if applied {
                            // Deleting a comment can invalidate a parked
                            // submit panel (review finding P1-3).
                            this.cancel_submit_flow_if_parked(cx);
                        }
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
    /// open at a time. Gates both `Reply` and `EditBody` behind
    /// `review_is_readonly` (docs/phase-6-review-navigator.md S6c
    /// doc-deviation #4) — a SUBMITTED review's thread cards keep their
    /// buttons rendered (this is a suppress-the-entry-point posture, not a
    /// hide-the-controls one) but the click does nothing.
    fn open_thread_input(
        &mut self,
        comment_id: String,
        mode: ThreadInputMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_component::input::{InputEvent, InputState};
        if self.review_is_readonly() {
            return;
        }
        // An in-flight save keeps its input alive: replacing it here would
        // drop the pending body (and the completion would close the NEW
        // input). Finish or fail first.
        if self.thread_input.as_ref().is_some_and(|ti| ti.saving) {
            return;
        }
        // Supersede any in-flight go-to-definition round trip (whole-phase
        // capstone review, P3 — see `on_open_pr_picker`'s matching bump and
        // `lsp_request_epoch`'s doc comment): otherwise a click stashed
        // while `lsp_session` is `Spawning` could complete after this
        // reply/edit box opens and commit the target viewer on top of it
        // mid-type.
        self.lsp_request_epoch += 1;
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
    /// submit_comment for why the UI clone can't be trusted). Unlike
    /// `open_thread_input` (which only checks `review_is_readonly` at
    /// dispatch time, when the input is opened), this re-checks the
    /// freshly-loaded review's state right before saving — a submit (in-
    /// app or via the CLI) can land on disk while the input sat open
    /// (review finding, docs/phase-6-review-navigator.md S6c). There's no
    /// "start a fresh draft" fallback for a reply/edit the way
    /// `submit_comment` has for a brand-new comment — a reply targets a
    /// specific comment id on a specific (now-submitted) review — so a
    /// save that loses this race is simply dropped and the input closed.
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
                    // Re-check against the freshly-loaded state — the
                    // `review_is_readonly` check `open_thread_input` did
                    // when this input was opened can be stale by now.
                    if matches!(review.state, dv_core::ReviewState::Submitted { .. }) {
                        return anyhow::Ok((review, false));
                    }
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
                    anyhow::Ok((review, true))
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
                    Ok((review, applied)) => {
                        this.review = Some(review);
                        cx.emit(ReviewChanged);
                        if applied {
                            // A reply or body edit can invalidate a parked
                            // submit panel (review finding P1-3).
                            this.cancel_submit_flow_if_parked(cx);
                        } else {
                            eprintln!("thread input save skipped: review was submitted");
                        }
                        // Whether applied or lost the submitted-race, there's
                        // nothing left for this input to do — a submitted
                        // review has no fresh-draft fallback for a reply/edit
                        // (see this fn's doc comment), so leaving it open
                        // would just let the user retry into the same wall.
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

    // ---- Go-to-definition (S8f, docs/phase-8-lsp-and-polish.md § LSP) --
    //
    // Scope note: the honest-view gate below matches the phase plan's
    // key_signatures exactly — "WorkingTree, or a Range whose head oid
    // equals the checked-out worktree HEAD" — using
    // `Self::worktree_head_oid`, a cache refreshed off-thread on initial
    // load and every `Self::revalidate` pass rather than an extra
    // `git rev-parse` round trip per click. Still scoped to the New side of
    // a line (see `Self::row_anchor`: New is exactly "this line's bytes are
    // what's on disk right now"); a stale/absent cache just declines the
    // Range case (never a wrong answer, only a missed one).

    /// Whether the diff pane is showing bytes that match what `vtsls` (which
    /// always reads the on-disk checked-out worktree) will actually answer
    /// about — see this section's scope note above.
    fn lsp_view_is_honest(&self) -> bool {
        match &self.source {
            DiffSource::WorkingTree => true,
            DiffSource::Range { head, .. } => {
                self.worktree_head_oid.as_deref() == Some(head.as_str())
            }
            DiffSource::Staged | DiffSource::Commit(_) => false,
        }
    }

    fn selected_file_path(&self) -> Option<String> {
        self.selected
            .and_then(|i| self.files.get(i))
            .map(|f| f.path.clone())
    }

    /// Whether `rel_path`'s on-disk (working-tree) bytes still match its
    /// blob at `head_rev` — the per-file honest-view re-check both
    /// `Self::run_definition_request` and `Self::on_symbol_hover` perform
    /// before trusting any vtsls answer on a `DiffSource::Range` view.
    /// `Self::lsp_view_is_honest`'s whole-view gate alone can't see an
    /// uncommitted LOCAL edit to just this one file (P3 finding: vtsls
    /// always reads on-disk bytes, so a `head == worktree HEAD` view can
    /// still be showing this file's stale, committed blob at the displayed
    /// line numbers).
    fn range_head_matches_working(repo: &GitRepo, rel_path: &str, head_rev: &str) -> bool {
        let head_sha = repo
            .blob_sha(&BlobSpec::Rev {
                rev: head_rev.to_string(),
                path: rel_path.to_string(),
            })
            .ok()
            .flatten();
        let working_sha = repo
            .blob_sha(&BlobSpec::Working {
                path: rel_path.to_string(),
            })
            .ok()
            .flatten();
        head_sha == working_sha
    }

    /// Column-hit-test a click inside a rendered diff line: shapes `text`
    /// with the SAME font/size the row actually rendered at (so this is
    /// glyph-accurate, not an approximate monospace-width guess) and finds
    /// the closest byte offset to `local_x` (relative to the text content's
    /// own left edge — see `Self::wrap_symbol_click_target`'s bounds
    /// capture for how that's obtained).
    fn hit_test_byte_column(
        text: &str,
        font_family: SharedString,
        font_size: f32,
        local_x: Pixels,
        window: &mut Window,
    ) -> usize {
        if text.is_empty() {
            return 0;
        }
        let run = TextRun {
            len: text.len(),
            font: font(font_family),
            ..Default::default()
        };
        // `layout_line` lives on `WindowTextSystem` (per-window glyph atlas
        // state), not the App-level `TextSystem` `cx.text_system()` hands
        // back — hence taking `&mut Window` here rather than `&mut App`/`cx`.
        let layout = window
            .text_system()
            .layout_line(text, px(font_size), &[run], None);
        layout.closest_index_for_x(local_x.max(px(0.)))
    }

    /// Wrap `content` (a rendered line's text div) so a ctrl/cmd-click
    /// inside it drives go-to-definition. Only ever called for a row/cell
    /// that's on the New side with a real line number (see `render_diff_row`/
    /// `render_split_cell`'s call sites) — Removed-only unified rows and Old
    /// split cells never get this wrapper at all, which is what enforces
    /// the New-side half of `Self::lsp_view_is_honest`'s scope. Returns
    /// `AnyElement`, not `Div`: the wrapper needs `.id()` to use `.on_hover`
    /// (P2 finding fix — see that call site's comment), which turns it into
    /// a `Stateful<Div>`, a different type than the un-wrapped `Div` its
    /// callers' `_ => content` arms produce; erasing both to `AnyElement`
    /// keeps those `match` arms type-uniform.
    fn wrap_symbol_click_target(
        &self,
        content: Div,
        new_line: u32,
        text: SharedString,
        cx: &Context<Self>,
    ) -> AnyElement {
        let font_family = cx.theme().mono_font_family.clone();
        let font_size = self.font_size;
        // Captures this row's own on-screen bounds at paint time (a plain
        // `MouseDownEvent` only carries a WINDOW-relative position, not an
        // element-relative one) — read back at click time to turn the
        // click's x into a column via `hit_test_byte_column`. Cheap: one
        // `Rc<Cell<..>>` per visible row, only for rows this wrapper is
        // even called on (New-side, real line number).
        let bounds: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
        let bounds_for_paint = Rc::clone(&bounds);
        let bounds_for_click = Rc::clone(&bounds);
        let bounds_for_hover = Rc::clone(&bounds);
        let probe = canvas(
            move |b, _, _| {
                bounds_for_paint.set(Some(b));
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        // S8g: hover shares this exact wrapper (same New-side/real-line
        // scope, same bounds probe) rather than a second one — cloned ahead
        // of the `on_mouse_down` closure below, which moves its own copies.
        let text_for_hover = text.clone();
        let font_family_for_hover = font_family.clone();

        div()
            .id(("symbol-hover-target", new_line as usize))
            .relative()
            .size_full()
            .child(probe)
            .child(content)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    if !ev.modifiers.secondary() {
                        return; // plain click — normal text/selection, not go-to-def
                    }
                    let Some(row_bounds) = bounds_for_click.get() else {
                        return;
                    };
                    let local_x = ev.position.x - row_bounds.origin.x;
                    let byte_col = Self::hit_test_byte_column(
                        &text,
                        font_family.clone(),
                        font_size,
                        local_x,
                        window,
                    );
                    this.on_symbol_click(new_line, byte_col, text.clone(), window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, window, cx| {
                // A button held during this move means a drag (e.g. the
                // summary-panel resize handle), not a hover-intent rest —
                // and gpui's `.on_hover` below forces `is_hovered` to
                // `false` for the whole drag (`div.rs`'s hover listener
                // checks `!cx.has_active_drag()`), so a popover raised here
                // mid-drag would never get a matching leave and would stick
                // after release (P3 finding). `.on_hover`'s leave side
                // already effectively enforces this; mirror it here on the
                // raise side too.
                if ev.pressed_button.is_some() {
                    return;
                }
                let Some(row_bounds) = bounds_for_hover.get() else {
                    return;
                };
                let local_x = ev.position.x - row_bounds.origin.x;
                let byte_col = Self::hit_test_byte_column(
                    &text_for_hover,
                    font_family_for_hover.clone(),
                    font_size,
                    local_x,
                    window,
                );
                this.on_symbol_hover(new_line, byte_col, text_for_hover.clone(), ev.position, cx);
            }))
            // NOT `.on_mouse_exit(MouseExitEvent)` (P2 finding, live-verified
            // stuck-popover): gpui's Windows backend never dispatches
            // `PlatformInput::MouseExited` at all (only the mac/linux/web
            // backends do — `gpui_windows`'s WM_MOUSELEAVE handler only
            // flips the window's own hovered flag), so that listener was
            // dead code on the only platform dv ships on. `.on_hover`
            // instead recomputes this element's own hitbox-hover state from
            // `hitbox.is_hovered(window)` on every `MouseMoveEvent` anywhere
            // in the window (see gpui's `div.rs`), so it correctly flips to
            // `false` on pointer-out on every platform — requires `.id()`
            // above (on_hover is stateful). NOTE this is registered against
            // `MouseMoveEvent`/`MouseExitEvent` only, same as gpui's own
            // hover machinery — it does NOT flip on a scroll-wheel event
            // with the mouse stationary (a wheel scroll dispatches
            // `ScrollWheelEvent`, never a synthetic move), and once this row
            // scrolls out of the `uniform_list`/`list` viewport its listener
            // stops being registered at all, so neither a wheel-scroll nor a
            // later move over some OTHER, ineligible surface can ever fire
            // this row's own leave again (P2 finding, corrected: the popover
            // used to be claimed self-healing here — it is not). The diff
            // pane's own `.on_scroll_wheel` (see its container in `render`)
            // is what actually clears a popover left behind by a scroll.
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                if !*hovered {
                    this.on_symbol_hover_leave(new_line, cx);
                }
            }))
            .into_any_element()
    }

    /// Entry point for a ctrl/cmd-click on an eligible (New-side, real line)
    /// diff row/cell — `new_line` is the 1-based diff line number, `byte_col`
    /// a byte offset into `line_text` from `Self::hit_test_byte_column`.
    /// Every early-out sets `self.lsp_status` to a short, human-readable
    /// reason rather than silently doing nothing (docs/phase-8-lsp-and-polish.md
    /// § LSP: "surface a gentle warning ... don't fail" — this is that
    /// warning's UI-visible half).
    fn on_symbol_click(
        &mut self,
        new_line: u32,
        byte_col: usize,
        line_text: SharedString,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.lsp_view_is_honest() {
            self.lsp_status =
                Some("code intelligence: only available on the working-tree view".into());
            cx.notify();
            return;
        }
        let RepoLocation::Wsl {
            distro,
            path: root_path,
        } = self.location.clone()
        else {
            self.lsp_status = Some(
                "code intelligence: WSL repos only (docs/phase-8-lsp-and-polish.md § LSP)".into(),
            );
            cx.notify();
            return;
        };
        let Some(rel_path) = self.selected_file_path() else {
            return;
        };
        let Some(language_id) = dv_core::lsp::language_id_for_path(&rel_path) else {
            self.lsp_status = Some("code intelligence: not a TypeScript/JavaScript file".into());
            cx.notify();
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };

        let character = utf16_column(&line_text, byte_col);
        let position = lsp_types::Position {
            line: new_line.saturating_sub(1),
            character,
        };
        let root_trimmed = root_path.trim_end_matches('/').to_string();
        let uri = dv_core::lsp::file_uri(&format!("{root_trimmed}/{rel_path}"));
        let from_location = crate::lsp::Location {
            uri: uri.clone(),
            line: position.line,
            character: position.character,
        };
        // This click supersedes any earlier in-flight go-to-def round trip
        // (definition request, target-file read, or a queued click still
        // waiting on `Spawning`) — see `lsp_request_epoch`'s doc comment.
        self.lsp_request_epoch += 1;
        let epoch = self.lsp_request_epoch;
        // `Self::lsp_view_is_honest` only compares the WHOLE view's oid
        // against the worktree HEAD; for a `Range` view that still leaves a
        // per-file gap — the displayed bytes are `head`'s committed blob,
        // but vtsls always reads THIS file's on-disk bytes, which can carry
        // uncommitted local edits even when `head == worktree HEAD` (P3
        // finding). `run_definition_request` re-checks this file specifically
        // (in the background, alongside the sync it already does) before
        // ever asking vtsls anything.
        let range_head = match &self.source {
            DiffSource::Range { head, .. } => Some(head.clone()),
            _ => None,
        };

        // A vtsls child that died mid-session (crash, `wsl --terminate`,
        // OOM) otherwise leaves `Ready` installed forever: the reader thread
        // flips `is_alive()` false, but nothing else observes it, so every
        // click here would keep failing with "lsp connection lost" until the
        // workspace is parked or a fresh one is opened (P3 finding). Reset
        // to `Unattempted` so THIS click falls straight into the same lazy
        // respawn path below a brand-new workspace would take.
        if let crate::lsp::LspSessionState::Ready(handle) = &self.lsp_session
            && !handle.is_alive()
        {
            self.lsp_session = crate::lsp::LspSessionState::Unattempted;
            self.lsp_ready_since = None;
        }

        // An earlier attempt found vtsls missing, or the spawn/handshake
        // itself failed. Every path that can land here already cleared the
        // WSL/TS/honest-view gates above (unlike, say, a local/Windows repo,
        // which bails out long before ever consulting `lsp_session`), so
        // nothing captured in this state is a permanent, structural block —
        // it can change. The common trigger is a ctrl-click landing mid-
        // install (S8e's up-to-180s consent-triggered `npm install -g`,
        // before the reverify flips the onboarding row to `Ok`): without
        // this reset, every later click kept reading the same stale
        // verdict forever, even once vtsls was actually installed and
        // working, with only a park (review switch away/back) or an app
        // restart able to heal it (P2 finding). Treat this fresh click —
        // itself a deliberate user action, not an automatic retry — as
        // license to re-detect rather than trust the old answer.
        if matches!(
            self.lsp_session,
            crate::lsp::LspSessionState::Unavailable(_)
        ) {
            self.lsp_session = crate::lsp::LspSessionState::Unattempted;
        }

        match &self.lsp_session {
            crate::lsp::LspSessionState::Ready(handle) => {
                let handle = handle.clone();
                let ready_since = self.lsp_ready_since;
                self.run_definition_request(
                    handle,
                    repo,
                    rel_path,
                    uri,
                    language_id,
                    position,
                    from_location,
                    range_head,
                    epoch,
                    ready_since,
                    cx,
                );
            }
            crate::lsp::LspSessionState::Spawning => {
                self.lsp_status = Some("code intelligence: starting…".into());
                // Stash this click's request rather than dropping it — see
                // `Self::lsp_pending_definition`'s doc comment for the races
                // this fixes. A later click while still `Spawning` simply
                // replaces whatever was stashed before.
                self.lsp_pending_definition = Some(PendingDefinition {
                    repo,
                    rel_path,
                    uri,
                    language_id,
                    position,
                    from: from_location,
                    range_head,
                    epoch,
                });
                cx.notify();
            }
            crate::lsp::LspSessionState::Unavailable(reason) => {
                self.lsp_status = Some(format!("code intelligence unavailable: {reason}").into());
                cx.notify();
            }
            crate::lsp::LspSessionState::Unattempted => {
                self.lsp_session = crate::lsp::LspSessionState::Spawning;
                self.lsp_status = Some("code intelligence: starting…".into());
                // Stash this click's request the same way the `Spawning` arm
                // above does — this click happens to be the one that
                // triggers the spawn, but the completion below always
                // replays whatever's CURRENTLY stashed rather than assuming
                // it's still this one (see `Self::lsp_pending_definition`'s
                // doc comment — a park+reactivate racing two spawn attempts
                // is exactly the case that requires this).
                self.lsp_pending_definition = Some(PendingDefinition {
                    repo,
                    rel_path,
                    uri,
                    language_id,
                    position,
                    from: from_location,
                    range_head,
                    epoch,
                });
                cx.notify();
                self.lsp_spawn_generation += 1;
                let spawn_gen = self.lsp_spawn_generation;
                let root_uri = dv_core::lsp::file_uri(&root_trimmed);
                let location = self.location.clone();
                cx.spawn(async move |this, cx| {
                    let spawned = cx
                        .background_spawn(async move {
                            match dv_core::provision::detect_node_vtsls(&distro, None) {
                                Ok(nv) if nv.vtsls_path.is_some() => {
                                    dv_core::lsp::LspHandle::spawn(&location, &nv, &root_uri).map(
                                        |handle| {
                                            // docs/phase-8-lsp-and-polish.md §
                                            // LSP.1: "surface a gentle warning
                                            // when absent, don't fail" — run
                                            // this bounded check in the same
                                            // background task, before the
                                            // handle is ever handed back to
                                            // the UI thread (P2 finding: this
                                            // warning didn't exist at all).
                                            let node_modules_missing =
                                                !dv_core::lsp::node_modules_present(&location);
                                            (handle, node_modules_missing)
                                        },
                                    )
                                }
                                Ok(_) => Err(dv_core::lsp::LspError::Unavailable(
                                    "node found but vtsls is not installed for this distro"
                                        .to_string(),
                                )),
                                Err(err) => Err(dv_core::lsp::LspError::Unavailable(format!(
                                    "node/vtsls detection failed: {err:#}"
                                ))),
                            }
                        })
                        .await;

                    // `spawned` must survive past `this.update` even when the
                    // closure below never runs at all — not just when it runs
                    // and finds a generation mismatch. If the workspace
                    // entity was released outright while this spawn was in
                    // flight (e.g. a no-review workspace dropped rather than
                    // parked by `Self::stash_active` — capstone P3 finding),
                    // `update` returns `Err` without invoking its closure, so
                    // a `spawned` moved directly into that closure would be
                    // dropped right here, inline on this task's executor
                    // (the same foreground executor driving the UI — see the
                    // comment below on why that matters). Stash it in a cell
                    // the closure borrows from instead, so it's still ours to
                    // dispose of in the `Err` case after `update` returns.
                    let spawned = std::cell::RefCell::new(Some(spawned));
                    let updated = this.update(cx, |this, cx| {
                        let spawned = spawned
                            .borrow_mut()
                            .take()
                            .expect("update's closure runs at most once");
                        // The workspace was parked (`Self::park_lsp_session`
                        // resets `Spawning` -> `Unattempted` AND bumps the
                        // generation) or a later click already kicked off
                        // its OWN spawn attempt (also bumping the
                        // generation) while this attempt was in flight —
                        // either way, this attempt has been superseded and
                        // must not mutate `lsp_session`/
                        // `lsp_pending_definition` at all: not to install
                        // `Ready`/`Unavailable` (would resurrect a vtsls
                        // child, or permanently poison retry, on a session
                        // nobody's looking at anymore — P2 finding), and not
                        // even to clear `lsp_pending_definition` on `Err`
                        // (would eat whatever the winning attempt has
                        // stashed — P3 finding: a bare `Spawning`-only guard
                        // couldn't tell two overlapping attempts apart, so a
                        // stale attempt's `Err` could clobber a second,
                        // still-in-flight attempt's state, discarding its
                        // click even though that second attempt goes on to
                        // succeed). Discard `spawned`'s `Ok` handle off the
                        // UI thread rather than dropping it inline here:
                        // `LspClient`'s `Drop` runs a bounded but real
                        // shutdown handshake (up to ~1.5s if vtsls is
                        // wedged) that must never block the thread driving
                        // this update — often the UI thread (P3 finding).
                        if this.lsp_spawn_generation != spawn_gen {
                            if let Ok((handle, _)) = spawned {
                                cx.background_spawn(async move { drop(handle) }).detach();
                            }
                            return;
                        }
                        match spawned {
                            Ok((handle, node_modules_missing)) => {
                                this.lsp_session =
                                    crate::lsp::LspSessionState::Ready(handle.clone());
                                this.lsp_status = None;
                                this.lsp_ready_since = Some(std::time::Instant::now());
                                this.lsp_node_modules_warning = node_modules_missing.then(|| {
                                    SharedString::from(
                                        "code intelligence: node_modules not installed — \
                                         definitions into packages may be unavailable",
                                    )
                                });
                                // Replay whichever request is CURRENTLY
                                // stashed — not necessarily this attempt's
                                // own triggering click (see
                                // `Self::lsp_pending_definition`'s doc
                                // comment) — and only if nothing has
                                // superseded it since (another click, a nav,
                                // a file switch).
                                if let Some(pending) = this.lsp_pending_definition.take()
                                    && this.lsp_request_epoch == pending.epoch
                                {
                                    let ready_since = this.lsp_ready_since;
                                    this.run_definition_request(
                                        handle,
                                        pending.repo,
                                        pending.rel_path,
                                        pending.uri,
                                        pending.language_id,
                                        pending.position,
                                        pending.from,
                                        pending.range_head,
                                        pending.epoch,
                                        ready_since,
                                        cx,
                                    );
                                }
                                // See the `Err` arm below — this closure has
                                // no other reason to repaint (the replay
                                // above, if any, notifies on its own once
                                // the definition round trip resolves), but
                                // the banner/warning just changed either way
                                // (P3 finding: this was missing here, so a
                                // superseded pending click left the "code
                                // intelligence: starting…" status and the
                                // freshly set node_modules warning both
                                // stuck on-screen until some unrelated
                                // repaint).
                                cx.notify();
                            }
                            Err(err) => {
                                this.lsp_status =
                                    Some(format!("code intelligence unavailable: {err}").into());
                                this.lsp_session =
                                    crate::lsp::LspSessionState::Unavailable(err.to_string());
                                this.lsp_pending_definition = None;
                                cx.notify();
                            }
                        }
                    });
                    // `update`'s closure above takes at most once — if it
                    // never ran at all (entity released, not just
                    // superseded), `spawned` is still sitting in the cell;
                    // dispose of any `Ok` handle the same way the
                    // generation-mismatch arm does, off this task's
                    // executor.
                    if updated.is_err()
                        && let Some(Ok((handle, _))) = spawned.into_inner()
                    {
                        cx.background_spawn(async move { drop(handle) }).detach();
                    }
                })
                .detach();
            }
        }
    }

    // ---- Hover (S8g, docs/phase-8-lsp-and-polish.md § LSP) -------------
    //
    // Same honest-view gate and New-side-only scope as go-to-definition
    // (`Self::wrap_symbol_click_target`'s callers), but deliberately does
    // NOT spawn a session: unlike a ctrl/cmd-click, a hover is a passive
    // side-effect of just moving the mouse across the diff — spawning a
    // `vtsls` child for every stray sweep over a WSL TypeScript file would
    // be a surprising, unrequested cost (and, worse, a boot-storm vector if
    // it happened while merely reviewing without ever meaning to use code
    // intelligence). Hover only ever activates once something else (a
    // click) has already brought the session to `Ready`.

    /// Entry point for a plain (no modifier needed) mouse-move over an
    /// eligible diff token — mirrors `Self::on_symbol_click`'s argument
    /// shape (`new_line`/`byte_col`/`line_text`) plus the WINDOW-relative
    /// `anchor` point the popover renders at. Silent on every early-out
    /// (no status banner, unlike `on_symbol_click`'s "surface a gentle
    /// warning" posture) — a hover that declines to answer is the ordinary
    /// case on every plain mouse sweep across a diff, not a user action
    /// worth narrating.
    fn on_symbol_hover(
        &mut self,
        new_line: u32,
        byte_col: usize,
        line_text: SharedString,
        anchor: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        // Bump first, unconditionally: even a gated-off hover (wrong view,
        // no session yet, …) must supersede whatever earlier hover request
        // is still in flight for this exact spot, or a slow stale answer
        // could pop the popover back up after the mouse has already left.
        self.hover_request_epoch += 1;
        let epoch = self.hover_request_epoch;
        // Claim this epoch for `new_line` — see `hover_request_line`'s doc
        // comment for why `on_symbol_hover_leave` needs to know this.
        self.hover_request_line = Some(new_line);

        if !self.lsp_view_is_honest() {
            return;
        }
        let RepoLocation::Wsl {
            path: root_path, ..
        } = self.location.clone()
        else {
            return;
        };
        let Some(rel_path) = self.selected_file_path() else {
            return;
        };
        let Some(language_id) = dv_core::lsp::language_id_for_path(&rel_path) else {
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        // Hover never spawns — see this section's scope note above.
        let crate::lsp::LspSessionState::Ready(handle) = &self.lsp_session else {
            return;
        };
        let handle = handle.clone();

        let character = utf16_column(&line_text, byte_col);
        let position = lsp_types::Position {
            line: new_line.saturating_sub(1),
            character,
        };
        let uri =
            dv_core::lsp::file_uri(&format!("{}/{}", root_path.trim_end_matches('/'), rel_path));
        // Same per-file re-check `Self::run_definition_request` performs
        // (P3 finding: hover captured no `range_head` at all and skipped
        // this entirely) — `Self::lsp_view_is_honest`'s whole-view gate
        // above only compares the view's oid against the worktree HEAD, so
        // a `Range` view with an uncommitted LOCAL edit to just this file
        // would otherwise show a confidently wrong tooltip.
        let range_head = match &self.source {
            DiffSource::Range { head, .. } => Some(head.clone()),
            _ => None,
        };
        // Captured here, not read fresh inside the background task (which
        // has no `&self`) — same pattern `Self::on_symbol_click` uses before
        // calling `Self::run_definition_request`.
        let ready_since = self.lsp_ready_since;

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HOVER_DEBOUNCE).await;
            // Re-check BEFORE doing any work at all (the debounce's whole
            // point — see `HOVER_DEBOUNCE`'s doc comment): a mouse sweep
            // spawns one of these per pixel, and only the one still current
            // once the debounce elapses should ever reach vtsls.
            let still_current = this
                .update(cx, |this, _| this.hover_request_epoch == epoch)
                .unwrap_or(false);
            if !still_current {
                return;
            }

            let result = cx
                .background_spawn(async move {
                    if let Some(head) = &range_head
                        && !Self::range_head_matches_working(&repo, &rel_path, head)
                    {
                        // Silently decline — same posture as any other
                        // gated-off hover (no status banner; see this
                        // section's scope note above).
                        return Ok::<_, dv_core::lsp::LspError>(None);
                    }
                    let text = repo
                        .blob_bytes(&BlobSpec::Working {
                            path: rel_path.clone(),
                        })
                        .ok()
                        .flatten()
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        .unwrap_or_default();
                    let _ = handle.sync_document(&uri, language_id, &text);
                    let mut hover = handle.hover(&uri, position)?;
                    // Same warm-up posture as `Self::run_definition_request`
                    // (P3 finding: this used to be missing here entirely) —
                    // vtsls's semantic tsserver can still be indexing for a
                    // few seconds after a session first reaches `Ready`, and
                    // its fast syntax tsserver answers hover with `null` in
                    // the meantime. Retrying a handful of times only within
                    // `LSP_WARMUP_WINDOW` keeps a warm session's genuinely
                    // empty answer (hovering whitespace, say) instant.
                    if hover.is_none()
                        && ready_since.is_some_and(|since| since.elapsed() < LSP_WARMUP_WINDOW)
                    {
                        for _ in 0..LSP_WARMUP_RETRIES {
                            std::thread::sleep(LSP_WARMUP_RETRY_DELAY);
                            hover = handle.hover(&uri, position)?;
                            if hover.is_some() {
                                break;
                            }
                        }
                    }
                    Ok(hover)
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                if this.hover_request_epoch != epoch {
                    return; // superseded while the round trip was in flight
                }
                // The pointer can leave the window entirely while this round
                // trip is in flight with no in-window `MouseMoveEvent` ever
                // following to supersede `epoch` (P3 finding) — gpui's
                // Windows backend never dispatches a `MouseExited` INPUT
                // event (see `Self::wrap_symbol_click_target`'s own doc
                // comment), so a debounced hover answer landing after an
                // alt-tab-away or a flick off the window's edge would
                // otherwise pop a popover up over a window nothing is
                // pointing at. `Window::is_window_hovered` is a separate,
                // platform-level signal (WM_MOUSELEAVE on Windows) that
                // stays accurate even though the input event doesn't fire.
                // Under `--automation` that same accuracy is the problem:
                // the REAL cursor sits over the driving terminal, so this
                // check would discard every scripted hover — skip it there
                // (`automation::is_active`'s doc has the full story; found
                // when the S8g consolidation rerun showed hover null while
                // the pre-fix live run had verified it visually).
                #[cfg(feature = "automation")]
                let window_hovered = window.is_window_hovered() || crate::automation::is_active();
                #[cfg(not(feature = "automation"))]
                let window_hovered = window.is_window_hovered();
                if !window_hovered {
                    return;
                }
                this.hover_popover =
                    match result {
                        Ok(Some(hover)) => dv_core::lsp::hover_contents_to_text(&hover.contents)
                            .map(|markdown| crate::lsp::HoverPopover {
                                anchor,
                                line: new_line,
                                markdown,
                            }),
                        Ok(None) | Err(_) => None,
                    };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The mouse left an eligible token's row — clear the popover, but only
    /// if it's still the one THIS row raised (see `crate::lsp::HoverPopover`'s
    /// doc comment: this check is what makes the clear order-independent
    /// against a fresher hover on a different row racing in either order).
    fn on_symbol_hover_leave(&mut self, new_line: u32, cx: &mut Context<Self>) {
        // Supersede any in-flight debounced hover THIS row itself kicked off
        // — leaving before it resolves must not have it pop back up. Only
        // when the in-flight epoch is still this row's own claim, though
        // (see `hover_request_line`'s doc comment): gpui's bubble-phase
        // mouse dispatch runs in reverse paint order, so a single downward
        // `MouseMoveEvent` (hovered row -> a row painted later) fires the
        // DESTINATION row's `on_symbol_hover` — which already bumped the
        // epoch and claimed `hover_request_line` for itself — before this
        // (origin) row's own leave listener runs. Bumping unconditionally
        // here would re-supersede that fresher claim and silently drop the
        // destination row's answer; skip the bump entirely when some other
        // row already owns the current epoch.
        if self.hover_request_line == Some(new_line) {
            self.hover_request_epoch += 1;
            self.hover_request_line = None;
        }
        if self
            .hover_popover
            .as_ref()
            .is_some_and(|popover| popover.line == new_line)
        {
            self.hover_popover = None;
            cx.notify();
        }
    }

    /// `LspHandle::sync_document` (best-effort — vtsls needs the file open,
    /// and current, before it can answer `definition` at all; see that
    /// method's doc comment for why this isn't a flat `didOpen`) +
    /// `textDocument/definition`, off the UI thread. `from` is where the
    /// click originated, pushed onto `nav_stack` only on a SUCCESSFUL jump
    /// (see `Self::handle_definition_result`). `epoch` is `lsp_request_epoch`
    /// as of the click that kicked this off — re-checked on completion so a
    /// click the user has since moved on from (closed the viewer, selected
    /// another file, clicked again) can't reopen the target viewer with a
    /// stale answer (P3 finding; see `lsp_request_epoch`'s doc comment).
    ///
    /// `range_head`, when `Some`, is the `Range` view's `head` rev — this
    /// file's displayed bytes are `head`'s committed blob, but vtsls always
    /// answers about the on-disk working copy, so before asking it anything
    /// this compares the two blob shas and declines (same message as
    /// `Self::lsp_view_is_honest`'s coarser, whole-view gate) on a mismatch
    /// (P3 finding: an uncommitted local edit to this one file otherwise
    /// produced a confident wrong-symbol answer even though the view as a
    /// whole passed the honest-view gate).
    #[allow(clippy::too_many_arguments)]
    fn run_definition_request(
        &mut self,
        handle: dv_core::lsp::LspHandle,
        repo: Arc<GitRepo>,
        rel_path: String,
        uri: String,
        language_id: &'static str,
        position: lsp_types::Position,
        from: crate::lsp::Location,
        range_head: Option<String>,
        epoch: u64,
        ready_since: Option<std::time::Instant>,
        cx: &mut Context<Self>,
    ) {
        // Decremented at the top of the completion closure below,
        // unconditionally (epoch match or not) — see
        // `Self::lsp_inflight_requests`'s doc comment.
        self.lsp_inflight_requests += 1;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    if let Some(head) = &range_head
                        && !Self::range_head_matches_working(&repo, &rel_path, head)
                    {
                        return Err(dv_core::lsp::LspError::Unavailable(
                            "this file has uncommitted changes since the reviewed \
                             revision — only available on the working-tree view"
                                .to_string(),
                        ));
                    }
                    let text = repo
                        .blob_bytes(&BlobSpec::Working {
                            path: rel_path.clone(),
                        })
                        .ok()
                        .flatten()
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        .unwrap_or_default();
                    let _ = handle.sync_document(&uri, language_id, &text);
                    let mut links = handle.definition(&uri, position)?;
                    // vtsls's project load can legitimately still be in
                    // progress for several seconds after a session first
                    // reaches `Ready` (docs/phase-8-lsp-and-polish.md §
                    // LSP.1) — an EMPTY answer that early is far more likely
                    // "still indexing" than "genuinely no definition" (P2
                    // finding). Retry a handful of times, same bounded
                    // pattern the live test uses
                    // (`crates/core/tests/wsl_lsp_definition.rs`), rather
                    // than reporting "no definition found" on the very
                    // first click against a cold server. Only within
                    // `LSP_WARMUP_WINDOW` of `Ready`, so a warm session's
                    // genuinely-empty answer stays instant.
                    if links.is_empty()
                        && ready_since.is_some_and(|since| since.elapsed() < LSP_WARMUP_WINDOW)
                    {
                        for _ in 0..LSP_WARMUP_RETRIES {
                            std::thread::sleep(LSP_WARMUP_RETRY_DELAY);
                            links = handle.definition(&uri, position)?;
                            if !links.is_empty() {
                                break;
                            }
                        }
                    }
                    Ok(links)
                })
                .await;

            this.update(cx, |this, cx| {
                this.lsp_inflight_requests = this.lsp_inflight_requests.saturating_sub(1);
                if this.lsp_request_epoch != epoch {
                    return; // superseded — see this fn's doc comment
                }
                this.handle_definition_result(result, from, epoch, cx)
            })
            .ok();
        })
        .detach();
    }

    fn handle_definition_result(
        &mut self,
        result: Result<Vec<lsp_types::LocationLink>, dv_core::lsp::LspError>,
        from: crate::lsp::Location,
        epoch: u64,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(links) if !links.is_empty() => {
                let target = crate::lsp::Location::from_link(&links[0]);
                self.lsp_status = None;
                // Deferred to `open_target_at`'s success arm — see
                // `crate::lsp::NavCommit`'s doc comment — rather than
                // pushed here, so a target read that fails doesn't leave
                // `nav_stack` claiming a jump happened that never actually
                // displayed (P3 finding).
                self.open_target_at(target, Some(crate::lsp::NavCommit::Push(from)), epoch, cx);
            }
            Ok(_) => {
                self.lsp_status = Some("no definition found".into());
                cx.notify();
            }
            Err(err) => {
                self.lsp_status = Some(format!("go-to-definition failed: {err}").into());
                cx.notify();
            }
        }
    }

    /// Open (or re-point) the read-only target viewer at `target`, reading
    /// its current worktree content via the same `BlobSpec::Working` path
    /// the diff pane's own `WorkingTree` side uses (`compute_diff`'s
    /// `new_spec` arm) — honest by construction, since that's the same
    /// on-disk bytes vtsls itself just read.
    ///
    /// `nav` describes the `nav_stack` mutation the CALLER wants applied —
    /// a fresh jump's `Push`, or the `Back`/`Forward` step a `NavBack`/
    /// `NavForward` action already peeked at — and is only actually applied
    /// in the success arm below, once the viewer has really updated. Every
    /// early-out above that (an unrecognized URI, a non-WSL location, a
    /// target outside the repo) and the async failure arms (`Ok(None)`/
    /// `Err`) simply drop it, leaving `nav_stack` exactly as it was (P3
    /// finding: applying the mutation at the call site desynced history
    /// from a target read that then failed).
    ///
    /// `epoch` is `lsp_request_epoch` as of the moment the caller decided to
    /// navigate here; re-checked once the blob read actually completes and
    /// discarded silently on a mismatch. This is what makes two rapid
    /// `NavBack`/`NavForward` presses (or a click racing a nav, or either
    /// racing a file switch/viewer close) commit at most once instead of
    /// desyncing `nav_stack`/`target_viewer` from what's actually on screen
    /// (P3 finding — see `lsp_request_epoch`'s doc comment).
    fn open_target_at(
        &mut self,
        target: crate::lsp::Location,
        nav: Option<crate::lsp::NavCommit>,
        epoch: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(posix_path) = dv_core::lsp::path_from_file_uri(&target.uri) else {
            self.lsp_status = Some("go-to-definition: unrecognized target URI".into());
            cx.notify();
            return;
        };
        let RepoLocation::Wsl {
            path: root_path, ..
        } = &self.location
        else {
            return;
        };
        let root_prefix = format!("{}/", root_path.trim_end_matches('/'));
        let Some(rel_path) = posix_path.strip_prefix(&root_prefix) else {
            self.lsp_status = Some(
                "go-to-definition: target is outside this repo (e.g. a bundled library file) \
                 — not shown"
                    .into(),
            );
            cx.notify();
            return;
        };
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let rel_path = rel_path.to_string();
        let highlight_line = target.line;

        // Decremented at the top of the completion closure below,
        // unconditionally (epoch match or not) — see
        // `Self::lsp_inflight_requests`'s doc comment.
        self.lsp_inflight_requests += 1;
        cx.spawn(async move |this, cx| {
            let rel_path_for_read = rel_path.clone();
            let content = cx
                .background_spawn(async move {
                    repo.blob_bytes(&BlobSpec::Working {
                        path: rel_path_for_read,
                    })
                })
                .await;

            this.update(cx, |this, cx| {
                this.lsp_inflight_requests = this.lsp_inflight_requests.saturating_sub(1);
                if this.lsp_request_epoch != epoch {
                    return; // superseded — see this fn's doc comment
                }
                match content {
                    Ok(Some(bytes)) => {
                        let text = String::from_utf8_lossy(&bytes).into_owned();
                        let mut lines: Vec<SharedString> =
                            text.lines().map(|l| l.to_string().into()).collect();
                        // Captured BEFORE the truncation-notice row is
                        // pushed below, so the header reports the file's
                        // real size rather than one-more-for-the-notice (P3
                        // finding).
                        let total_lines = lines.len();
                        let truncated = total_lines > MAX_TARGET_VIEWER_LINES;
                        if truncated {
                            lines.truncate(MAX_TARGET_VIEWER_LINES);
                            lines.push("… (file truncated)".into());
                        }
                        // A target beyond the cap (a huge generated `.d.ts`,
                        // say) would otherwise have `scroll_to_item` clamp
                        // silently to the last row with no highlighted line
                        // and no explanation (P3 finding) — surface it
                        // instead, and skip the scroll attempt entirely
                        // (`scrolled_to_highlight: true` up front) rather
                        // than land on a bottom that isn't actually the
                        // target.
                        let beyond_cap =
                            truncated && highlight_line as usize >= MAX_TARGET_VIEWER_LINES;
                        this.target_viewer = Some(TargetViewer {
                            path: rel_path,
                            lines,
                            total_lines,
                            highlight_line,
                            scroll: UniformListScrollHandle::new(),
                            scrolled_to_highlight: beyond_cap,
                        });
                        this.lsp_status = if beyond_cap {
                            Some(
                                format!(
                                    "target line {} is beyond the {MAX_TARGET_VIEWER_LINES}-line \
                                     display cap",
                                    highlight_line + 1
                                )
                                .into(),
                            )
                        } else {
                            None
                        };
                        match nav {
                            Some(crate::lsp::NavCommit::Push(from)) => {
                                this.nav_stack.push(from);
                            }
                            Some(crate::lsp::NavCommit::Back(current)) => {
                                this.nav_stack.go_back(current);
                            }
                            Some(crate::lsp::NavCommit::Forward(current)) => {
                                this.nav_stack.go_forward(current);
                            }
                            None => {}
                        }
                    }
                    Ok(None) => {
                        this.lsp_status =
                            Some("go-to-definition: target file not found on disk".into());
                    }
                    Err(err) => {
                        this.lsp_status = Some(
                            format!("go-to-definition: reading target failed: {err:#}").into(),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The target viewer's OWN current location, if it's open — the "where
    /// am I now" `Self::on_nav_back`/`Self::on_nav_forward` file onto the
    /// opposite stack. `None` (a no-op nav) while the viewer is closed:
    /// back/forward navigate within definition-jump history, which only
    /// exists once a jump has actually opened the viewer.
    fn current_nav_location(&self) -> Option<crate::lsp::Location> {
        let viewer = self.target_viewer.as_ref()?;
        let RepoLocation::Wsl {
            path: root_path, ..
        } = &self.location
        else {
            return None;
        };
        let uri = dv_core::lsp::file_uri(&format!(
            "{}/{}",
            root_path.trim_end_matches('/'),
            viewer.path
        ));
        Some(crate::lsp::Location {
            uri,
            line: viewer.highlight_line,
            character: 0,
        })
    }

    fn on_nav_back(&mut self, _: &NavBack, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(current) = self.current_nav_location() else {
            return;
        };
        // `peek_back` only READS the back stack — the actual pop (moving
        // `current` onto forward) happens in `open_target_at`'s success arm
        // via the `NavCommit::Back` it's handed here, so a target read that
        // fails never desyncs `nav_stack` from what's on screen. Bumping the
        // epoch here (before the peek/read) is what makes a second rapid
        // `NavBack` press — which would otherwise peek the same back-stack
        // entry a first press hasn't committed yet — supersede the first
        // instead of both completions committing a pop (P3 finding; see
        // `lsp_request_epoch`'s doc comment).
        self.lsp_request_epoch += 1;
        let epoch = self.lsp_request_epoch;
        if let Some(target) = self.nav_stack.peek_back() {
            self.open_target_at(
                target,
                Some(crate::lsp::NavCommit::Back(current)),
                epoch,
                cx,
            );
        }
    }

    fn on_nav_forward(&mut self, _: &NavForward, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(current) = self.current_nav_location() else {
            return;
        };
        // See `Self::on_nav_back`'s comment on the epoch bump.
        self.lsp_request_epoch += 1;
        let epoch = self.lsp_request_epoch;
        if let Some(target) = self.nav_stack.peek_forward() {
            self.open_target_at(
                target,
                Some(crate::lsp::NavCommit::Forward(current)),
                epoch,
                cx,
            );
        }
    }

    fn on_close_target_viewer(
        &mut self,
        _: &CloseTargetViewer,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.target_viewer = None;
        // Closing the viewer supersedes any in-flight click/nav that would
        // otherwise reopen it later with a stale target (P3 finding; see
        // `lsp_request_epoch`'s doc comment).
        self.lsp_request_epoch += 1;
        cx.notify();
    }

    /// Tear down this workspace's LSP session when it's about to be
    /// PARKED (`AppShell::stash_active`, Phase-7 S7-3's workspace-LRU
    /// cache) rather than actually closed. The LRU deliberately keeps a
    /// parked `Workspace` entity alive for instant reactivation — which
    /// means its `lsp_session` field, and the only long-lived
    /// [`dv_core::lsp::LspHandle`] clone it holds, would otherwise live for
    /// the rest of the session too, leaking a vtsls `node` process in WSL
    /// for every distinct WSL TS review the user has ever ctrl-clicked in
    /// (P3 finding — parking is neither close nor eviction, and vtsls
    /// memory isn't counted against the LRU's own byte cap). Resets to
    /// `Unattempted` immediately so reactivation — a fresh ctrl-click —
    /// lazily respawns exactly as a brand-new workspace would; the spawn is
    /// already lazy and self-healing, so parking loses nothing but the warm
    /// process.
    ///
    /// The graceful `shutdown()` handshake (a bounded request/response round
    /// trip) and the drop of the last `LspHandle` clone it triggers (which
    /// runs `LspClient`'s own `Drop` — a SECOND shutdown round trip plus an
    /// up-to-`SHUTDOWN_GRACE` wait for the child to exit) are both handed to
    /// `cx.background_spawn` rather than run inline: this fn runs on the UI
    /// thread on every review switch (`AppShell::stash_active`, Phase-7's
    /// ~2ms hot path) and `Drop`'s own wait alone can cost up to a second
    /// (worse if vtsls is wedged) — a P2 finding this was the fix for.
    pub(crate) fn park_lsp_session(&mut self, cx: &mut Context<Self>) {
        let previous = std::mem::replace(
            &mut self.lsp_session,
            crate::lsp::LspSessionState::Unattempted,
        );
        // A pending click and the node_modules warning both belong to the
        // session being torn down here — a fresh reactivation respawns (and,
        // if relevant, re-detects node_modules) from scratch, so carrying
        // either forward would only ever be stale.
        self.lsp_pending_definition = None;
        self.lsp_node_modules_warning = None;
        self.lsp_ready_since = None;
        // The S8g hover popover (and any debounced request still chasing an
        // answer) belongs to the session being torn down here too — hover
        // never outlives the `Ready` handle it was answered against.
        self.hover_popover = None;
        self.hover_request_epoch += 1;
        self.hover_request_line = None;
        // Invalidate any spawn attempt still in flight from before this
        // park — without this, a stale attempt completing after parking
        // (but before any reactivation click starts a new one) would still
        // see its captured generation match and could install `Ready`/
        // `Unavailable` on a session nobody's looking at anymore (same
        // rationale as `lsp_spawn_generation`'s own doc comment).
        self.lsp_spawn_generation += 1;
        if let crate::lsp::LspSessionState::Ready(handle) = previous {
            cx.background_spawn(async move {
                handle.shutdown();
                // `handle` (and, once every other clone is gone, the
                // `LspClient` it wraps) drops here, off the UI thread.
            })
            .detach();
        }
    }

    /// One line of the target viewer: a fixed-width line-number gutter +
    /// the raw (unhighlighted — see `TargetViewer`'s doc comment) text,
    /// with the jumped-to line tinted.
    fn render_target_line(&self, idx: usize, cx: &mut Context<Self>) -> Div {
        let Some(viewer) = self.target_viewer.as_ref() else {
            return div();
        };
        let theme = cx.theme();
        let Some(text) = viewer.lines.get(idx) else {
            return div();
        };
        let highlighted = idx as u32 == viewer.highlight_line;
        h_flex()
            .w_full()
            .font_family(theme.mono_font_family.clone())
            .text_size(px(self.font_size))
            .when(highlighted, |el| el.bg(theme.primary.opacity(0.18)))
            .child(
                div()
                    .w(px(gutter_width(self.font_size) * 1.4))
                    .flex_none()
                    .pr_2()
                    .text_right()
                    .text_color(theme.muted_foreground.opacity(0.8))
                    .child((idx + 1).to_string()),
            )
            .child(div().whitespace_nowrap().child(text.clone()))
    }

    /// The hover popover as it should actually be SHOWN right now — `None`
    /// while any modal overlay (target viewer, palette, PR picker) is open,
    /// even if `self.hover_popover` itself still holds a value.
    ///
    /// Live-verification finding (S8g): `Cmd::Click`'s own "move first so
    /// hover state matches what a real click sees" (a real physical click
    /// does the same — the cursor arrives at the spot before the button
    /// goes down) means EVERY ctrl/cmd-click also fires this exact hover
    /// path for the very same token, immediately before the click opens the
    /// target-viewer modal on top of it. Without this gate, the stale
    /// popover from that incidental hover kept rendering ON TOP of the
    /// modal (both are children of the same workspace root, and the
    /// popover — added after the target viewer in `Self::render`'s child
    /// order — painted over it) instead of being covered by it. Gating at
    /// render time, rather than hunting every call site that opens an
    /// overlay, is what makes this correct regardless of which overlay:
    /// `self.hover_popover` is left alone as a plain "last hover answer"
    /// cache that a later render (once the overlay closes, if the mouse is
    /// still over that same token) can still show.
    fn hover_popover_visible(&self) -> Option<&crate::lsp::HoverPopover> {
        if self.target_viewer.is_some() || self.palette.is_some() || self.pr_picker.is_some() {
            return None;
        }
        self.hover_popover.as_ref()
    }

    /// The S8g hover popover — a small, non-modal floating card (unlike the
    /// target viewer's centered backdrop-modal) positioned near the token
    /// that raised it. `HoverPopover::anchor` is a WINDOW-relative point (the
    /// only kind a raw `MouseMoveEvent` carries); `self.root_bounds` (kept
    /// current by a `canvas` probe in `Self::render`) is this workspace's own
    /// root element's WINDOW-relative bounds, the nearest positioned
    /// ancestor an `.absolute()` child here resolves against — subtracting
    /// its origin converts the anchor into a position relative to that root.
    /// `.occlude()` (not `.on_mouse_down` capture) so the popover doesn't
    /// eat clicks meant for the diff underneath it while still not itself
    /// stealing keyboard focus (this slice's gpui gotcha: position-only
    /// overlay, no focus grab).
    fn render_hover_popover(&self, cx: &mut Context<Self>) -> Option<Div> {
        // Which edge the popover is anchored from — see the flip-above
        // branch below for why this can't just always be `Top`.
        enum VerticalOffset {
            Top(Pixels),
            Bottom(Pixels),
        }

        let popover_state = self.hover_popover_visible()?;
        let theme = cx.theme();
        let root_bounds = self.root_bounds.get();
        let root_origin = root_bounds
            .map(|b| b.origin)
            .unwrap_or_else(|| point(px(0.), px(0.)));
        const POPOVER_WIDTH: Pixels = px(480.);
        const POPOVER_MAX_HEIGHT: Pixels = px(280.);
        let mut left = (popover_state.anchor.x - root_origin.x).max(px(0.));
        // Clamp to the root's own right edge (live-verification finding:
        // hovering a token near the window's right side otherwise left the
        // popover's own right portion rendered off-window and unreadable,
        // since `left` alone never accounted for the box's width).
        if let Some(root_width) = root_bounds.map(|b| b.size.width) {
            left = left.min((root_width - POPOVER_WIDTH).max(px(0.)));
        }
        // A little below and right of the cursor, same convention as an OS
        // tooltip — sitting exactly under the pointer would have the
        // pointer itself obscure the popover's own top-left corner.
        let anchor_y = popover_state.anchor.y - root_origin.y;
        let top = anchor_y + px(20.);
        // Mirror the horizontal clamp above, but flip ABOVE the anchor
        // instead of just clamping — clamping `top` down to fit would slide
        // the card up so it no longer points at the hovered token at all.
        // (P3 finding: hovering a token on the bottom rows otherwise
        // rendered the popover partially or entirely below the window, the
        // same defect class the horizontal clamp fixed, left unhandled
        // vertically.)
        //
        // The flip anchors the popover's BOTTOM edge to the token instead of
        // its top (`.bottom(..)` instead of `.top(..)` — both set gpui's
        // `inset`, so only one of the two is ever applied). Anchoring `top`
        // at a fixed `POPOVER_MAX_HEIGHT` offset (P3 finding, corrected)
        // assumed the card always rendered at that max height, but `max_h`
        // only caps it — a typical short 1-3 line vtsls answer shrinks to
        // content, leaving its real bottom edge (and thus its visual
        // connection to the hovered token) up to `POPOVER_MAX_HEIGHT` away
        // from the cursor. Anchoring the bottom edge instead keeps the card
        // adjacent to the token regardless of how tall the content actually
        // renders.
        let vertical_offset = match root_bounds.map(|b| b.size.height) {
            Some(root_height) if top + POPOVER_MAX_HEIGHT > root_height => {
                VerticalOffset::Bottom((root_height - anchor_y + px(4.)).max(px(0.)))
            }
            _ => VerticalOffset::Top(top),
        };

        let popover = div()
            .absolute()
            .left(left)
            .occlude()
            .max_w(POPOVER_WIDTH)
            .max_h(POPOVER_MAX_HEIGHT)
            .overflow_hidden()
            .p_2()
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_lg()
            .text_xs()
            .font_family(theme.mono_font_family.clone())
            .child(
                div()
                    .whitespace_normal()
                    .child(popover_state.markdown.clone()),
            );
        Some(match vertical_offset {
            VerticalOffset::Top(top) => popover.top(top),
            VerticalOffset::Bottom(bottom) => popover.bottom(bottom),
        })
    }

    /// The S8f read-only target-viewer overlay — same modal backdrop
    /// pattern as `AppShell::render_onboarding`/`render_settings_panel`:
    /// `inset_0().occlude()` plus a click-to-close backdrop, with
    /// `stop_propagation()` on the card itself.
    fn render_target_viewer(&mut self, cx: &mut Context<Self>) -> Option<Div> {
        use gpui_component::Sizable as _;
        use gpui_component::button::{Button, ButtonVariants as _};

        let viewer = self.target_viewer.as_mut()?;
        if !viewer.scrolled_to_highlight {
            viewer
                .scroll
                .scroll_to_item(viewer.highlight_line as usize, ScrollStrategy::Center);
            // Only ever fire the scroll-into-view once per open target —
            // flipped immediately (this method takes `&mut self`, unlike
            // the sibling `render_palette`/`render_pr_picker`, specifically
            // so this can be a plain field write instead of a `cx.spawn`
            // one-shot) so it never fights the user's own manual scroll on
            // later renders of the same viewer.
            viewer.scrolled_to_highlight = true;
        }
        let viewer = self.target_viewer.as_ref()?;
        let theme = cx.theme();
        let border = theme.border;
        let popover = theme.popover;
        let popover_fg = theme.popover_foreground;
        let muted = theme.muted_foreground;
        // The list's own row count (includes the truncation-notice row when
        // present) vs. the header's — see `TargetViewer::total_lines`'s doc
        // comment for why these two differ (P3 finding).
        let line_count = viewer.lines.len();
        let header_line_count = viewer.total_lines;

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.background.opacity(0.6))
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.on_close_target_viewer(&CloseTargetViewer, window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    v_flex()
                        .id("target-viewer")
                        .w(px(880.))
                        .max_w_full()
                        .h(px(640.))
                        .max_h_full()
                        .overflow_hidden()
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
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(border)
                                .child(
                                    div()
                                        .min_w(px(0.))
                                        .flex_1()
                                        .truncate()
                                        .text_sm()
                                        .font_semibold()
                                        .child(viewer.path.clone()),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            Button::new("target-viewer-back")
                                                .ghost()
                                                .xsmall()
                                                .label("< back")
                                                .disabled(!self.nav_stack.can_go_back())
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.on_nav_back(&NavBack, window, cx);
                                                })),
                                        )
                                        .child(
                                            Button::new("target-viewer-forward")
                                                .ghost()
                                                .xsmall()
                                                .label("forward >")
                                                .disabled(!self.nav_stack.can_go_forward())
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.on_nav_forward(&NavForward, window, cx);
                                                })),
                                        )
                                        .child(
                                            Button::new("target-viewer-close")
                                                .ghost()
                                                .xsmall()
                                                .label("Close")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.on_close_target_viewer(
                                                        &CloseTargetViewer,
                                                        window,
                                                        cx,
                                                    );
                                                })),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .px_2()
                                .pt_1()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("{header_line_count} lines · read-only")),
                        )
                        .child(
                            div().flex_1().min_h(px(0.)).child(
                                uniform_list(
                                    "target-viewer-lines",
                                    line_count,
                                    cx.processor(|this, range: Range<usize>, _, cx| {
                                        range
                                            .map(|i| this.render_target_line(i, cx))
                                            .collect::<Vec<_>>()
                                    }),
                                )
                                .track_scroll(&viewer.scroll)
                                .size_full(),
                            ),
                        ),
                ),
        )
    }

    // ---- Jump-to-file palette ----------------------------------------

    fn on_jump_to_file(&mut self, _: &JumpToFile, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::{InputEvent, InputState};
        if self.files.is_empty() {
            return;
        }
        // Decline while another overlay already has the screen (review
        // finding P3). The `browse` key-context predicate normally keeps
        // this action from firing while the PR picker/comment editor/
        // target viewer/shell overlays are up, but the macOS menu bar (S8i)
        // dispatches straight to this handler with no key-context gate at
        // all — menu dispatch bypasses the keymap entirely, same as every
        // other `on_*` handler here, so the guard has to live in the
        // handler itself. Mirrors `on_open_pr_picker`'s decline set: its
        // own overlay, the comment editor/thread-reply input (same pair
        // `render`'s `key_context` computation treats as "editor open"),
        // the S8f target viewer (phase-8 capstone review, P3 — the viewer
        // renders after the palette in `render`'s child order, so opening
        // this behind it would be invisible), and the shell-level overlays
        // via `overlay_open`.
        if self.pr_picker.is_some()
            || self.editor.is_some()
            || self.thread_input.is_some()
            || self.target_viewer.is_some()
            || self
                .shell
                .upgrade()
                .is_some_and(|shell| shell.read(cx).overlay_open())
        {
            return;
        }
        if let Some(palette) = &self.palette {
            // Already open — just refocus the input.
            let input = palette.input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }

        // Supersede any in-flight go-to-definition round trip (whole-phase
        // capstone review, P3 — see `on_open_pr_picker`'s matching bump and
        // `lsp_request_epoch`'s doc comment): otherwise a click stashed
        // while `lsp_session` is `Spawning` could complete after this
        // palette opens and commit the target viewer on top of it.
        self.lsp_request_epoch += 1;
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
    /// The shared modal-picker recipe (R2 item 6)
    /// — see `AppShell::render_theme_picker`'s
    /// doc comment for the full spec, with one scope caveat: this overlay
    /// lives in the *workspace's* render tree, so its `inset_0()` scrim
    /// dims the workspace pane only (the shell-owned theme picker dims the
    /// whole window). This one also keeps its blur-closes semantics: a
    /// backdrop click blurs the input too, so both paths funnel into
    /// `close_palette` (idempotent).
    fn render_palette(&self, cx: &mut Context<Self>) -> Option<Div> {
        use gpui_component::Sizable as _;
        const VISIBLE: usize = 12;
        let palette = self.palette.as_ref()?;
        let theme = cx.theme();
        let dv = crate::themes::dv_theme(cx);
        let surface_active = dv.surface_active;

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
                    .h(px(30.))
                    .mx_1()
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |el| el.bg(surface_active))
                    .hover(|el| el.bg(surface_active.opacity(0.5)))
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
                            .w_full()
                            .truncate()
                            .child(self.files[file_ix].path.clone()),
                    )
            })
            .collect();

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(dv.backdrop)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.close_palette(window, cx);
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
                                .p_2()
                                .gap_2()
                                // Swallow clicks on the popover chrome (padding,
                                // gaps): otherwise they bubble to the workspace
                                // root's focus-on-mousedown, blurring the input and
                                // closing the palette out from under the user.
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .bg(theme.sidebar)
                                .text_color(theme.foreground)
                                .text_size(px(13.))
                                .border_1()
                                .border_color(theme.muted)
                                .rounded_lg()
                                .shadow_lg()
                                .child(gpui_component::input::Input::new(&palette.input).small())
                                .child(v_flex().w_full().children(rows).when(
                                    palette.matches.is_empty(),
                                    |el| {
                                        el.child(
                                            div()
                                                .px_2()
                                                .py_1()
                                                .text_color(theme.muted_foreground)
                                                .child("no matching files"),
                                        )
                                    },
                                )),
                        ),
                ),
        )
    }

    /// The workspace's title-bar-anatomy header (R1c title-bar spec)
    /// — the single row above the diff area that replaces the
    /// old two-tier header (a generic per-review strip always shown, plus a
    /// PR-only band underneath when a PR was open). One row now, in either
    /// of two shapes:
    ///
    /// - **PR variant**: state pill (open/merged/closed/draft) → bold
    ///   truncating PR title → `#N`/`by <author>` in `text_secondary` →
    ///   optional approved/changes-requested decision pill (`review
    ///   required` is intentionally dropped here — the title bar only
    ///   surfaces a *resolved* decision by design; the sidebar's
    ///   `render_pr_glyphs` still shows all three, unchanged) → right
    ///   cluster: `base ← head` in `muted.foreground`, `+N`/`−N` diffstat
    ///   (see `Self::diffstat`'s doc comment for the scope limit), LSP
    ///   chip(s), a "details" toggle (dv-only — keeps the existing
    ///   PR-body-expansion feature the reference anatomy has no analogue
    ///   for), an external-link ghost button, then the three real action
    ///   buttons (PR picker / split / review toggle) this bar has always
    ///   carried.
    /// - **Local variant**: a `primary`-colored "local" `Tag` + bold repo
    ///   dirname (`dv_core::repo_label`, no PR) on the left; the same right
    ///   cluster minus the PR-only pieces (decision pill, details, external
    ///   link) — `base ← head` becomes dv's own `source_desc`/`head` pair
    ///   (dv's `DiffSource` has no base/head ref pair for a plain working-
    ///   tree or commit diff).
    /// - **PR-linked-but-not-`open_pr`'d variant**: `self.pr` is only
    ///   populated by [`Self::open_pr`]'s own fetch — a sidebar card click
    ///   (`AppShell::open_review_row` passes `pending_pr: None`), a
    ///   submitted GitHub review reopened later, or the transient window
    ///   while a background `dv pr <n>` fetch is still in flight all leave
    ///   `self.pr` `None` even though the review genuinely has a remote
    ///   (`self.pr_remote`, stamped from `review.remote` at load time). The
    ///   `local` tag would misstate these (review finding P2-1), so this
    ///   case gets its own muted/neutral "PR" pill + bold `owner/repo#N`
    ///   (`dv_core::repo_label(location, self.pr_remote)`) instead of either
    ///   the PR variant (no `PrHeader` to source state/title/author from
    ///   yet) or the `local` tag.
    ///
    /// `pr_error`/`source_switch_error` (surfaced failures) are kept as
    /// their own capped, truncating segments — dropping them would be a
    /// functional regression, not a restyle.
    fn render_header(&self, cx: &mut Context<Self>) -> Div {
        use crate::shell::{pr_state_pill, state_pill};
        use gpui_component::IconName;
        use gpui_component::Selectable as _;
        use gpui_component::Sizable as _;
        use gpui_component::button::{Button, ButtonVariants as _};

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
        let primary = theme.primary;
        let dv = crate::themes::dv_theme(cx);
        let text_secondary = dv.text_secondary;
        let accent_alt = dv.accent_alt;

        let mode = self.view_mode;
        let summary_open = self.summary_open;
        let details_open = self.pr_details_open;
        let (added, removed) = self.diffstat();
        // `self.diffs` loads lazily, per file visited (see `Self::diffstat`'s
        // doc comment) — `diffstat_known` alone can't distinguish "this is
        // the whole review's total" from "only the files opened so far",
        // and rendering the partial sum in the exact spot readers expect a
        // whole-PR total misinforms anyone who hasn't visited every file
        // (review finding P2-1). `diffstat_partial` drives a visible
        // "· n/total files" qualifier below so the number is never silently
        // mistaken for the total. `Self::files_loaded` (not a raw
        // `self.diffs.len()`) excludes `error_diff` placeholders, so a
        // failed file keeps this qualifier showing even after every file
        // has been *visited* (review finding P3-1).
        let files_loaded = self.files_loaded();
        let files_total = self.files.len();
        let diffstat_known = files_loaded > 0;
        let diffstat_partial = diffstat_known && files_loaded < files_total;

        let left: AnyElement = match &self.pr {
            Some(pr) => {
                let (chip_label, chip_color) =
                    pr_state_pill(pr.is_draft, pr.state, muted, success, danger, accent_alt);
                let decision = pr.review_decision.and_then(|d| match d {
                    ReviewDecision::Approved => Some(("approved", success)),
                    ReviewDecision::ChangesRequested => Some(("changes requested", danger)),
                    ReviewDecision::ReviewRequired => None,
                });
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .items_center()
                    .gap_2()
                    .child(state_pill(chip_color, chip_label).flex_none())
                    .child(
                        div()
                            .flex_shrink_1()
                            .flex_grow_0()
                            .min_w(px(0.))
                            .font_semibold()
                            .truncate()
                            .child(pr.title.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(text_secondary)
                            .child(format!("#{}", pr.number)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(text_secondary)
                            .child(format!("by {}", pr.author)),
                    )
                    .children(decision.map(|(label, color)| state_pill(color, label).flex_none()))
                    .into_any_element()
            }
            // `self.pr` is `None` but the review is still remote-linked —
            // render a PR-shaped (not `local`) left cluster (review finding
            // P2-1; see this fn's doc comment's third bullet).
            None if self.pr_remote.is_some() => h_flex()
                .flex_1()
                .min_w(px(0.))
                .items_center()
                .gap_2()
                .child(state_pill(muted, "PR").flex_none())
                .child(
                    div()
                        .flex_shrink_1()
                        .flex_grow_0()
                        .min_w(px(0.))
                        .font_semibold()
                        .truncate()
                        .child(repo_label(self.location(), self.pr_remote.as_ref())),
                )
                .into_any_element(),
            None => h_flex()
                .flex_1()
                .min_w(px(0.))
                .items_center()
                .gap_2()
                .child(state_pill(primary, "local").flex_none())
                .child(
                    div()
                        .flex_shrink_1()
                        .flex_grow_0()
                        .min_w(px(0.))
                        .font_semibold()
                        .truncate()
                        .child(repo_label(self.location(), None)),
                )
                .into_any_element(),
        };

        // Right cluster: refs, diffstat, LSP chip(s), then the action
        // buttons. Split into two groups (review finding P3-4): `info`
        // (refs/diffstat/LSP chips) is allowed to shrink — its own children
        // are bounded (`max_w` + `.truncate()`) and individually shrinkable
        // — while `actions` (the only *interactive* elements in this
        // cluster: PR-picker/split/review-toggle, plus the PR-only details/
        // external-link buttons) stays `flex_none` so it's never the side
        // that gives. Previously everything lived in one `flex_none`
        // cluster: an unbounded refs label alone, or an LSP chip/warning
        // that couldn't compress, could together exceed a narrow (1280px)
        // window's width with nothing able to yield, pushing the action
        // buttons off-screen and out of mouse reach.
        let refs_label: Option<String> = match &self.pr {
            Some(pr) => Some(format!("{} \u{2190} {}", pr.base_ref, pr.head_ref)),
            // Base (the branch HEAD) on the left, the reviewed side on the
            // right — same convention as the PR arm above (`base ← head`),
            // not reversed (review finding P3-3: this used to render
            // `{source_desc} ← {head}`, e.g. "working tree ← main", which
            // reads as main's changes flowing into the working tree —
            // backwards from what's actually being diffed).
            //
            // The arrow only holds for `WorkingTree`/`Staged`, where `head`
            // (the checked-out branch/commit label) genuinely *is* the base
            // being diffed against. For `Range`/`Commit` sources, `head` is
            // just whatever happens to be checked out right now — unrelated
            // to the diff's actual base — so asserting an arrow between them
            // would claim a relationship that isn't there (review finding
            // adversarial-P3: a Range review read as "current-branch ←
            // range", falsely implying the range diffs against the current
            // checkout). Fall back to the plain source description — for
            // `Range`/`Commit` that's only the generic `source_label` word
            // ("range" / "range (merge base)" / "commit"), not the actual
            // endpoints; `DiffSource::Range` carries `base`/`head` fields
            // that could render a real pair here, but that's future work,
            // not this fix.
            None if !self.head.is_empty()
                && matches!(self.source, DiffSource::WorkingTree | DiffSource::Staged) =>
            {
                Some(format!("{} \u{2190} {}", self.head, self.source_desc))
            }
            None if !self.source_desc.is_empty() => Some(self.source_desc.to_string()),
            None => None,
        };
        // An out-of-diff comment jump (`Self::switch_to_comment_source`)
        // repoints `source`/`source_desc` at the review's recorded range
        // without clearing `self.pr` — so the PR arm above would otherwise
        // keep showing the live PR's `base ← head` while the rows on screen
        // are the older recorded range (review finding P2-2). `open_pr`
        // always stamps `source_desc` as `"PR #{number}"` on success, so a
        // mismatch here means a jump has since repointed the source.
        let viewing_label = match &self.pr {
            Some(_) if !self.source_desc.starts_with("PR ") => {
                Some(format!("viewing: {}", self.source_desc))
            }
            _ => None,
        };

        let mut info = h_flex()
            .flex_shrink_1()
            .flex_grow_0()
            .min_w(px(0.))
            .items_center()
            .gap_2();
        info = info.children(refs_label.map(|label| {
            div()
                .flex_shrink_1()
                .flex_grow_0()
                .min_w(px(0.))
                .max_w(px(260.))
                .truncate()
                .text_color(muted)
                .child(label)
        }));
        info = info.children(viewing_label.map(|label| {
            div()
                .flex_shrink_1()
                .flex_grow_0()
                .min_w(px(0.))
                .max_w(px(200.))
                .truncate()
                .text_color(muted)
                .child(label)
        }));
        if diffstat_known {
            info = info
                .child(
                    div()
                        .flex_none()
                        .text_color(success)
                        .child(format!("+{added}")),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(danger)
                        .child(format!("\u{2212}{removed}")),
                )
                .when(diffstat_partial, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_color(muted)
                            .child(format!("\u{00b7} {files_loaded}/{files_total} files")),
                    )
                });
        }
        info = info
            .children(
                self.lsp_status
                    .clone()
                    .map(|status| lsp_chip(status, muted)),
            )
            .children(
                self.lsp_node_modules_warning
                    .clone()
                    .map(|warning_text| lsp_chip(warning_text, warning)),
            );

        let mut actions = h_flex().flex_none().items_center().gap_2();
        if let Some(pr) = &self.pr {
            let url = pr.url.clone();
            actions = actions
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
                )
                .child(
                    Button::new("pr-open-in-browser")
                        .icon(IconName::ExternalLink)
                        .ghost()
                        .xsmall()
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                );
        }
        actions = actions
            .child(
                Button::new("pr-picker-hint")
                    .ghost()
                    .small()
                    .label("PRs · ctrl-g")
                    .on_click(cx.listener(|this, _, window, cx| {
                        // Same guard the `ctrl-g` keybinding gets for free
                        // from its `!EditorOpen` key context: a click
                        // mustn't steal focus from a focused comment/thread
                        // input — the picker would open but sit
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
                    .selected(summary_open)
                    .label("review · r")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_toggle_summary(&ToggleSummary, window, cx)
                    })),
            );

        let body_text = self.pr.as_ref().map(|pr| pr.body.clone());
        let url_text = self.pr.as_ref().map(|pr| pr.url.clone());

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
                    .child(left)
                    .when_some(self.pr_error.clone(), |el, err| {
                        el.child(
                            div()
                                .flex_none()
                                .max_w(px(200.))
                                .text_sm()
                                .text_color(danger)
                                .truncate()
                                .child(format!("PR: {err}")),
                        )
                    })
                    .when_some(self.source_switch_error.clone(), |el, err| {
                        el.child(
                            div()
                                .flex_none()
                                .max_w(px(200.))
                                .text_sm()
                                .text_color(danger)
                                .truncate()
                                .child(format!("Jump: {err}")),
                        )
                    })
                    .child(info)
                    .child(actions),
            )
            .when(details_open, |el| {
                let Some(body_text) = body_text else {
                    return el;
                };
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
                        .children(url_text.map(|url| div().text_xs().text_color(muted).child(url))),
                )
            })
    }

    /// The PR picker overlay (`ctrl-g`), when open: same positioning/chrome
    /// as [`Self::render_palette`], but no text input — a background `gh pr
    /// list` call and up/down/enter/escape over whatever it returns.
    /// The shared modal-picker recipe (R2 item 6)
    /// — see `AppShell::render_theme_picker`'s
    /// doc comment for the full spec, and `render_palette`'s for the
    /// workspace-pane scrim-scope caveat both workspace pickers share.
    fn render_pr_picker(&self, cx: &mut Context<Self>) -> Option<Div> {
        use crate::shell::state_pill;

        let picker = self.pr_picker.as_ref()?;

        let theme = cx.theme();
        let seam = theme.muted;
        let panel_bg = theme.sidebar;
        let fg = theme.foreground;
        let muted = theme.muted_foreground;
        let danger = theme.danger;
        let mono = theme.mono_font_family.clone();
        let dv = crate::themes::dv_theme(cx);
        let backdrop = dv.backdrop;
        let surface_active = dv.surface_active;

        // Phase 7 D2 freshness hint: a warning if the background revalidation
        // most recently failed (stale list stays on screen either way — see
        // the `Err` arm in `on_open_pr_picker`), else "refreshing…" while one
        // is in flight, else "updated Ns/Nm ago" for whatever's currently
        // shown. `None` only for a first-ever (uncached) `Loading` open,
        // which has nothing to date yet.
        let freshness: Option<SharedString> = if let Some(err) = &picker.refresh_error {
            // Cap to a single line and a small width: `refresh_error` is the
            // raw `gh` failure string, which can be multi-line/multi-KB
            // (crates/core/src/github/client.rs truncates it only at 2000
            // bytes, not to one line). Left uncapped, it squishes/wraps the
            // "esc to close" label next to it in this fixed-width header
            // (review finding). The untruncated error is still available
            // verbatim via `--automation`'s `refresh_error` field.
            const MAX_CHARS: usize = 60;
            let first_line = err.lines().next().unwrap_or("");
            let capped: SharedString = if first_line.chars().count() > MAX_CHARS {
                format!(
                    "{}…",
                    first_line.chars().take(MAX_CHARS).collect::<String>()
                )
                .into()
            } else {
                first_line.to_string().into()
            };
            Some(format!("stale — refresh failed: {capped}").into())
        } else if picker.refreshing {
            Some("refreshing…".into())
        } else {
            picker.fetched_at.map(|at| {
                let secs = at.elapsed().as_secs();
                if secs < 60 {
                    format!("updated {secs}s ago").into()
                } else {
                    format!("updated {}m ago", secs / 60).into()
                }
            })
        };

        let body: AnyElement =
            match &picker.state {
                PrPickerState::Loading => div()
                    .px_2()
                    .py_1()
                    .text_color(muted)
                    .child("loading…")
                    .into_any_element(),
                PrPickerState::Error(err) => div()
                    .px_2()
                    .py_1()
                    .text_color(danger)
                    .child(err.clone())
                    .into_any_element(),
                PrPickerState::Loaded(prs) if prs.is_empty() => div()
                    .px_2()
                    .py_1()
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
                                            .truncate()
                                            .child(pr.title.clone()),
                                    )
                                    .when(pr.is_draft, |el| {
                                        el.child(state_pill(muted, "draft").flex_none())
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
                .inset_0()
                .occlude()
                .bg(backdrop)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.close_pr_picker(window, cx);
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
                                .max_h(px(420.))
                                .overflow_hidden()
                                .p_2()
                                .gap_2()
                                // Same swallow-the-click-on-chrome reasoning as
                                // `render_palette`.
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .bg(panel_bg)
                                .text_color(fg)
                                .text_size(px(13.))
                                .border_1()
                                .border_color(seam)
                                .rounded_lg()
                                .shadow_lg()
                                .child(
                                    h_flex()
                                        .w_full()
                                        .justify_between()
                                        .px_2()
                                        .pt_1()
                                        .child(
                                            div()
                                                .flex_shrink_0()
                                                .text_size(px(11.))
                                                .text_color(muted)
                                                .child(
                                                    "Open PRs \u{b7} enter to open, esc to close",
                                                ),
                                        )
                                        .when_some(freshness, |el, hint| {
                                            let color = if picker.refresh_error.is_some() {
                                                danger
                                            } else {
                                                muted
                                            };
                                            el.child(
                                                div()
                                                    .min_w(px(0.))
                                                    .max_w(px(260.))
                                                    .truncate()
                                                    .text_size(px(11.))
                                                    .text_color(color)
                                                    .child(hint),
                                            )
                                        }),
                                )
                                .child(body),
                        ),
                ),
        )
    }

    /// One file-tree row, restyled by R1e to the reference design's
    /// file-tree anatomy:
    /// [`FILE_TREE_ROW_HEIGHT`] (24px, independent of the
    /// diff pane's font-size-scaled [`row_height`]), the filename itself
    /// carrying the status color rather than a separate badge — the tree
    /// row never pairs a letter pill with the filename, so matching that
    /// look means dropping the pill this row used pre-R1e (the underlying
    /// `file.status` is unaffected — only this row's render changes; see
    /// [`Self::render_file_diff_header`]'s doc comment, updated alongside
    /// this one, for the surface that DOES keep a pill) — plus this file's
    /// own `+N`/`−N` at 70% opacity, right-aligned, when its diff happens to
    /// already be loaded.
    fn render_file_row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let file = &self.files[index];
        let theme = cx.theme();
        let selected = self.selected == Some(index);
        // Colors snapshotted up front alongside `theme`'s fields (both plain
        // `&Theme`/`&DvTheme` reads, so they coexist fine) — the `cx.listener`
        // below needs `cx` mutably, so nothing borrowed from it can still be
        // live by then (this file's usual gpui gotcha).
        let dv = crate::themes::dv_theme(cx);
        let accent_alt = dv.accent_alt;
        let text_secondary = dv.text_secondary;
        let surface_active = dv.surface_active;
        // File-tree status-color mapping: renamed
        // shares the merged-PR pill's `accent_alt` "purple link" hue (`shell::
        // render_pr_glyphs`); copied is grouped with it (also a path-linkage
        // status, no separate slot in the spec's table). Type-change/unmerged/
        // unknown aren't in that table either — kept at their pre-restyle
        // colors (the same warning/danger/muted buckets they already used).
        let color = match file.status {
            ChangeStatus::Added => theme.success,
            ChangeStatus::Deleted => theme.danger,
            ChangeStatus::Renamed | ChangeStatus::Copied => accent_alt,
            ChangeStatus::Modified => theme.primary,
            ChangeStatus::TypeChanged => theme.warning,
            ChangeStatus::Unmerged => theme.danger,
            ChangeStatus::Unknown(_) => theme.muted_foreground,
        };
        // Dim directory prefix, status-colored basename (R1e visual review,
        // P2): only the leaf filename gets the color — a fully-tinted path made
        // the tree read as a wall of primary. Rename rows keep whole-label
        // coloring (the "old → new" pair reads as one linked unit).
        let (dir_prefix, leaf): (Option<SharedString>, SharedString) = match &file.old_path {
            Some(old) => (None, format!("{old} → {}", file.path).into()),
            None => match file.path.rfind('/') {
                Some(i) => (
                    Some(file.path[..=i].to_string().into()),
                    file.path[i + 1..].to_string().into(),
                ),
                None => (None, file.path.clone().into()),
            },
        };
        // Per-file diffstat: only available once this file's diff has
        // actually been loaded this session (Phase 7's lazy per-file load is
        // untouchable — this must never trigger an eager diff just to fill
        // in a number). Reuses the pre-tallied `RenderedDiff::added`/
        // `removed` fields (see `Self::render_file_diff_header`'s own doc
        // comment on why: no re-walking `diff.unified` on the hot render
        // path). Blank — not "+0 −0" — for a binary/errored/not-yet-loaded
        // diff, same suppression [`Self::render_file_diff_header`] uses.
        let diffstat: Option<(u32, u32)> = self.diffs.get(&index).and_then(|diff| {
            if diff.is_binary || diff.error {
                None
            } else {
                Some((diff.added, diff.removed))
            }
        });

        h_flex()
            .id(index)
            .gap_2()
            .items_center()
            .px_2()
            .h(px(FILE_TREE_ROW_HEIGHT))
            .w_full()
            .overflow_hidden()
            .when(selected, |el| el.bg(surface_active))
            .hover(|el| el.bg(surface_active.opacity(0.5)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| this.select_file(index, window, cx)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .text_sm()
                    .overflow_hidden()
                    .children(
                        dir_prefix.map(|prefix| div().text_color(text_secondary).child(prefix)),
                    )
                    .child(div().min_w(px(0.)).truncate().text_color(color).child(leaf)),
            )
            .children(diffstat.map(|(added, removed)| {
                h_flex()
                    .flex_none()
                    .gap_1()
                    .text_xs()
                    .opacity(0.7)
                    .child(div().text_color(theme.success).child(format!("+{added}")))
                    .child(
                        div()
                            .text_color(theme.danger)
                            .child(format!("\u{2212}{removed}")),
                    )
            }))
    }

    /// A hunk header row (shared by both views). When context is hidden
    /// above the hunk, the row is clickable and says how much it reveals.
    /// Two anatomies: a plain
    /// "hunk header row" (`recess_bg` bg, `muted.foreground` label) when
    /// nothing is hidden above it, or a "gap row" (`recess_bg` bg, hover
    /// `muted.background`, centered "⋯ N hidden lines") when there is. The
    /// reference design
    /// keeps these as two distinct row kinds; dv's row model folds the
    /// gap-affordance into the same [`Row::HunkHeader`]/[`SplitRow::
    /// HunkHeader`] variant (this fn's shared caller, both views) — see
    /// this file's row-model doc comment — so this single row plays double
    /// duty when `expandable` is `Some`: it's still the `@@ ...@@` boundary
    /// label (kept `flex_none` on the left — dropping it would lose the
    /// hunk's line-range info with nowhere else to show it) AND the
    /// clickable hidden-lines affordance, given its own `flex_1` centered
    /// segment to match the centered gap-row treatment.
    fn render_hunk_header(
        &self,
        label: SharedString,
        hunk: usize,
        expandable: Option<u32>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let muted_bg = theme.muted;
        let muted_fg = theme.muted_foreground;
        let accent = theme.primary;
        let recess_bg = crate::themes::dv_theme(cx).recess_bg;
        let base = h_flex()
            .id(("hunk-header", hunk))
            .w_full()
            .h(px(row_height(self.font_size)))
            .px_2()
            .bg(recess_bg)
            .font_family(mono)
            .text_size(px(self.font_size))
            .text_color(muted_fg);
        match expandable {
            None => base.child(label),
            Some(hidden) => base
                .cursor_pointer()
                .hover(|el| el.bg(muted_bg))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.expand_hunk_gap(hunk, cx)),
                )
                .child(label)
                .child(h_flex().flex_1().justify_center().child(
                    div().text_color(accent.opacity(0.9)).child(format!(
                        "\u{22ef} {hidden} hidden line{} \u{2014} click to expand",
                        if hidden == 1 { "" } else { "s" }
                    )),
                )),
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
        // A comment reads as resolved here if EITHER dv's local status says
        // so OR it's one of dv's own submitted-review threads that GitHub
        // reports resolved (`github_resolved`, populated by
        // `reset_diff_list` — see its doc comment). The summary panel lists
        // every comment in the review regardless of which file is open
        // (deliverable 5), so it has to agree with `render_thread`'s badge
        // rather than falling back to local-only status.
        let is_resolved = |c: &dv_core::Comment| {
            c.status == dv_core::CommentStatus::Resolved || self.github_resolved.contains(&c.id)
        };
        let comments: Vec<(usize, &dv_core::Comment)> = review
            .map(|r| {
                r.comments
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| match self.summary_filter {
                        SummaryFilter::All => true,
                        SummaryFilter::Open => !is_resolved(c),
                        SummaryFilter::Resolved => is_resolved(c),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let open_count = review
            .map(|r| r.comments.iter().filter(|c| !is_resolved(c)).count())
            .unwrap_or(0);
        let total = review.map(|r| r.comments.len()).unwrap_or(0);
        let submitted = review.and_then(|r| match &r.state {
            dv_core::ReviewState::Draft => None,
            dv_core::ReviewState::Submitted { verdict, .. } => Some(*verdict),
        });
        let readonly = self.review_is_readonly();
        let new_comment_blocked = self.new_comment_blocked();

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
        let files = &self.files;
        let rows: Vec<Stateful<Div>> = comments
            .iter()
            .map(|(_, comment)| {
                let id = comment.id.clone();
                let resolved = is_resolved(comment);
                let first_line = comment.body.lines().next().unwrap_or("").to_string();
                // docs/phase-6-review-navigator.md deliverable 5: a comment
                // whose file isn't in the currently open diff at all (as
                // opposed to `stale`, which is a comment ON this diff whose
                // anchored content has since drifted). Its click still
                // works — `jump_to_comment` reopens the review's own
                // recorded source for it (`switch_source_and_jump`).
                let off_diff = !files.iter().any(|f| f.path == comment.path);
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
                            })
                            .when(off_diff, |el| {
                                el.child(div().text_color(muted).child("\u{2197}"))
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
                .when(readonly, |el| {
                    // Suppress-the-entry-point posture (docs/phase-6-
                    // review-navigator.md S6c doc-deviation #4): this banner
                    // is the visible half of it — `open_thread_input`/
                    // `set_comment_status`/`delete_comment` are the
                    // unconditional enforcing half (existing threads on a
                    // submitted review can never be mutated). `gutter_down`
                    // is narrower still (`Self::new_comment_blocked`): a
                    // merely auto-selected submitted review still accepts a
                    // NEW comment, which starts a fresh draft — so the
                    // banner text distinguishes the two cases rather than
                    // claiming a uniform lockdown that isn't real.
                    el.child(
                        div()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .text_color(warning)
                            .child(if new_comment_blocked {
                                "Read-only \u{2014} this review has been submitted."
                            } else {
                                "This review has been submitted \u{2014} new comments start a fresh draft."
                            }),
                    )
                })
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
    /// thread, a read-only GitHub thread, or the comment editor.
    fn render_display_row(&self, display_ix: usize, cx: &mut Context<Self>) -> AnyElement {
        match self.display.get(display_ix) {
            Some(&DisplayRow::Diff(row)) => match self.view_mode {
                ViewMode::Unified => self.render_diff_row(row, cx).into_any_element(),
                ViewMode::Split => self.render_split_row(row, cx).into_any_element(),
            },
            Some(&DisplayRow::Thread(comment_ix)) => {
                self.render_thread(comment_ix, cx).into_any_element()
            }
            Some(&DisplayRow::RemoteThread(thread_ix)) => {
                self.render_remote_thread(thread_ix, cx).into_any_element()
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
        // `local_resolved` drives the Resolve/Unresolve button — dv can
        // only ever mutate the local status (two-way GitHub sync is a
        // non-goal, docs/phase-6-review-navigator.md § Non-goals).
        // `resolved` (badge/border) ORs in `github_resolved`, which covers
        // dv's own submitted-review threads deduped out of the read-only
        // `RemoteThread` cards in `reset_diff_list` — their resolved state
        // has to surface here instead, since either side resolving reads
        // as "resolved" (deliverable 6).
        let local_resolved = comment.status == dv_core::CommentStatus::Resolved;
        let resolved = local_resolved || self.github_resolved.contains(&comment.id);
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
        let text_secondary = crate::themes::dv_theme(cx).text_secondary;
        let created_age = crate::shell::relative_age(comment.created_ms);

        h_flex()
            .w_full()
            .py_3()
            .child(
                // The "72px blank gutter
                // spacer" — aligns the card's left edge with where the diff
                // row's CODE TEXT begins (see `thread_gutter_width`'s doc
                // comment), not `px_4`'s flat padding on both sides.
                div().w(px(thread_gutter_width(self.font_size))).flex_none(),
            )
            .child(
                v_flex()
                    .flex_1()
                    // `mr_4`, not `pr_4` — `p_3` below assigns all four
                    // padding sides (last-write-wins), which silently killed
                    // a `pr_4` set before it. An outer margin survives that
                    // and restores the right-edge gap the old `px_4` wrapper
                    // gave the card.
                    .mr_4()
                    .max_w(px(720.))
                    .p_3()
                    .gap_3()
                    // `sidebar.background` bg + a left-only accent bar
                    // ("card bg sidebar.background +
                    // border_l_2 primary accent bar") replaces the old
                    // popover-bg/full-border card. Resolved threads get the
                    // softer `muted.background` bar instead of full `primary` —
                    // NOT `border` (the Aura-dark trap: that theme's `border`
                    // is pure black, invisible here — see CLAUDE.md's cross-
                    // cutting risk).
                    .bg(theme.sidebar)
                    .border_l_2()
                    .border_color(if resolved { theme.muted } else { theme.primary })
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
                                    .text_xs()
                                    .text_color(text_secondary)
                                    .child(created_age),
                            )
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
                                            .label(if local_resolved {
                                                "Unresolve"
                                            } else {
                                                "Resolve"
                                            })
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                let status = if local_resolved {
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
                            el.child(
                                div()
                                    .text_sm()
                                    .text_color(text_secondary)
                                    .child(comment.body.clone()),
                            )
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
                                // `muted`, not `theme.border` — the Aura-dark
                                // trap (that theme's `border` is pure black,
                                // invisible on this card's background; see
                                // CLAUDE.md's cross-cutting risk and the
                                // resolved-accent choice above).
                                .border_color(theme.muted)
                                .children(comment.replies.iter().map(|reply| {
                                    h_flex()
                                        .gap_2()
                                        .text_sm()
                                        .child(div().font_semibold().child(reply.author.clone()))
                                        .child(
                                            div()
                                                .text_color(text_secondary)
                                                .child(reply.body.clone()),
                                        )
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

    /// A read-only GitHub-side review thread card (docs/phase-6-review-
    /// navigator.md deliverable 6): author/body/created for every comment
    /// in the thread, plus a "resolved" badge when GitHub says so — no
    /// reply/edit/resolve controls at all (two-way sync is a non-goal, see
    /// the phase doc's Non-goals). Deliberately a separate render fn from
    /// [`Self::render_thread`] rather than a read-only branch bolted onto
    /// it: a shared render fn risks a stray enabled button leaking through
    /// a future edit, where a wholly distinct fn structurally can't.
    fn render_remote_thread(&self, thread_ix: usize, cx: &mut Context<Self>) -> Div {
        // Owned copies before building any child (same borrow-avoidance
        // idiom as `render_thread`/`render_summary` — cross-cutting risk F
        // — even though this card has no listeners today, for consistency
        // with every other card in this file).
        let theme = cx.theme();
        // `muted`, not `theme.border` — the Aura-dark trap (that theme's
        // `border` is pure black, invisible on this card's background; see
        // CLAUDE.md's cross-cutting risk and `render_thread`'s resolved-
        // accent choice, which this card's read-only outline should match).
        let border = theme.muted;
        let popover = theme.popover;
        let success = theme.success;
        let muted = theme.muted_foreground;
        let text_secondary = crate::themes::dv_theme(cx).text_secondary;

        let Some(thread) = self.remote_threads.get(thread_ix) else {
            return div();
        };
        let Some(opening) = thread.comments.first() else {
            return div();
        };
        let replies = &thread.comments[1..];
        let line_label = thread
            .line
            .map(|l| format!("line {l}"))
            .unwrap_or_else(|| "outdated".to_string());

        h_flex()
            .w_full()
            .py_3()
            .child(
                // Same 72px-equivalent gutter spacer as `render_thread` —
                // keeps every card in the interleaved display list flush
                // to the same left edge regardless of which kind it is.
                div().w(px(thread_gutter_width(self.font_size))).flex_none(),
            )
            .child(
                v_flex()
                    .flex_1()
                    // `mr_4`, not `pr_4` (dead under `p_3`'s last-write-wins
                    // padding assignment below) — see `render_thread`'s note.
                    .mr_4()
                    .max_w(px(720.))
                    .p_3()
                    .gap_3()
                    // A muted background (rather than `render_thread`'s
                    // unresolved-primary tint, which means "needs your
                    // attention in dv") — a read-only remote thread never
                    // needs dv-side attention, only visibility.
                    .bg(popover.opacity(0.6))
                    .border_1()
                    .border_color(border)
                    .rounded_lg()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .flex_wrap()
                            .text_sm()
                            .child(div().font_semibold().child(opening.author.clone()))
                            .child(div().text_color(muted).child(format!(
                                "GitHub \u{b7} {line_label} \u{b7} {}",
                                crate::shell::relative_age(opening.created_ms)
                            )))
                            .when(thread.is_resolved, |el| {
                                el.child(div().text_color(success).child("\u{2713} resolved"))
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(text_secondary)
                            .child(opening.body.clone()),
                    )
                    .when(!replies.is_empty(), |el| {
                        el.child(
                            v_flex()
                                .gap_2()
                                .pl_3()
                                .border_l_2()
                                .border_color(border)
                                .children(replies.iter().map(|reply| {
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .text_sm()
                                        .child(div().font_semibold().child(reply.author.clone()))
                                        .child(
                                            div().text_color(muted).text_xs().child(
                                                crate::shell::relative_age(reply.created_ms),
                                            ),
                                        )
                                        .child(
                                            div()
                                                .text_color(text_secondary)
                                                .child(reply.body.clone()),
                                        )
                                })),
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

        h_flex()
            .w_full()
            .py_3()
            .child(
                // Same gutter spacer as `render_thread`/`render_remote_thread`
                // — the editor is a comment-card row too, in the same
                // interleaved display list.
                div().w(px(thread_gutter_width(self.font_size))).flex_none(),
            )
            .child(
                v_flex()
                    .flex_1()
                    // `mr_4`, not `pr_4` (dead under `p_3`'s last-write-wins
                    // padding assignment below) — see `render_thread`'s note.
                    .mr_4()
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
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_editor(window, cx)
                                    })),
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

    /// The "file header row" that sits atop the diff-row list
    /// itself — distinct from
    /// [`Self::render_header`]'s whole-REVIEW title bar (R1c): this one
    /// names the single file currently open. The reference design
    /// concatenates every
    /// file's diff into one scroll region, so its file-header row repeats
    /// per file as you scroll past one; dv shows one file at a time (picked
    /// from the file tree), so "the file header row" collapses to "the
    /// selected file's header", rendered once above the row list. `sidebar.
    /// background` bg, the same status pill/color mapping as [`Self::
    /// render_file_row`], the bold path (+ `← old_path` for renames), a
    /// `N comments · M outdated` note, and this ONE file's own `+N`/`−N` —
    /// counted straight off the already-computed [`RenderedDiff`] (unlike
    /// [`Self::diffstat`]'s whole-review running total, a single open
    /// file's diff is always fully loaded by the time this renders, so
    /// there's no partial-total qualifier to carry here).
    fn render_file_diff_header(&self, index: usize, cx: &mut Context<Self>) -> Div {
        use crate::shell::state_pill;

        let Some(file) = self.files.get(index) else {
            return div();
        };
        let theme = cx.theme();
        let sidebar_bg = theme.sidebar;
        let muted = theme.muted_foreground;
        let success = theme.success;
        let danger = theme.danger;
        let warning = theme.warning;
        let primary = theme.primary;
        let accent_alt = crate::themes::dv_theme(cx).accent_alt;

        // Same status → color mapping as `Self::render_file_row`'s
        // status-colored filename (kept in sync there rather than factored
        // out — see that fn's own doc comment on why renamed/copied share
        // `accent_alt`), but this row still keeps a `state_pill` badge
        // (below) spelling out the full status word rather than R1e's
        // colored-text treatment: this is the one row the reference design
        // renders as a labeled pill ("modified", not just a color),
        // matched exactly, while the dense file tree row now carries
        // the same signal purely through the filename's own color (review
        // finding: diff-pane "status pill"
        // vs file-tree "status-colored filenames").
        let (label, color) = match file.status {
            ChangeStatus::Added => ("added", success),
            ChangeStatus::Deleted => ("deleted", danger),
            ChangeStatus::Renamed => ("renamed", accent_alt),
            ChangeStatus::Copied => ("copied", accent_alt),
            ChangeStatus::Modified => ("modified", primary),
            ChangeStatus::TypeChanged => ("type changed", warning),
            ChangeStatus::Unmerged => ("unmerged", danger),
            ChangeStatus::Unknown(_) => ("unknown", muted),
        };

        // `None` ⇒ the diff hasn't loaded yet, it's binary (no textual diff
        // to count), or it failed to load (`error_diff` caches added=0/
        // removed=0, which is not a real count) — in every case "+0 -0"
        // would misleadingly read as an actual line-change count, so
        // suppress the diffstat children entirely rather than assert a
        // fake zero. Counts are read straight off the cached `RenderedDiff`
        // (tallied once in `build_rows`/`error_diff`) instead of re-walking
        // `diff.unified` here on every render.
        let diffstat: Option<(u32, u32)> = self.diffs.get(&index).and_then(|diff| {
            if diff.is_binary || diff.error {
                None
            } else {
                Some((diff.added, diff.removed))
            }
        });

        // Single pass over this file's comments for both the total and the
        // stale/outdated subset, rather than two separate `.filter().count()`
        // scans of `review.comments`.
        let (comment_total, outdated) = self
            .review
            .as_ref()
            .map(|r| {
                r.comments.iter().filter(|c| c.path == file.path).fold(
                    (0usize, 0usize),
                    |(total, outdated), c| {
                        (
                            total + 1,
                            outdated + usize::from(self.stale.contains(&c.id)),
                        )
                    },
                )
            })
            .unwrap_or((0, 0));

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .bg(sidebar_bg)
            .child(state_pill(color, label).flex_none())
            .child(
                div()
                    .flex_shrink_1()
                    .min_w(px(0.))
                    .font_semibold()
                    .truncate()
                    .child(file.path.clone()),
            )
            .children(file.old_path.as_ref().map(|old| {
                div()
                    .flex_none()
                    .text_color(muted)
                    .child(format!("\u{2190} {old}"))
            }))
            .child(div().flex_1())
            .when(comment_total > 0, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(muted)
                        .child(if outdated > 0 {
                            format!(
                                "{comment_total} comment{} \u{b7} {outdated} outdated",
                                if comment_total == 1 { "" } else { "s" }
                            )
                        } else {
                            format!(
                                "{comment_total} comment{}",
                                if comment_total == 1 { "" } else { "s" }
                            )
                        }),
                )
            })
            .children(diffstat.map(|(added, removed)| {
                h_flex()
                    .flex_none()
                    .gap_2()
                    .child(div().text_color(success).child(format!("+{added}")))
                    .child(div().text_color(danger).child(format!("\u{2212}{removed}")))
            }))
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
                // Go-to-definition (S8f) only ever wraps a New-side line —
                // see `Self::lsp_view_is_honest`'s scope note: a pure
                // Removed row (`new_line: None`) has no on-disk counterpart
                // vtsls could ever answer honestly about.
                let content = match new_line {
                    Some(new_line) => {
                        self.wrap_symbol_click_target(content, *new_line, text.clone(), cx)
                    }
                    None => content.into_any_element(),
                };

                let anchor = Self::row_anchor(*old_line, *new_line);
                // Existing-token role mapping:
                // selection (blue @30%) computes `primary.background`
                // @ 0.30 at apply time — was 0.18 pre-restyle.
                let selected_bg = anchor
                    .filter(|&(side, line)| {
                        self.selection
                            .as_ref()
                            .is_some_and(|sel| sel.contains(side, line))
                    })
                    .map(|_| theme.primary.opacity(0.30));

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
                    .child(
                        div()
                            .w(px(marker_width(self.font_size)))
                            .flex_none()
                            .child(marker),
                    )
                    .child(content)
            }
        }
    }

    fn render_split_row(&self, row_index: usize, cx: &mut Context<Self>) -> Div {
        // Copy the few colors out as owned values so no `&Theme` borrow of
        // `cx` is held across the `&mut cx` calls to render_split_cell.
        let mono = cx.theme().mono_font_family.clone();
        let muted = cx.theme().muted_foreground;
        let muted_bg = cx.theme().muted;
        // Divider recipe: a 6px
        // `recess_bg` column bordered `muted.background` both sides — NOT
        // `border` (the Aura-dark trap: that theme's `border` is pure
        // black, invisible against its own dark surfaces — see CLAUDE.md's
        // cross-cutting risk and `DvTheme`'s own doc comment in themes.rs).
        let recess_bg = crate::themes::dv_theme(cx).recess_bg;

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
                    .child(
                        div()
                            .w(px(6.))
                            .h_full()
                            .flex_none()
                            .bg(recess_bg)
                            .border_l_1()
                            .border_r_1()
                            .border_color(muted_bg),
                    )
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
        // The "absent side of a
        // one-sided split-view row" — `DvTheme::void_bg` (`recess_bg` @
        // 0.60), not a bare `muted.opacity(0.25)` guess.
        let void_bg = crate::themes::dv_theme(cx).void_bg;
        let Some(cell) = cell else {
            return div().h_full().bg(void_bg);
        };
        let (marker, bg) = match cell.kind {
            LineKind::Added => ("+", Some(theme.success.opacity(0.14))),
            LineKind::Removed => ("-", Some(theme.danger.opacity(0.14))),
            LineKind::Context => (" ", None),
        };
        // See `render_diff_row`'s matching comment: selection composes
        // `primary.background` @ 0.30 (was 0.18 pre-restyle).
        let selected_bg = cell
            .line
            .filter(|&line| {
                self.selection
                    .as_ref()
                    .is_some_and(|sel| sel.contains(side, line))
            })
            .map(|_| theme.primary.opacity(0.30));
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
        // Go-to-definition (S8f): only the New column ever gets the
        // click-target wrapper — same New-side-only scope as
        // `render_diff_row`'s unified path (see `Self::lsp_view_is_honest`).
        let content = match (side, cell.line) {
            (DiffSide::New, Some(line)) => {
                self.wrap_symbol_click_target(content, line, cell.text.clone(), cx)
            }
            _ => content.into_any_element(),
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
            .child(
                div()
                    .w(px(marker_width(self.font_size)))
                    .flex_none()
                    .child(marker),
            )
            .child(content)
    }
}

/// Rewrite a `base...head` (merge-base) range to a concrete two-dot range
/// anchored at the actual merge base, resolved with one `git merge-base`
/// call, and resolve `head` to a full oid (`git rev-parse --verify`) so
/// `Self::lsp_view_is_honest`'s oid comparison against
/// `Self::worktree_head_oid` actually has a chance to match. Without this,
/// a user-supplied `--range base..feature` (a symbolic ref, not already an
/// oid) could never satisfy that comparison — `worktree_head_oid` is
/// always a full SHA — even when `feature` is exactly what's checked out
/// (P3 finding). A `head` that fails to resolve (e.g. names something that
/// doesn't exist) is left as-is: `lsp_view_is_honest`'s comparison then
/// simply never matches, the existing decline-on-mismatch behavior, rather
/// than failing the whole diff load over an LSP-only affordance. Other
/// sources pass through unchanged.
fn resolve_source(repo: &GitRepo, source: DiffSource) -> anyhow::Result<DiffSource> {
    match source {
        DiffSource::Range {
            base,
            head,
            merge_base: true,
        } => {
            let merged = repo.merge_base(&base, &head)?;
            let head = repo.resolve(&head).unwrap_or(head);
            Ok(DiffSource::Range {
                base: merged,
                head,
                merge_base: false,
            })
        }
        DiffSource::Range {
            base,
            head,
            merge_base: false,
        } => {
            let head = repo.resolve(&head).unwrap_or(head);
            Ok(DiffSource::Range {
                base,
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
    let mut added_count = 0u32;
    let mut removed_count = 0u32;
    if diff.is_binary {
        unified.push(Row::Binary);
        split.push(SplitRow::Binary);
        return RenderedDiff {
            unified,
            split,
            hunk_rows_unified,
            hunk_rows_split,
            error: false,
            added: 0,
            removed: 0,
            is_binary: true,
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
            added: 0,
            removed: 0,
            is_binary: false,
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
            match p.kind {
                LineKind::Added => added_count += 1,
                LineKind::Removed => removed_count += 1,
                LineKind::Context => {}
            }
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
        added: added_count,
        removed: removed_count,
        is_binary: false,
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
        let summary = self.render_summary(cx);
        // Computed as its own local (same reasoning as `summary` just
        // above), not inline inside `.children(...)` down in the `Ready`
        // arm: `cx` needs a fresh `&mut` reborrow for the call, and this
        // keeps that reborrow's lifetime scoped to one statement rather
        // than tangled up in the match arm building the rest of `body`.
        let file_header = self
            .selected
            .map(|index| self.render_file_diff_header(index, cx));
        let theme = cx.theme();

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
                    .child(
                        v_flex()
                            .h_full()
                            .flex_1()
                            .min_w(px(0.))
                            .children(file_header)
                            .child(
                                div()
                                    .flex_1()
                                    .min_h(px(0.))
                                    // P2 finding: `Self::wrap_symbol_click_target`'s
                                    // `.on_hover` leave never fires for a wheel
                                    // scroll with the mouse stationary — gpui
                                    // dispatches a distinct `ScrollWheelEvent` for
                                    // that, never a synthetic `MouseMoveEvent` —
                                    // and once the raising row scrolls out of this
                                    // list's viewport its listener isn't even
                                    // registered anymore, so nothing else would ever
                                    // clear a popover left behind by a scroll.
                                    // Dropping it here (rather than repositioning
                                    // it) matches `on_symbol_hover_leave`'s own
                                    // posture: a stale popover is worth clearing,
                                    // not worth chasing across a scroll.
                                    //
                                    // Bump the epoch/release the claimed row
                                    // UNCONDITIONALLY — an in-flight debounced hover
                                    // request (up to ~3.3s inside the LSP warm-up
                                    // retry window) must also be superseded here,
                                    // not just an already-shown popover cleared, or
                                    // its answer can land after the scroll and pop a
                                    // stale, wrongly-anchored popover over whatever
                                    // is now under the stationary pointer (P2
                                    // finding). `cx.notify()` stays conditional on
                                    // an existing popover to actually clear.
                                    .on_scroll_wheel(cx.listener(
                                        |this, _: &ScrollWheelEvent, _window, cx| {
                                            this.hover_request_epoch += 1;
                                            this.hover_request_line = None;
                                            if this.hover_popover.take().is_some() {
                                                cx.notify();
                                            }
                                        },
                                    ))
                                    .child({
                                        let this = cx.weak_entity();
                                        list(self.diff_list.clone(), move |ix, _window, cx| {
                                            this.update(cx, |this, cx| {
                                                this.render_display_row(ix, cx)
                                            })
                                            .unwrap_or_else(|_| div().into_any_element())
                                        })
                                        .size_full()
                                    }),
                            ),
                    )
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
        if self.target_viewer.is_some() {
            key_context.push(' ');
            key_context.push_str(TARGET_VIEWER_CONTEXT);
        }

        // S8g: keeps `self.root_bounds` current every paint — the nearest
        // positioned ancestor `Self::render_hover_popover`'s `.absolute()`
        // child resolves against (see that method's doc comment). Zero-size,
        // paint-only, same `canvas` idiom `Self::wrap_symbol_click_target`
        // uses for its own per-row bounds probe.
        let root_bounds_for_paint = Rc::clone(&self.root_bounds);
        let root_bounds_probe = canvas(
            move |b, _, _| {
                root_bounds_for_paint.set(Some(b));
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        v_flex()
            .size_full()
            .relative()
            .track_focus(&self.focus_handle)
            .key_context(key_context.as_str())
            .child(root_bounds_probe)
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
            .on_action(cx.listener(Self::on_nav_back))
            .on_action(cx.listener(Self::on_nav_forward))
            .on_action(cx.listener(Self::on_close_target_viewer))
            .child(self.render_header(cx))
            .child(body)
            .children(self.render_palette(cx))
            .children(self.render_pr_picker(cx))
            .children(self.render_target_viewer(cx))
            .children(self.render_hover_popover(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChecksSummary, PrHeader, PrMeta, PrState, PreparedLine, SplitRow, SubmissionOutcome,
        SubmitFlow, SubmitPrep, Violation, ViolationKind, build_split_rows,
        cancel_submit_flow_outcome, gap_above, pick_review, pr_source_key,
        reconcile_file_selection, resolved_pin, review_adopts_pr, submit_flow_from_submission,
        submit_flow_from_validation, trim_trailing_newlines, verdict_label,
    };
    // Automation-only word functions (see their `#[cfg(feature =
    // "automation")]` gates above) — only the tests exercising them
    // directly need the import, so it's gated the same way.
    #[cfg(feature = "automation")]
    use super::{verdict_automation_word, violation_kind_word};
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
        let picked = pick_review(reviews, None, None, None);
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
        let picked = pick_review(reviews, Some("r-a-current"), Some(&pr_a), None);
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
        let picked = pick_review(reviews, None, Some(&current_pr), None);
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
        let picked = pick_review(reviews, Some("r-current"), Some(&current_pr), None);
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
        let picked = pick_review(reviews, None, Some(&current_pr), None);
        assert!(picked.is_none());
    }

    // ---- pick_review pinned_id precedence (docs/phase-6-review-navigator.md
    // S6c) --------------------------------------------------------------

    /// The headline case: a pin wins outright over a SUBMITTED review that
    /// `pick_review`'s ordinary rules would never surface (no PR open, and
    /// a draft exists and would otherwise win).
    #[test]
    fn pick_review_pinned_id_wins_over_a_submitted_review_with_a_draft_present() {
        let submitted = fake_review(
            "r-submitted",
            ReviewState::Submitted {
                verdict: dv_core::Verdict::Approve,
                at_ms: 0,
            },
            None,
        );
        let reviews = vec![submitted, fake_review("r-draft", ReviewState::Draft, None)];
        let picked = pick_review(reviews, None, None, Some("r-submitted"));
        assert_eq!(picked.map(|r| r.id), Some("r-submitted".to_string()));
    }

    /// A pin also wins over the PR-scoping rule — pinning a review outside
    /// the currently open PR is an explicit user choice, not the kind of
    /// accidental cross-PR adoption `current_pr`'s arm guards against.
    #[test]
    fn pick_review_pinned_id_wins_over_pr_scoping() {
        let pr_a = fake_remote(5, "github.com/acme/widgets");
        let reviews = vec![
            fake_review("r-a-current", ReviewState::Draft, Some(pr_a.clone())),
            fake_review("r-unrelated", ReviewState::Draft, None),
        ];
        let picked = pick_review(
            reviews,
            Some("r-a-current"),
            Some(&pr_a),
            Some("r-unrelated"),
        );
        assert_eq!(picked.map(|r| r.id), Some("r-unrelated".to_string()));
    }

    /// A vanished pin (the review was deleted between the click and this
    /// listing) must fall through to the normal rules rather than return
    /// `None` and blank the workspace.
    #[test]
    fn pick_review_pinned_id_not_found_falls_through() {
        let reviews = vec![fake_review("r-draft", ReviewState::Draft, None)];
        let picked = pick_review(reviews, None, None, Some("r-gone"));
        assert_eq!(picked.map(|r| r.id), Some("r-draft".to_string()));
    }

    // ---- resolved_pin (review finding, docs/phase-6-review-navigator.md
    // S6c: a stale pin must not outlive the review it points at) ---------

    /// The common case: the pin resolved to the review actually shown, so
    /// it's kept as-is.
    #[test]
    fn resolved_pin_keeps_a_pin_matching_the_shown_review() {
        let review = fake_review("r-a", ReviewState::Draft, None);
        let pin = resolved_pin(Some("r-a".to_string()), &Some(review));
        assert_eq!(pin, Some("r-a".to_string()));
    }

    /// The pinned review vanished and `pick_review` fell through to an
    /// unrelated one — the pin must be dropped, not left dangling (a
    /// dangling pin would keep blocking new comments on a review nobody
    /// explicitly selected).
    #[test]
    fn resolved_pin_drops_a_pin_that_landed_on_a_different_review() {
        let fallback = fake_review("r-other", ReviewState::Draft, None);
        let pin = resolved_pin(Some("r-gone".to_string()), &Some(fallback));
        assert_eq!(pin, None);
    }

    /// Nothing was pinned to begin with — stays `None`.
    #[test]
    fn resolved_pin_stays_none_when_nothing_was_pinned() {
        let review = fake_review("r-a", ReviewState::Draft, None);
        assert_eq!(resolved_pin(None, &Some(review)), None);
    }

    /// The pin resolved to nothing at all (no review loaded) — dropped.
    #[test]
    fn resolved_pin_drops_a_pin_when_no_review_is_loaded() {
        assert_eq!(resolved_pin(Some("r-a".to_string()), &None), None);
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

    // ---- pr_source_key (Phase 7 D3 PR-reopen cache) ------------------------
    //
    // `open_pr`'s cache stash/restore itself does real git/gh I/O and can't
    // run headless (same reasoning as `load_pr` above) — but the key
    // derivation it hinges on is pure, so it's exercised directly here
    // against the exact matrix the content-addressing promise relies on: a
    // `Range` source keys by `(merge_base, head_oid)` regardless of the
    // `merge_base` two-dot/three-dot flag, and every non-`Range` source has
    // no key at all (nothing for `open_pr`'s stash-on-entry guard to cache
    // under).

    #[test]
    fn pr_source_key_extracts_base_and_head_from_a_range_source() {
        let source = DiffSource::Range {
            base: "deadbeef".to_string(),
            head: "cafef00d".to_string(),
            merge_base: false,
        };
        assert_eq!(
            pr_source_key(&source),
            Some(("deadbeef".to_string(), "cafef00d".to_string()))
        );
    }

    #[test]
    fn pr_source_key_ignores_the_merge_base_flag() {
        // `load_pr` always resolves `base` to the merge-base tip itself and
        // sets `merge_base: false` (a concrete two-dot range) — but the key
        // must be stable off `base`/`head` alone, not the flag, so a
        // hand-built three-dot `Range` (if one ever reached this path)
        // wouldn't silently key differently for the same content.
        let two_dot = DiffSource::Range {
            base: "deadbeef".to_string(),
            head: "cafef00d".to_string(),
            merge_base: false,
        };
        let three_dot = DiffSource::Range {
            base: "deadbeef".to_string(),
            head: "cafef00d".to_string(),
            merge_base: true,
        };
        assert_eq!(pr_source_key(&two_dot), pr_source_key(&three_dot));
    }

    #[test]
    fn pr_source_key_is_none_for_non_range_sources() {
        assert_eq!(pr_source_key(&DiffSource::WorkingTree), None);
        assert_eq!(pr_source_key(&DiffSource::Staged), None);
        assert_eq!(
            pr_source_key(&DiffSource::Commit("abc123".to_string())),
            None
        );
    }

    #[test]
    fn pr_source_key_differs_on_either_oid_changing() {
        // The two cases the D3 verification calls out by name: a
        // force-push changes `head_oid` (base branch untouched), a rebase/
        // merge of the base branch changes `merge_base` (head untouched).
        // Either alone must miss the cache.
        let original = pr_source_key(&DiffSource::Range {
            base: "base1".to_string(),
            head: "head1".to_string(),
            merge_base: false,
        });
        let force_pushed = pr_source_key(&DiffSource::Range {
            base: "base1".to_string(),
            head: "head2".to_string(),
            merge_base: false,
        });
        let rebased_base = pr_source_key(&DiffSource::Range {
            base: "base2".to_string(),
            head: "head1".to_string(),
            merge_base: false,
        });
        assert_ne!(original, force_pushed);
        assert_ne!(original, rebased_base);
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
        }
        assert_eq!(
            verdict_label(dv_core::Verdict::RequestChanges),
            "request changes"
        );
    }

    // `verdict_automation_word`/`violation_kind_word` are automation-only
    // (see their `#[cfg(feature = "automation")]` gates above); these two
    // tests exercise them directly, so they're gated the same way rather
    // than failing to resolve under `--no-default-features`.
    #[cfg(feature = "automation")]
    #[test]
    fn verdict_automation_words_cover_every_variant() {
        for verdict in [
            dv_core::Verdict::Comment,
            dv_core::Verdict::Approve,
            dv_core::Verdict::RequestChanges,
        ] {
            assert!(!verdict_automation_word(verdict).is_empty());
        }
        assert_eq!(
            verdict_automation_word(dv_core::Verdict::RequestChanges),
            "request_changes"
        );
    }

    #[cfg(feature = "automation")]
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
