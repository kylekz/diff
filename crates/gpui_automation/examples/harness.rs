//! A small gpui-component app that implements `AutomationHost`, used to prove
//! every automation command end to end (`crates/gpui_automation/tests/
//! harness.jsonl`, run through the `e2e` runner).
//!
//! ```text
//! cargo build -p gpui_automation --example harness --bin e2e
//! cargo run -p gpui_automation --bin e2e -- --app target/debug/examples/harness.exe \
//!     crates/gpui_automation/tests/harness.jsonl
//! ```
//!
//! Flags: `--automation`, `--automation-pipe <name>` (serve the protocol on
//! a named pipe instead of stdio), `--automation-background` (open the
//! windows unfocused and behind everything else, as the runner does by
//! default), `--window-size WxH`, `--maximized` (ask for a maximized main
//! window, which background mode opens windowed), `--data-dir <dir>`
//! (only reported in `state`, so the runner's temp dir is visible to
//! scenarios) and `--counter <n>` (the counter's start value, so scenarios
//! can see which arguments a restart used).
//!
//! `state.env` reports the variables the runner sets for dv (`DV_DATA_DIR`,
//! `DV_GH`, `DV_MOCK_GH`, `DV_MOCK_GH_LOG`, `GIT_CEILING_DIRECTORIES`), and
//! the `run_gh` command runs `$DV_GH` as dv runs `gh` (docs/PLAN.md D7), so
//! `harness_gh.jsonl` can prove the `mock_gh` wiring without dv itself.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Bounds, ClickEvent, Context, DragMoveEvent, Entity,
    ExternalPaths, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyBinding,
    MouseButton, ParentElement as _, Pixels, Render, ScrollHandle, SharedString, Size,
    StatefulInteractiveElement as _, Styled as _, Task, TitlebarOptions, Window, WindowBounds,
    WindowHandle, WindowOptions, actions, div, px, rgb, size,
};
use gpui_automation::{AutomationHost, AutomationTargetExt as _, Options};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::{Root, WindowExt as _, h_flex, v_flex};
use serde_json::{Value, json};

actions!(harness, [Increment, Reset, OpenSecondWindow]);

/// How long a drag must hover over the drop target before it "expands",
/// mirroring the sidebar's hover-to-expand folders.
const HOVER_EXPAND: Duration = Duration::from_millis(1000);
/// Delay of the "Slow +1" button, which keeps the app unsettled meanwhile.
const SLOW_INCREMENT: Duration = Duration::from_millis(400);
const ROWS: usize = 60;
const SECOND_MIN_SIZE: Size<Pixels> = Size {
    width: px(320.),
    height: px(200.),
};
/// The environment the e2e runner sets for dv, reported under `state.env`.
const RUNNER_ENV: &[&str] = &[
    "DV_DATA_DIR",
    "DV_GH",
    "DV_MOCK_GH",
    "DV_MOCK_GH_LOG",
    "GIT_CEILING_DIRECTORIES",
];

#[derive(Debug, Default)]
struct Args {
    automation: bool,
    automation_pipe: Option<String>,
    automation_background: bool,
    window_size: Option<(f32, f32)>,
    /// Ask for a maximized main window, which a background run must still
    /// open windowed (gpui maximizes with `SW_MAXIMIZE`, which activates).
    maximized: bool,
    data_dir: Option<PathBuf>,
    counter: i64,
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--automation" => args.automation = true,
            "--automation-background" => args.automation_background = true,
            "--automation-pipe" => {
                args.automation_pipe = Some(iter.next().context("--automation-pipe needs a name")?);
            }
            "--counter" => {
                let value = iter.next().context("--counter needs a value")?;
                args.counter = value
                    .parse()
                    .with_context(|| format!("--counter expects an integer, got {value:?}"))?;
            }
            "--window-size" => {
                let spec = iter.next().context("--window-size needs a value")?;
                args.window_size = Some(gpui_automation::parse_window_size(&spec)?);
            }
            "--maximized" => args.maximized = true,
            "--data-dir" => {
                args.data_dir = Some(iter.next().context("--data-dir needs a value")?.into());
            }
            other => bail!("unknown argument {other:?}"),
        }
    }
    Ok(args)
}

/// Drag payload for the draggable item.
struct DraggedItem;

struct DragPreview;

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .rounded_md()
            .bg(rgb(0x3b82f6))
            .text_color(rgb(0xffffff))
            .child("Dragging item")
    }
}

struct SecondView {
    clicks: u32,
}

impl Render for SecondView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .gap_3()
            .p_4()
            .bg(rgb(0x1f2430))
            .text_color(rgb(0xe6e6e6))
            .child(format!("Second window, clicks: {}", self.clicks))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("second-click")
                            .primary()
                            .label("Click me")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.clicks += 1;
                                cx.notify();
                            }))
                            .automation_target("second/click"),
                    )
                    .child(
                        Button::new("second-close")
                            .label("Close")
                            .on_click(|_, window, _| window.remove_window())
                            .automation_target("second/close"),
                    ),
            )
    }
}

struct Harness {
    focus_handle: FocusHandle,
    input: Entity<InputState>,
    counter: i64,
    resets: u32,
    double_clicks: u32,
    right_clicks: u32,
    item_hovered: bool,
    dropped: Vec<String>,
    drop_zone_hovered: bool,
    item_in_target: bool,
    target_hovered: bool,
    target_expanded: bool,
    hover_timer: Option<Task<()>>,
    list_scroll: ScrollHandle,
    selected_row: Option<usize>,
    slow: Option<Task<()>>,
    /// Confirm presses in the dialog, which slides in as it opens.
    dialog_confirms: u32,
    second: Option<(WindowHandle<Root>, Entity<SecondView>)>,
    data_dir: Option<PathBuf>,
}

impl Harness {
    fn new(
        data_dir: Option<PathBuf>,
        counter: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            focus_handle,
            input: cx.new(|cx| InputState::new(window, cx).placeholder("Type here")),
            counter,
            resets: 0,
            double_clicks: 0,
            right_clicks: 0,
            item_hovered: false,
            dropped: Vec::new(),
            drop_zone_hovered: false,
            item_in_target: false,
            target_hovered: false,
            target_expanded: false,
            hover_timer: None,
            list_scroll: ScrollHandle::new(),
            selected_row: None,
            slow: None,
            dialog_confirms: 0,
            second: None,
            data_dir,
        }
    }

    fn second_open(&self, cx: &App) -> Option<&(WindowHandle<Root>, Entity<SecondView>)> {
        self.second
            .as_ref()
            .filter(|(handle, _)| cx.windows().contains(&(*handle).into()))
    }

    fn open_second_window(&mut self, cx: &mut Context<Self>) {
        if self.second_open(cx).is_some() {
            return;
        }
        let view = cx.new(|_| SecondView { clicks: 0 });
        // Like dv's windows: a custom (transparent) title bar and a minimum
        // size, which `resize` has to respect.
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(480.), px(260.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Second".into()),
                appears_transparent: true,
                traffic_light_position: None,
            }),
            window_min_size: Some(SECOND_MIN_SIZE),
            ..Default::default()
        };
        let root_view = view.clone();
        match gpui_automation::open_window(cx, options, |window, cx| {
            cx.new(|cx| Root::new(root_view, window, cx))
        }) {
            Ok(handle) => {
                gpui_automation::register_window("second", handle.into(), cx);
                self.second = Some((handle, view));
            }
            Err(err) => log::error!("opening the second window failed: {err:#}"),
        }
        cx.notify();
    }

    fn start_slow_increment(&mut self, cx: &mut Context<Self>) {
        self.slow = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SLOW_INCREMENT).await;
            this.update(cx, |this, cx| {
                this.counter += 1;
                this.slow = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// A gpui-component dialog, which slides in over a quarter second: a click on its button
    /// right after it opens has to wait for the button to stop moving.
    fn open_dialog(window: &mut Window, cx: &mut Context<Self>) {
        let harness = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let harness = harness.clone();
            dialog
                .title("Confirm")
                .child("Slides in as it opens.")
                .footer(
                    Button::new("dialog-confirm")
                        .primary()
                        .label("Confirm")
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            harness
                                .update(cx, |this, cx| {
                                    this.dialog_confirms += 1;
                                    cx.notify();
                                })
                                .ok();
                        })
                        .automation_target("dialog/confirm"),
                )
        });
    }

    fn on_target_drag_move(&mut self, event: &DragMoveEvent<DraggedItem>, cx: &mut Context<Self>) {
        let inside = event.bounds.contains(&event.event.position);
        if inside == self.target_hovered {
            return;
        }
        self.target_hovered = inside;
        self.hover_timer = inside.then(|| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(HOVER_EXPAND).await;
                this.update(cx, |this, cx| {
                    this.target_expanded = true;
                    cx.notify();
                })
                .ok();
            })
        });
        cx.notify();
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected_row == Some(index);
        div()
            .id(("row", index))
            .h(px(28.))
            .px_2()
            .flex()
            .items_center()
            .when_else(
                selected,
                |row| row.bg(rgb(0x2563eb)),
                |row| row.hover(|style| style.bg(rgb(0x2a3140))),
            )
            .child(format!("Row {index}"))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected_row = Some(index);
                cx.notify();
            }))
            .automation_target_with(|| format!("list/row/{index}").into())
    }
}

impl Render for Harness {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dropped = if self.dropped.is_empty() {
            SharedString::from("Drop files here")
        } else {
            SharedString::from(format!("Dropped: {}", self.dropped.join(", ")))
        };
        let target_label = match (self.item_in_target, self.target_expanded) {
            (true, _) => "Item dropped here",
            (false, true) => "Drop target (expanded)",
            (false, false) => "Drop target",
        };

        div()
            .id("harness")
            .key_context("Harness")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Increment, _, cx| {
                this.counter += 1;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Reset, _, cx| {
                this.counter = 0;
                this.resets += 1;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenSecondWindow, _, cx| {
                this.open_second_window(cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(rgb(0x161a22))
            .text_color(rgb(0xe6e6e6))
            .child(gpui_automation::pointer_guard())
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        Button::new("increment")
                            .primary()
                            .label("Increment")
                            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                // ctrl-click proves modifiers reach handlers.
                                this.counter += if event.modifiers().control { 10 } else { 1 };
                                cx.notify();
                            }))
                            .automation_target("counter/increment"),
                    )
                    .child(
                        div()
                            .id("counter-value")
                            .px_2()
                            .child(format!("Counter: {}", self.counter))
                            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                if event.click_count() == 2 {
                                    this.double_clicks += 1;
                                    cx.notify();
                                }
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(|this, _, _, cx| {
                                    this.right_clicks += 1;
                                    cx.notify();
                                }),
                            )
                            .automation_target("counter/value"),
                    )
                    .child(
                        Button::new("slow")
                            .label(if self.slow.is_some() {
                                "Working..."
                            } else {
                                "Slow +1"
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.start_slow_increment(cx)))
                            .automation_target("counter/slow"),
                    )
                    .child(
                        Button::new("open-second")
                            .label("Open second window")
                            .on_click(cx.listener(|this, _, _, cx| this.open_second_window(cx)))
                            .automation_target("second/open"),
                    )
                    .child(
                        Button::new("open-dialog")
                            .label("Open dialog")
                            .on_click(cx.listener(|_, _, window, cx| Self::open_dialog(window, cx)))
                            .automation_target("dialog/open"),
                    ),
            )
            .child(
                div()
                    .w(px(420.))
                    .child(Input::new(&self.input))
                    .automation_target("input"),
            )
            .child(
                div()
                    .id("drop-zone")
                    .h(px(64.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_2()
                    .border_dashed()
                    .border_color(rgb(0x4b5563))
                    .rounded_md()
                    .drag_over::<ExternalPaths>(|style, _, _, _| {
                        style.bg(rgb(0x14532d)).border_color(rgb(0x22c55e))
                    })
                    .on_drag_move(cx.listener(
                        |this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                            let inside = event.bounds.contains(&event.event.position);
                            if inside != this.drop_zone_hovered {
                                this.drop_zone_hovered = inside;
                                cx.notify();
                            }
                        },
                    ))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                        this.drop_zone_hovered = false;
                        this.dropped.extend(paths.paths().iter().map(|path| {
                            path.file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.display().to_string())
                        }));
                        cx.notify();
                    }))
                    .child(dropped)
                    .automation_target("drop-zone"),
            )
            .child(
                h_flex()
                    .gap_6()
                    .child(
                        div()
                            .id("drag-item")
                            .w(px(140.))
                            .h(px(48.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .bg(rgb(0x374151))
                            .cursor_grab()
                            .when(self.item_hovered, |item| item.bg(rgb(0x4b5563)))
                            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                                this.item_hovered = *hovered;
                                cx.notify();
                            }))
                            .on_drag(DraggedItem, |_, _, _, cx| cx.new(|_| DragPreview))
                            .child(if self.item_in_target {
                                "(moved)"
                            } else {
                                "Drag me"
                            })
                            .automation_target("drag/item"),
                    )
                    .child(
                        div()
                            .id("drop-target")
                            .w(px(260.))
                            .h(if self.target_expanded {
                                px(96.)
                            } else {
                                px(48.)
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .border_2()
                            .border_color(rgb(0x4b5563))
                            .drag_over::<DraggedItem>(|style, _, _, _| {
                                style.bg(rgb(0x1e3a8a)).border_color(rgb(0x60a5fa))
                            })
                            .on_drag_move(cx.listener(
                                |this, event: &DragMoveEvent<DraggedItem>, _, cx| {
                                    this.on_target_drag_move(event, cx)
                                },
                            ))
                            .on_drop(cx.listener(|this, _: &DraggedItem, _, cx| {
                                this.item_in_target = true;
                                this.target_hovered = false;
                                this.hover_timer = None;
                                cx.notify();
                            }))
                            .child(target_label)
                            .automation_target("drag/target"),
                    ),
            )
            .child(
                div()
                    .id("list")
                    .w(px(320.))
                    .h(px(196.))
                    .border_1()
                    .border_color(rgb(0x4b5563))
                    .overflow_y_scroll()
                    .track_scroll(&self.list_scroll)
                    .children((0..ROWS).map(|index| self.render_row(index, cx)))
                    .automation_target("list"),
            )
    }
}

impl Focusable for Harness {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AutomationHost for Harness {
    fn state(&self, window: &Window, cx: &App) -> Value {
        if let Some((handle, view)) = self.second_open(cx)
            && window.window_handle() == (*handle).into()
        {
            return json!({"view": "second", "clicks": view.read(cx).clicks});
        }
        let dragging = cx.has_active_drag();
        let env: serde_json::Map<String, Value> = RUNNER_ENV
            .iter()
            .map(|name| ((*name).to_string(), json!(std::env::var(name).ok())))
            .collect();
        json!({
            "view": "main",
            "counter": self.counter,
            "resets": self.resets,
            "double_clicks": self.double_clicks,
            "right_clicks": self.right_clicks,
            "dialog_confirms": self.dialog_confirms,
            "slow_pending": self.slow.is_some(),
            "input": {
                "value": self.input.read(cx).value(),
                "focused": self.input.focus_handle(cx).is_focused(window),
            },
            "dropped": self.dropped,
            "drop_zone": {"hovered": dragging && self.drop_zone_hovered},
            "drag": {
                "active": dragging,
                "item_hovered": self.item_hovered,
                "target_hovered": dragging && self.target_hovered,
                "target_expanded": self.target_expanded,
                "item_in_target": self.item_in_target,
            },
            "list": {
                // Offsets are negative when scrolled; `0.0 -` avoids "-0.0".
                "scroll_y": 0.0 - f32::from(self.list_scroll.offset().y),
                "selected": self.selected_row,
            },
            "second": {
                "open": self.second_open(cx).is_some(),
                "clicks": self.second_open(cx).map(|(_, view)| view.read(cx).clicks),
            },
            "data_dir": self.data_dir.as_ref().map(|dir| dir.display().to_string()),
            // Let runner tests check that runs are hermetic without dv itself:
            // the data dir, where `gh` calls go and their log, the git
            // ceiling (`null` when unset), and the working directory (the
            // data dir, not this checkout).
            "env": env,
            "cwd": std::env::current_dir().ok().map(|dir| dir.display().to_string()),
        })
    }

    fn settled(&self, _: &App) -> bool {
        self.slow.is_none()
    }

    fn custom(
        &mut self,
        cmd: &str,
        args: Value,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Result<Value>> {
        Some(match cmd {
            "set_counter" => args
                .get("value")
                .and_then(Value::as_i64)
                .ok_or_else(|| anyhow!("set_counter needs an integer value"))
                .map(|value| {
                    self.counter = value;
                    cx.notify();
                    json!({"counter": value})
                }),
            "echo" => Ok(args),
            "reset_drag" => {
                self.item_in_target = false;
                self.target_expanded = false;
                self.target_hovered = false;
                self.hover_timer = None;
                cx.notify();
                Ok(json!({"reset": true}))
            }
            "fail" => Err(anyhow!("failed on purpose")),
            _ => return None,
        })
    }

    /// `slow_set_counter{value, ms}` sets the counter after a timer, so the
    /// reply has to wait for the task; a negative value fails it instead.
    /// `run_gh{args, stdin?}` runs `$DV_GH` off the UI thread and replies
    /// with its exit code and output.
    fn custom_task(
        &mut self,
        cmd: &str,
        args: Value,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<Value>>> {
        if cmd == "run_gh" {
            return Some(cx.background_spawn(async move { run_gh(&args) }));
        }
        if cmd != "slow_set_counter" {
            return None;
        }
        let value = args.get("value").and_then(Value::as_i64);
        let delay = Duration::from_millis(args.get("ms").and_then(Value::as_u64).unwrap_or(100));
        Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let value = value.ok_or_else(|| anyhow!("slow_set_counter needs an integer value"))?;
            if value < 0 {
                bail!("{value} is negative");
            }
            this.update(cx, |this, cx| {
                this.counter = value;
                cx.notify();
            })?;
            Ok(json!({"counter": value}))
        }))
    }
}

/// Runs `$DV_GH <args>` with `stdin` (if any), as dv runs `gh` (docs/PLAN.md
/// D7: `GH_PROMPT_DISABLED=1`), and returns `{exit, stdout, stderr}`. `exit`
/// is `null` when the process ended without a code.
fn run_gh(args: &Value) -> Result<Value> {
    let program = std::env::var_os("DV_GH").context("DV_GH is not set")?;
    let argv = match args.get("args") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .context("run_gh args must be strings")
            })
            .collect::<Result<Vec<_>>>()?,
        Some(other) => bail!("run_gh args must be a list of strings, got {other}"),
    };
    let stdin = match args.get("stdin") {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => bail!("run_gh stdin must be a string, got {other}"),
    };
    let mut child = Command::new(&program)
        .args(&argv)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", program.to_string_lossy()))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A gh that exits without reading its input closes the pipe; what it
        // printed and its exit code still tell the scenario what happened.
        match pipe.write_all(text.as_bytes()) {
            Err(err) if err.kind() != std::io::ErrorKind::BrokenPipe => {
                return Err(err).context("writing run_gh stdin");
            }
            _ => {}
        }
    }
    let output = child.wait_with_output().context("waiting for gh")?;
    Ok(json!({
        "exit": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

/// Logs to stderr so stdout stays reserved for the automation protocol.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("[{} {}] {}", record.level(), record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

fn main() -> Result<()> {
    log::set_logger(&LOGGER).map_err(|err| anyhow!("installing the logger: {err}"))?;
    log::set_max_level(log::LevelFilter::Info);
    let args = parse_args()?;

    gpui_platform::application().run(move |cx| {
        if let Err(err) = launch(args, cx) {
            log::error!("harness failed to start: {err:#}");
            cx.quit();
        }
    });
    Ok(())
}

fn launch(args: Args, cx: &mut App) -> Result<()> {
    gpui_component::init(cx);
    gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
    cx.bind_keys([
        KeyBinding::new("ctrl-i", Increment, Some("Harness")),
        KeyBinding::new("ctrl-k ctrl-r", Reset, Some("Harness")),
        KeyBinding::new("ctrl-shift-n", OpenSecondWindow, Some("Harness")),
    ]);

    let (w, h) = args.window_size.unwrap_or((1200., 800.));
    let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
    let options = WindowOptions {
        window_bounds: Some(if args.maximized {
            WindowBounds::Maximized(bounds)
        } else {
            WindowBounds::Windowed(bounds)
        }),
        ..Default::default()
    };
    let data_dir = args.data_dir;
    let counter = args.counter;
    let mut harness = None;
    gpui_automation::set_background(args.automation_background);
    let window = gpui_automation::open_window(cx, options, |window, cx| {
        let view = cx.new(|cx| Harness::new(data_dir, counter, window, cx));
        harness = Some(view.clone());
        cx.new(|cx| Root::new(view, window, cx))
    })?;
    let harness = harness.context("the harness view was not created")?;

    // Closing the main window ends the app; the second window may come and go.
    let main_id = window.window_id();
    cx.on_window_closed(move |cx, closed| {
        if closed == main_id {
            cx.quit();
        }
    })
    .detach();

    if args.automation || args.automation_pipe.is_some() {
        let options = Options {
            pipe: args.automation_pipe,
            ..Options::default()
        };
        gpui_automation::start(window.into(), harness, cx, options)?;
    }
    if !gpui_automation::is_background() {
        cx.activate(true);
    }
    Ok(())
}
