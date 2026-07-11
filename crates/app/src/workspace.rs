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

actions!(workspace, [NextFile, PrevFile, ToggleSplit]);

const KEY_CONTEXT: &str = "Workspace";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("j", NextFile, Some(KEY_CONTEXT)),
        KeyBinding::new("down", NextFile, Some(KEY_CONTEXT)),
        KeyBinding::new("k", PrevFile, Some(KEY_CONTEXT)),
        KeyBinding::new("up", PrevFile, Some(KEY_CONTEXT)),
        KeyBinding::new("s", ToggleSplit, Some(KEY_CONTEXT)),
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
    HunkHeader(SharedString),
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
    HunkHeader(SharedString),
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
        cx.notify();

        if self.diffs.contains_key(&index) || self.diff_pending.contains(&index) {
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.diff_pending.insert(index);

        let file = self.files[index].clone();
        let source = self.source.clone();
        let theme = cx.theme();
        let hl = HighlightInputs {
            theme: theme.highlight_theme.clone(),
            intra_added: theme.success.opacity(0.32),
            intra_removed: theme.danger.opacity(0.32),
        };
        cx.spawn(async move |this, cx| {
            let rendered = cx
                .background_executor()
                .spawn(async move { compute_diff(&repo, &source, &file, &hl) })
                .await;

            this.update(cx, |this, cx| {
                this.diff_pending.remove(&index);
                match rendered {
                    Ok(diff) => {
                        this.diffs.insert(index, Arc::new(diff));
                    }
                    Err(err) => {
                        let msg: SharedString = format!("failed to compute diff: {err:#}").into();
                        this.diffs.insert(
                            index,
                            Arc::new(RenderedDiff {
                                unified: vec![Row::HunkHeader(msg.clone())],
                                split: vec![SplitRow::HunkHeader(msg)],
                            }),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
    /// and the selected file's diff computed. `wait_ready` polls this.
    pub(crate) fn automation_settled(&self) -> bool {
        match &self.status {
            Status::Loading => false,
            Status::Failed(_) => true,
            Status::Ready => match self.selected {
                Some(index) => self.diffs.contains_key(&index),
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
        cx.notify();
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

    fn render_diff_row(&self, row_index: usize, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let Some(diff) = self.selected.and_then(|i| self.diffs.get(&i)) else {
            return div();
        };
        let mono = theme.mono_font_family.clone();

        match &diff.unified[row_index] {
            Row::HunkHeader(text) => div()
                .w_full()
                .px_2()
                .py_0p5()
                .bg(theme.muted)
                .font_family(mono)
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(text.clone()),
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

                h_flex()
                    .w_full()
                    .font_family(mono)
                    .text_sm()
                    .when_some(bg, |el, bg| el.bg(bg))
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
                    )
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
        let muted_bg = cx.theme().muted;
        let border = cx.theme().border;

        let Some(diff) = self.selected.and_then(|i| self.diffs.get(&i)) else {
            return div();
        };

        match &diff.split[row_index] {
            SplitRow::HunkHeader(text) => div()
                .w_full()
                .px_2()
                .py_0p5()
                .bg(muted_bg)
                .font_family(mono)
                .text_sm()
                .text_color(muted)
                .child(text.clone()),
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
                let left = self.render_split_cell(left.clone(), cx);
                let right = self.render_split_cell(right.clone(), cx);
                h_flex()
                    .w_full()
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
    fn render_split_cell(&self, cell: Option<SplitCell>, cx: &mut Context<Self>) -> Div {
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
        let number: SharedString = cell.line.map(|v| v.to_string()).unwrap_or_default().into();
        let content = if cell.runs.is_empty() {
            div().whitespace_nowrap().child(cell.text.clone())
        } else {
            div().whitespace_nowrap().child(
                StyledText::new(cell.text.clone()).with_highlights(cell.runs.iter().cloned()),
            )
        };

        h_flex()
            .w_full()
            .font_family(mono)
            .when_some(bg, |el, bg| el.bg(bg))
            .child(
                div()
                    .w_12()
                    .flex_none()
                    .pr_2()
                    .text_right()
                    .text_color(theme.muted_foreground.opacity(0.8))
                    .child(number),
            )
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

    Ok(build_rows(&diff, &old_runs, &new_runs, hl))
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

fn build_rows(
    diff: &FileDiff,
    old_runs: &LineRuns,
    new_runs: &LineRuns,
    hl: &HighlightInputs,
) -> RenderedDiff {
    let mut unified = Vec::new();
    let mut split = Vec::new();
    if diff.is_binary {
        unified.push(Row::Binary);
        split.push(SplitRow::Binary);
        return RenderedDiff { unified, split };
    }
    if diff.hunks.is_empty() {
        unified.push(Row::NoChanges);
        split.push(SplitRow::NoChanges);
        return RenderedDiff { unified, split };
    }

    let empty: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for hunk in &diff.hunks {
        let header: SharedString = format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        )
        .into();
        unified.push(Row::HunkHeader(header.clone()));
        split.push(SplitRow::HunkHeader(header));

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
    RenderedDiff { unified, split }
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
        let row_count = self
            .selected
            .and_then(|i| self.diffs.get(&i))
            .map_or(0, |d| match mode {
                ViewMode::Unified => d.unified.len(),
                ViewMode::Split => d.split.len(),
            });

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
                                .size_full(),
                            ),
                    )
                    .child(
                        div().h_full().flex_1().min_w(px(0.)).child(match mode {
                            ViewMode::Unified => uniform_list(
                                "diff-rows-unified",
                                row_count,
                                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                    range
                                        .map(|i| this.render_diff_row(i, cx))
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .size_full(),
                            ViewMode::Split => uniform_list(
                                "diff-rows-split",
                                row_count,
                                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                    range
                                        .map(|i| this.render_split_row(i, cx))
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .size_full(),
                        }),
                    ),
            ),
        };

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_next_file))
            .on_action(cx.listener(Self::on_prev_file))
            .on_action(cx.listener(Self::on_toggle_split))
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
    }
}

#[cfg(test)]
mod tests {
    use super::{PreparedLine, SplitRow, build_split_rows};
    use dv_core::LineKind;

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
