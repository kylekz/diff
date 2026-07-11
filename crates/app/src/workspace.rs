use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use dv_core::{
    BlobSpec, ChangeStatus, ChangedFile, DiffOptions, DiffSource, FileDiff, GitRepo, LineKind,
    RepoLocation,
};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::ActiveTheme;
use gpui_component::highlighter::HighlightTheme;
use gpui_component::{h_flex, v_flex};

use crate::highlight::{self, LineRuns};

actions!(
    workspace,
    [
        NextFile,
        PrevFile,
        NextHunk,
        PrevHunk,
        ToggleSplit,
        JumpToFile,
        ClearSelection,
        PaletteNext,
        PalettePrev,
        PaletteClose
    ]
);

const KEY_CONTEXT: &str = "Workspace";

/// Every diff row (lines and hunk headers alike) renders at this exact
/// height. uniform_list sizes its slots from a measured row; any variant
/// taller than the rest (the old padded headers) makes every other row sit
/// short in its slot, leaving unpainted gaps between line backgrounds.
const ROW_HEIGHT: f32 = 24.;
/// Extra identifier stamped onto the workspace node while the jump-to-file
/// palette is open. Single-char bindings are scoped to `!PaletteOpen` so
/// they keep bubbling into the palette's text input instead of firing.
const PALETTE_CONTEXT: &str = "PaletteOpen";

pub fn init(cx: &mut App) {
    let browse = Some("Workspace && !PaletteOpen");
    let palette = Some("Workspace && PaletteOpen");
    cx.bind_keys([
        KeyBinding::new("j", NextFile, browse),
        KeyBinding::new("down", NextFile, browse),
        KeyBinding::new("k", PrevFile, browse),
        KeyBinding::new("up", PrevFile, browse),
        KeyBinding::new("n", NextHunk, browse),
        KeyBinding::new("p", PrevHunk, browse),
        KeyBinding::new("s", ToggleSplit, browse),
        KeyBinding::new("f", JumpToFile, browse),
        KeyBinding::new("ctrl-p", JumpToFile, browse),
        KeyBinding::new("cmd-p", JumpToFile, browse),
        KeyBinding::new("escape", ClearSelection, browse),
        KeyBinding::new("down", PaletteNext, palette),
        KeyBinding::new("up", PalettePrev, palette),
        KeyBinding::new("escape", PaletteClose, palette),
    ]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Unified,
    Split,
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

pub struct Workspace {
    focus_handle: FocusHandle,
    source: DiffSource,
    status: Status,
    repo: Option<Arc<GitRepo>>,
    title: SharedString,
    head: SharedString,
    files: Vec<ChangedFile>,
    selected: Option<usize>,
    diffs: HashMap<usize, Arc<RenderedDiff>>,
    diff_pending: HashSet<usize>,
    view_mode: ViewMode,
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
    /// Wall-clock of the most recent per-file diff computation (blob fetch
    /// + diff + highlight), for `--automation` perf validation.
    last_diff_ms: Option<u64>,
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

impl Workspace {
    pub fn new(
        location: RepoLocation,
        source: DiffSource,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let this = Self {
            focus_handle: cx.focus_handle(),
            title: location.display_name().into(),
            source_desc: source_label(&source).into(),
            source: source.clone(),
            status: Status::Loading,
            repo: None,
            head: "".into(),
            files: Vec::new(),
            selected: None,
            diffs: HashMap::new(),
            diff_pending: HashSet::new(),
            view_mode: ViewMode::Unified,
            current_hunk: 0,
            expanded: HashMap::new(),
            file_scroll: UniformListScrollHandle::new(),
            diff_list: ListState::new(0, ListAlignment::Top, px(600.)),
            palette: None,
            selection: None,
            last_diff_ms: None,
        };

        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let repo = GitRepo::open(location)?;
                    let head = repo.head_label().unwrap_or_default();
                    // Resolve a merge-base range to a concrete two-dot range
                    // once here, so both the file list and every per-file
                    // old-side blob load from the merge base rather than from
                    // `base` directly (correct when base has advanced past
                    // the fork point). `git diff a...b` ≡ `a-merge-base..b`.
                    let source = resolve_source(&repo, source)?;
                    let files = repo.changed_files(&source)?;
                    anyhow::Ok((Arc::new(repo), head, files, source))
                })
                .await;

            this.update(cx, |this, cx| {
                match loaded {
                    Ok((repo, head, files, source)) => {
                        this.repo = Some(repo);
                        this.head = head.into();
                        this.files = files;
                        this.source = source;
                        this.status = Status::Ready;
                        if !this.files.is_empty() {
                            this.select_file(0, cx);
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

    pub(crate) fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.files.len() {
            return;
        }
        self.selected = Some(index);
        self.current_hunk = 0;
        self.file_scroll
            .scroll_to_item(index, ScrollStrategy::Nearest);
        self.reset_diff_list(cx);
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
        let expand = self.expanded.get(&index).cloned().unwrap_or_default();
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
                .spawn(async move { compute_diff(&repo, &source, &file, &hl, &expand) })
                .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;

            this.update(cx, |this, cx| {
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
                    this.reset_diff_list(cx);
                    this.scroll_to_current_hunk();
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        })
    }

    /// Bounds-checked selection for `--automation`: unlike the UI path
    /// (which silently ignores stale indices), scripts get a hard error so
    /// the response never lies about what happened.
    pub(crate) fn automation_select_file(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        if index >= self.files.len() {
            anyhow::bail!(
                "file index {index} out of range ({} files)",
                self.files.len()
            );
        }
        self.select_file(index, cx);
        Ok(())
    }

    /// True once there is nothing left in flight: repo loaded (or failed)
    /// and the selected file's diff computed with no recompute pending
    /// (gap expansion keeps stale rows visible while it rebuilds).
    /// `wait_ready` polls this.
    pub(crate) fn automation_settled(&self) -> bool {
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

    fn on_next_file(&mut self, _: &NextFile, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.selected.map_or(0, |i| i + 1);
        self.select_file(next.min(self.files.len().saturating_sub(1)), cx);
    }

    fn on_prev_file(&mut self, _: &PrevFile, _: &mut Window, cx: &mut Context<Self>) {
        let prev = self.selected.map_or(0, |i| i.saturating_sub(1));
        self.select_file(prev, cx);
    }

    fn on_toggle_split(&mut self, _: &ToggleSplit, _: &mut Window, cx: &mut Context<Self>) {
        self.view_mode = match self.view_mode {
            ViewMode::Unified => ViewMode::Split,
            ViewMode::Split => ViewMode::Unified,
        };
        // Row count and indices differ between the views; re-sync the list
        // and keep the eye on the same hunk across the toggle.
        self.reset_diff_list(cx);
        self.scroll_to_current_hunk();
        cx.notify();
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

    /// Re-sync the list's item count after anything that changes the row
    /// set (file switch, mode toggle, recompute) and park it at the top.
    fn reset_diff_list(&mut self, _cx: &mut Context<Self>) {
        self.diff_list.reset(self.diff_row_count());
    }

    fn scroll_to_current_hunk(&mut self) {
        let current = self.current_hunk;
        if let Some(&row) = self.hunk_rows().and_then(|rows| rows.get(current)) {
            self.diff_list.scroll_to(ListOffset {
                item_ix: row,
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

    fn gutter_up(&mut self, cx: &mut Context<Self>) {
        if let Some(sel) = &mut self.selection
            && sel.dragging
        {
            sel.dragging = false;
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
            self.select_file(file, cx);
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
                cx.listener(move |this, _, _, cx| this.select_file(index, cx)),
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
            .h(px(ROW_HEIGHT))
            .px_2()
            .bg(theme.muted)
            .font_family(theme.mono_font_family.clone())
            .text_sm()
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
                            .w_12()
                            .flex_none()
                            .pr_1()
                            .text_right()
                            .text_color(theme.muted_foreground.opacity(0.8))
                            .child(num(old_line)),
                    )
                    .child(
                        div()
                            .w_12()
                            .flex_none()
                            .pr_2()
                            .text_right()
                            .text_color(theme.muted_foreground.opacity(0.8))
                            .child(num(new_line)),
                    );

                h_flex()
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .font_family(mono)
                    .text_sm()
                    .when_some(bg, |el, bg| el.bg(bg))
                    .when_some(selected_bg, |el, bg| el.bg(bg))
                    .when_some(anchor, |el, (side, line)| {
                        el.on_mouse_move(cx.listener(move |this, _, _, cx| {
                            this.gutter_drag_over(side, line, cx);
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| this.gutter_up(cx)),
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
                    .h(px(ROW_HEIGHT))
                    .items_stretch()
                    .font_family(mono)
                    .text_sm()
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
            .w_12()
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
                    cx.listener(|this, _, _, cx| this.gutter_up(cx)),
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
        &DiffOptions::default(),
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

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mode = self.view_mode;

        let body: Div = match &self.status {
            Status::Loading => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child("opening repository…"),
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
                            this.update(cx, |this, cx| match this.view_mode {
                                ViewMode::Unified => {
                                    this.render_diff_row(ix, cx).into_any_element()
                                }
                                ViewMode::Split => this.render_split_row(ix, cx).into_any_element(),
                            })
                            .unwrap_or_else(|_| div().into_any_element())
                        })
                        .size_full()
                    })),
            ),
        };

        // While the palette is open the workspace node also carries the
        // PaletteOpen identifier, flipping which key bindings apply (see
        // `init`).
        let key_context = if self.palette.is_some() {
            format!("{KEY_CONTEXT} {PALETTE_CONTEXT}")
        } else {
            KEY_CONTEXT.to_string()
        };

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
            .on_action(cx.listener(Self::on_jump_to_file))
            .on_action(cx.listener(Self::on_clear_selection))
            .on_action(cx.listener(Self::on_palette_next))
            .on_action(cx.listener(Self::on_palette_prev))
            .on_action(cx.listener(Self::on_palette_close))
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
                    .child(self.title.clone())
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
                    .child(div().flex_1())
                    .child(
                        h_flex()
                            .id("view-toggle")
                            .px_2()
                            .rounded_md()
                            .cursor_pointer()
                            .text_color(theme.muted_foreground)
                            .hover(|el| el.bg(theme.accent.opacity(0.4)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    this.on_toggle_split(&ToggleSplit, window, cx)
                                }),
                            )
                            .child(match mode {
                                ViewMode::Unified => "unified · s",
                                ViewMode::Split => "split · s",
                            }),
                    ),
            )
            .child(body)
            .children(self.render_palette(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::{PreparedLine, SplitRow, build_split_rows, gap_above};
    use dv_core::LineKind;

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
}
