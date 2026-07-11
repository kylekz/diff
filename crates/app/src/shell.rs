//! The application shell: the review-navigator sidebar plus the main area
//! that hosts the active review. dv is review-centric — the sidebar is a
//! library of reviews across repos (see docs/ui-design.md).

use std::path::PathBuf;

use dv_core::{DiffSource, RepoLocation};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, StyledExt, TitleBar, h_flex, v_flex};

use crate::recent::{RecentEntry, RecentStore, title_for};
use crate::workspace::Workspace;

actions!(shell, [NewReview]);

const KEY_CONTEXT: &str = "AppShell";

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

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-n", NewReview, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-n", NewReview, Some(KEY_CONTEXT)),
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
}

impl AppShell {
    pub fn new(
        seed: Option<(RepoLocation, DiffSource)>,
        automation: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            recent: RecentStore::load(),
            active: None,
            selected: None,
            automation,
        };
        match seed {
            Some((location, source)) => this.open_review(location, source, window, cx),
            // Nothing to focus into, so hold focus on the shell — otherwise
            // the advertised Ctrl+N binding (in the shell's key context) has
            // no focused node on its dispatch path and never fires.
            None => window.focus(&this.focus_handle, cx),
        }
        this
    }

    /// Open a review for `location`/`source`: record it as most-recent,
    /// spin up a fresh Workspace, and focus it so keyboard nav is live.
    fn open_review(
        &mut self,
        location: RepoLocation,
        source: DiffSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Persist an absolute path: a relative one (`dv .`) would resolve
        // against whatever cwd the app is next launched from.
        let location = absolutize(location);
        let title = title_for(&location, &source);
        self.recent.touch(RecentEntry {
            location: location.clone(),
            source: source.clone(),
            title,
        });
        self.selected = Some(0); // touch() moved it to the front

        let workspace = cx.new(|cx| Workspace::new(location, source, window, cx));
        let handle = workspace.focus_handle(cx);
        window.focus(&handle, cx);
        self.active = Some(workspace);
        cx.notify();
    }

    fn open_recent(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.recent.entries().get(index).cloned() else {
            return;
        };
        self.open_review(entry.location, entry.source, window, cx);
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
            self.open_review(location, DiffSource::WorkingTree, window, cx);
        }
    }

    /// Semantic state for `--automation`: the sidebar plus the active
    /// review's own dump (see [`Workspace::automation_state`]).
    pub(crate) fn automation_state(&self, cx: &App) -> serde_json::Value {
        use serde_json::json;
        json!({
            "recent": self.recent.entries().iter().map(|e| e.title.clone()).collect::<Vec<_>>(),
            "selected": self.selected,
            "workspace": self.active.as_ref().map(|ws| ws.read(cx).automation_state()),
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
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        match &self.active {
            Some(ws) => ws.update(cx, |ws, cx| ws.automation_select_file(index, cx)),
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
        self.open_review(location, DiffSource::WorkingTree, window, cx);
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
            .child(div().text_sm().truncate().child(entry.title.clone()))
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

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_new_review))
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
                                div()
                                    .px_2()
                                    .py_1()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("RECENT"),
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
    }
}
