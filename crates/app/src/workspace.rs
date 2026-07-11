use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dv_core::{
    BlobSpec, ChangeStatus, ChangedFile, DiffOptions, DiffSource, FileDiff, GitRepo, LineKind,
    RepoLocation,
};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{ActiveTheme, h_flex, v_flex};

actions!(workspace, [NextFile, PrevFile]);

const KEY_CONTEXT: &str = "Workspace";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("j", NextFile, Some(KEY_CONTEXT)),
        KeyBinding::new("down", NextFile, Some(KEY_CONTEXT)),
        KeyBinding::new("k", PrevFile, Some(KEY_CONTEXT)),
        KeyBinding::new("up", PrevFile, Some(KEY_CONTEXT)),
    ]);
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
    },
    Binary,
    NoChanges,
}

struct RenderedDiff {
    rows: Vec<Row>,
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
            source: source.clone(),
            status: Status::Loading,
            repo: None,
            head: "".into(),
            files: Vec::new(),
            selected: None,
            diffs: HashMap::new(),
            diff_pending: HashSet::new(),
        };

        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let repo = GitRepo::open(location)?;
                    let head = repo.head_label().unwrap_or_default();
                    let files = repo.changed_files(&source)?;
                    anyhow::Ok((Arc::new(repo), head, files))
                })
                .await;

            this.update(cx, |this, cx| {
                match loaded {
                    Ok((repo, head, files)) => {
                        this.repo = Some(repo);
                        this.head = head.into();
                        this.files = files;
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

    fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
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
        cx.spawn(async move |this, cx| {
            let rendered = cx
                .background_executor()
                .spawn(async move { compute_diff(&repo, &source, &file) })
                .await;

            this.update(cx, |this, cx| {
                this.diff_pending.remove(&index);
                match rendered {
                    Ok(diff) => {
                        this.diffs.insert(index, Arc::new(diff));
                    }
                    Err(err) => {
                        this.diffs.insert(
                            index,
                            Arc::new(RenderedDiff {
                                rows: vec![Row::HunkHeader(
                                    format!("failed to compute diff: {err:#}").into(),
                                )],
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

    fn on_next_file(&mut self, _: &NextFile, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.selected.map_or(0, |i| i + 1);
        self.select_file(next.min(self.files.len().saturating_sub(1)), cx);
    }

    fn on_prev_file(&mut self, _: &PrevFile, _: &mut Window, cx: &mut Context<Self>) {
        let prev = self.selected.map_or(0, |i| i.saturating_sub(1));
        self.select_file(prev, cx);
    }

    fn source_label(&self) -> &'static str {
        match &self.source {
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

        match &diff.rows[row_index] {
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
                .child("no content changes (mode or rename only)"),
            Row::Line {
                kind,
                old_line,
                new_line,
                text,
            } => {
                let (marker, bg) = match kind {
                    LineKind::Added => ("+", Some(theme.success.opacity(0.14))),
                    LineKind::Removed => ("-", Some(theme.danger.opacity(0.14))),
                    LineKind::Context => (" ", None),
                };
                let num = |n: &Option<u32>| -> SharedString {
                    n.map(|v| v.to_string()).unwrap_or_default().into()
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
                    .child(div().whitespace_nowrap().child(text.clone()))
            }
        }
    }
}

fn compute_diff(
    repo: &GitRepo,
    source: &DiffSource,
    file: &ChangedFile,
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
    Ok(render_rows(&diff))
}

fn render_rows(diff: &FileDiff) -> RenderedDiff {
    let mut rows = Vec::new();
    if diff.is_binary {
        rows.push(Row::Binary);
    } else if diff.hunks.is_empty() {
        rows.push(Row::NoChanges);
    }
    for hunk in &diff.hunks {
        rows.push(Row::HunkHeader(
            format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            )
            .into(),
        ));
        for line in &hunk.lines {
            rows.push(Row::Line {
                kind: line.kind,
                old_line: line.old_line,
                new_line: line.new_line,
                text: line.text.clone().into(),
            });
        }
    }
    RenderedDiff { rows }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let row_count = self
            .selected
            .and_then(|i| self.diffs.get(&i))
            .map_or(0, |d| d.rows.len());

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
                        div().h_full().flex_1().min_w(px(0.)).child(
                            uniform_list(
                                "diff-rows",
                                row_count,
                                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                    range
                                        .map(|i| this.render_diff_row(i, cx))
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .size_full(),
                        ),
                    ),
            ),
        };

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_next_file))
            .on_action(cx.listener(Self::on_prev_file))
            .child(
                gpui_component::TitleBar::new().child(
                    h_flex()
                        .gap_2()
                        .child(self.title.clone())
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(self.head.clone()),
                        )
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(self.source_label()),
                        ),
                ),
            )
            .child(body)
    }
}
