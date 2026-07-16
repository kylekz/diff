//! `dv --automation`: a JSON-over-stdio control channel so an agent can
//! drive and inspect the running app — dump semantic state, dispatch any
//! registered gpui action, send keystrokes and clicks, resize, and write a
//! window screenshot to a PNG path. This is the primary iteration loop for
//! UI/style work (docs/architecture.md § Testing): launch, command,
//! screenshot, inspect, repeat — no human in the loop.
//!
//! Protocol: one JSON object per line on stdin, one JSON response per line
//! on stdout. Requests may carry an `"id"` which is echoed back. On startup
//! the app emits `{"event":"ready"}` once the channel is listening. EOF on
//! stdin quits the app (the driver going away must not leave zombie
//! windows).

pub mod capture;

use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, Keystroke, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, WindowHandle, point, px, size,
};
use gpui_component::Root;
use serde_json::{Value, json};

use crate::shell::AppShell;

/// One decoded automation command. `cmd` selects the variant; the rest of
/// the object supplies its fields.
#[derive(serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Cmd {
    /// Semantic state dump: window size/scale, sidebar entries, and the
    /// active review (files, selection, view mode, row counts).
    State,
    /// List every registered gpui action name.
    Actions,
    /// Dispatch an action by name ("workspace::ToggleSplit", or a unique
    /// short name like "ToggleSplit") to the focused element.
    Action {
        name: String,
        #[serde(default)]
        data: Option<Value>,
    },
    /// Send a keystroke ("ctrl-n", "s") through the normal keymap path.
    Key { keystroke: String },
    /// Synthesize a mouse click at window coordinates (logical pixels).
    Click {
        x: f32,
        y: f32,
        #[serde(default)]
        button: Option<String>,
        /// Hold shift during the click (range selection etc.).
        #[serde(default)]
        shift: bool,
    },
    /// Resize the window content area (logical pixels).
    Resize { w: f32, h: f32 },
    /// Synthesize a click-drag from `from` to `to` (window coordinates,
    /// logical pixels) through the real input path — mouse-down at `from`,
    /// several incremental moves out to `to`, then mouse-up — so gpui's
    /// `on_drag`/`on_drag_move` machinery (drag handles: sidebar/summary
    /// panel resize, Phase 4 deliverable 5) actually engages. A single
    /// straight jump from `from` to `to` isn't enough: gpui only starts
    /// treating a mouse-down + move as a "drag" once the move exceeds a 2px
    /// threshold (see gpui's `elements/div.rs`, `DRAG_THRESHOLD`), and the
    /// very frame that crosses that threshold is the one that flips
    /// `cx.active_drag` on — it isn't itself delivered to `on_drag_move`
    /// listeners (those only fire once a drag is *already* active). Several
    /// intermediate steps sidesteps both: the first step reliably crosses
    /// the threshold, and every later one lands on an `on_drag_move` frame.
    Drag { from: (f32, f32), to: (f32, f32) },
    /// Open a working-tree review of a repo path (the scripted stand-in for
    /// the New Review folder picker, which is disabled under automation
    /// because the native dialog would block the foreground executor).
    Open { path: String },
    /// Select the nth changed file in the active review.
    SelectFile { index: usize },
    /// Explicit sidebar-row selection by review id
    /// (docs/phase-6-review-navigator.md S6c) — the scripted stand-in for a
    /// review card click, and the incident-fix entry point: pins and
    /// reopens that review (`AppShell::open_review_row`), including a
    /// SUBMITTED one, read-only. `id`s come from `state.shell.index[].
    /// review_id` or `state.shell.sidebar`.
    SelectReview { id: String },
    /// Open PR `number` in the active review's workspace
    /// (`Workspace::open_pr`) — replies once the fetch is dispatched, not
    /// once it completes; scripts follow with `wait_ready`.
    OpenPr { number: u64 },
    /// Apply one setting through the same code path the settings panel's
    /// own controls use (`AppShell::automation_set_setting`) — `key`
    /// matches `Settings`'s JSON field names (`theme`, `mono_font_size`,
    /// `view_mode_default`, ...).
    SetSetting { key: String, value: Value },
    /// Click the onboarding page's consent-install button on the row at
    /// index `row` (`state.shell.onboarding.rows[row]`), the scripted
    /// stand-in for clicking "Install" (`AppShell::
    /// automation_onboarding_consent`) — a coordinate `click` isn't
    /// deterministic since the button's position depends on how many rows
    /// precede it.
    OnboardingConsent { row: usize },
    /// Write a PNG of the window to `path`; responds with physical pixel
    /// dimensions.
    Screenshot { path: PathBuf },
    /// Sleep, for pacing scripted sessions. Capped at [`MAX_WAIT_MS`]: the
    /// stdin-EOF quit only runs after the in-flight command, so an unbounded
    /// wait could leave a zombie window long after the driver died.
    Wait { ms: u64 },
    /// Block until the app is settled (repo loaded, selected diff computed)
    /// or the timeout elapses. Makes one-shot piped scripts deterministic.
    WaitReady {
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// Quit the app (also triggered by stdin EOF).
    Quit,
}

/// Upper bound on `wait`/`wait_ready` durations — bounds how long a dead
/// driver's window can linger, since EOF-quit queues behind the in-flight
/// command.
const MAX_WAIT_MS: u64 = 60_000;

/// Wire up the channel: a thread pumps stdin lines into the foreground
/// executor, which handles commands strictly in order (a command finishes
/// before the next is read, so scripts need no client-side pacing).
pub fn start(window: WindowHandle<Root>, shell: Entity<AppShell>, cx: &mut App) {
    let (tx, mut rx) = mpsc::unbounded::<String>();
    std::thread::Builder::new()
        .name("automation-stdin".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                if tx.unbounded_send(line).is_err() {
                    return; // app side is gone
                }
            }
            // EOF: the driver hung up — quit rather than linger headless.
            tx.unbounded_send(r#"{"cmd":"quit"}"#.into()).ok();
        })
        .expect("failed to spawn automation stdin thread");

    // Erase the root-view type: handlers must NOT lease the Root entity
    // (`WindowHandle::update` does), because dispatching input re-enters
    // Root internally and gpui panics on the double lease.
    let window: AnyWindowHandle = window.into();
    cx.spawn(async move |cx| {
        emit(&json!({"event": "ready"}));
        while let Some(line) = rx.next().await {
            let (id, cmd) = parse_line(&line);
            let quit = matches!(cmd, Ok(Cmd::Quit));
            let result = match cmd {
                Ok(cmd) => handle(cmd, window, &shell, cx).await,
                Err(message) => Err(anyhow!(message)),
            };
            emit(&response(id, result));
            if quit {
                cx.update(|cx| cx.quit());
                break;
            }
        }
    })
    .detach();
}

/// Split a request line into its echoed `id` and the decoded command, so a
/// malformed command still produces a response the driver can correlate.
fn parse_line(line: &str) -> (Option<Value>, Result<Cmd, String>) {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(err) => return (None, Err(format!("invalid JSON: {err}"))),
    };
    let id = value.get("id").cloned();
    let cmd = serde_json::from_value(value).map_err(|err| format!("bad command: {err}"));
    (id, cmd)
}

fn response(id: Option<Value>, result: anyhow::Result<Value>) -> Value {
    let mut body = match result {
        Ok(data) => json!({"ok": true, "data": data}),
        Err(err) => json!({"ok": false, "error": format!("{err:#}")}),
    };
    if let Some(id) = id {
        body["id"] = id;
    }
    body
}

fn emit(value: &Value) {
    let mut out = std::io::stdout().lock();
    // A failed write means the driver is gone; nothing useful to do about it.
    let _ = serde_json::to_writer(&mut out, value);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

async fn handle(
    cmd: Cmd,
    window: AnyWindowHandle,
    shell: &Entity<AppShell>,
    cx: &mut AsyncApp,
) -> anyhow::Result<Value> {
    match cmd {
        Cmd::State => cx.update_window(window, |_, window, cx| {
            let viewport = window.viewport_size();
            // The `focus` field (Phase 7 D0) needs `window` to resolve
            // `is_focused`, which `AppShell::automation_state` doesn't take
            // — inject it here, where both `window` and `cx` are in scope.
            let mut shell_state = shell.read(cx).automation_state(cx);
            shell_state["focus"] = json!(shell.read(cx).focus_label(window, cx));
            json!({
                "window": {
                    "w": f32::from(viewport.width),
                    "h": f32::from(viewport.height),
                    "scale": window.scale_factor(),
                },
                "shell": shell_state,
            })
        }),

        Cmd::Actions => Ok(cx.update(|cx| json!(cx.all_action_names()))),

        Cmd::Action { name, data } => {
            cx.update_window(window, |_, window, cx| -> anyhow::Result<Value> {
                let full = resolve_action_name(cx.all_action_names(), &name)?;
                let action = cx
                    .build_action(&full, data)
                    .map_err(|err| anyhow!("building action {full}: {err}"))?;
                window.dispatch_action(action, cx);
                Ok(json!({"action": full}))
            })?
        }

        Cmd::Key { keystroke } => {
            let keystroke = Keystroke::parse(&keystroke)
                .map_err(|err| anyhow!("bad keystroke {keystroke:?}: {err:?}"))?;
            cx.update_window(window, |_, window, cx| {
                let handled = window.dispatch_keystroke(keystroke, cx);
                json!({"handled": handled})
            })
        }

        Cmd::Click {
            x,
            y,
            button,
            shift,
        } => {
            let button = match button.as_deref() {
                None | Some("left") => MouseButton::Left,
                Some("right") => MouseButton::Right,
                Some("middle") => MouseButton::Middle,
                Some(other) => bail!("unknown button: {other}"),
            };
            cx.update_window(window, |_, window, cx| {
                let position = point(px(x), px(y));
                let modifiers = Modifiers {
                    shift,
                    ..Modifiers::default()
                };
                // Move first so hover state matches what a real click sees.
                window.dispatch_event(
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: None,
                        modifiers,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        button,
                        position,
                        modifiers,
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        button,
                        position,
                        modifiers,
                        click_count: 1,
                    }),
                    cx,
                );
                json!({"clicked": {"x": x, "y": y}})
            })
        }

        Cmd::Resize { w, h } => cx.update_window(window, |_, window, _| {
            window.resize(size(px(w), px(h)));
            json!({"requested": {"w": w, "h": h}})
        }),

        Cmd::Drag { from, to } => cx.update_window(window, |_, window, cx| {
            let (fx, fy) = from;
            let (tx, ty) = to;
            let modifiers = Modifiers::default();
            let start = point(px(fx), px(fy));
            let end = point(px(tx), px(ty));
            // Hover onto the handle first (matches Click's own "move before
            // mouse-down" so hover-gated styles/cursors are already right),
            // then press.
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: start,
                    pressed_button: None,
                    modifiers,
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: start,
                    modifiers,
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            // Step toward `to` rather than jumping straight there — see
            // `Cmd::Drag`'s own doc comment for why a single move isn't
            // enough to both cross gpui's drag threshold *and* land an
            // `on_drag_move` frame.
            const STEPS: i32 = 8;
            for step in 1..=STEPS {
                let t = step as f32 / STEPS as f32;
                let position = point(px(fx + (tx - fx) * t), px(fy + (ty - fy) * t));
                window.dispatch_event(
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: Some(MouseButton::Left),
                        modifiers,
                    }),
                    cx,
                );
            }
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position: end,
                    modifiers,
                    click_count: 1,
                }),
                cx,
            );
            json!({"dragged": {"from": [fx, fy], "to": [tx, ty]}})
        }),

        Cmd::Open { path } => {
            cx.update_window(window, |_, window, cx| -> anyhow::Result<Value> {
                let location = dv_core::RepoLocation::from_path_arg(&path)
                    .map_err(|err| anyhow!("{err:#}"))?;
                shell.update(cx, |shell, cx| shell.automation_open(location, window, cx));
                Ok(json!({"opened": path}))
            })?
        }

        Cmd::SelectFile { index } => cx.update_window(window, |_, window, cx| {
            shell.update(cx, |shell, cx| {
                shell
                    .automation_select_file(index, window, cx)
                    .map(|()| json!({"selected": index}))
            })
        })?,

        Cmd::SelectReview { id } => cx.update_window(window, |_, window, cx| {
            shell.update(cx, |shell, cx| {
                shell
                    .automation_select_review(id.clone(), window, cx)
                    .map(|()| json!({"selected_review": id}))
            })
        })?,

        Cmd::OpenPr { number } => cx.update_window(window, |_, window, cx| {
            shell.update(cx, |shell, cx| {
                shell
                    .automation_open_pr(number, window, cx)
                    .map(|()| json!({"opened_pr": number}))
            })
        })?,

        Cmd::SetSetting { key, value } => cx.update_window(window, |_, window, cx| {
            shell.update(cx, |shell, cx| {
                shell
                    .automation_set_setting(&key, value, window, cx)
                    .map(|()| json!({"key": key}))
            })
        })?,

        Cmd::OnboardingConsent { row } => cx.update_window(window, |_, _, cx| {
            shell.update(cx, |shell, cx| {
                shell
                    .automation_onboarding_consent(row, cx)
                    .map(|()| json!({"onboarding_consent": row}))
            })
        })?,

        Cmd::Screenshot { path } => {
            // Ask for a fresh frame, give the compositor a beat, then read
            // the pixels back via DWM (capture.rs).
            cx.update_window(window, |_, window, _| window.refresh())?;
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let shown = path.display().to_string();
            let (w, h) = cx
                .background_executor()
                .spawn(async move { capture::capture_window_png(&path) })
                .await?;
            Ok(json!({"path": shown, "w": w, "h": h}))
        }

        Cmd::Wait { ms } => {
            if ms > MAX_WAIT_MS {
                bail!("wait ms is capped at {MAX_WAIT_MS}");
            }
            cx.background_executor()
                .timer(Duration::from_millis(ms))
                .await;
            Ok(json!({"waited_ms": ms}))
        }

        Cmd::WaitReady { timeout_ms } => {
            let timeout_ms = timeout_ms.unwrap_or(15_000);
            if timeout_ms > MAX_WAIT_MS {
                bail!("wait_ready timeout_ms is capped at {MAX_WAIT_MS}");
            }
            let timeout = Duration::from_millis(timeout_ms);
            let started = Instant::now();
            loop {
                let settled = cx.update(|cx| shell.read(cx).automation_settled(cx));
                if settled {
                    let elapsed = started.elapsed().as_millis() as u64;
                    return Ok(json!({"ready": true, "elapsed_ms": elapsed}));
                }
                if started.elapsed() > timeout {
                    bail!("not ready after {}ms", timeout.as_millis());
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
        }

        Cmd::Quit => Ok(json!({"quitting": true})), // caller quits after replying
    }
}

/// Accept either a full action name or a unique short name: "ToggleSplit"
/// resolves to "workspace::ToggleSplit" as long as no other namespace also
/// registers a ToggleSplit.
fn resolve_action_name(names: &[&'static str], query: &str) -> anyhow::Result<String> {
    if names.contains(&query) {
        return Ok(query.to_string());
    }
    let suffix = format!("::{query}");
    let matches: Vec<&&str> = names
        .iter()
        .filter(|name| name.ends_with(&suffix))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.to_string()),
        [] => bail!("unknown action: {query}"),
        many => bail!(
            "ambiguous action {query}: {}",
            many.iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{Cmd, parse_line, resolve_action_name};

    #[test]
    fn parse_line_echoes_id_and_decodes() {
        let (id, cmd) = parse_line(r#"{"id": 7, "cmd": "state"}"#);
        assert_eq!(id, Some(serde_json::json!(7)));
        assert!(matches!(cmd, Ok(Cmd::State)));
    }

    #[test]
    fn parse_line_reports_bad_command_with_id() {
        let (id, cmd) = parse_line(r#"{"id": "a", "cmd": "explode"}"#);
        assert_eq!(id, Some(serde_json::json!("a")));
        assert!(cmd.is_err());
    }

    #[test]
    fn parse_line_reports_invalid_json() {
        let (id, cmd) = parse_line("not json");
        assert!(id.is_none());
        assert!(cmd.is_err());
    }

    #[test]
    fn action_names_resolve_by_unique_suffix() {
        let names: &[&'static str] = &["workspace::ToggleSplit", "shell::NewReview"];
        assert_eq!(
            resolve_action_name(names, "ToggleSplit").unwrap(),
            "workspace::ToggleSplit"
        );
        assert_eq!(
            resolve_action_name(names, "shell::NewReview").unwrap(),
            "shell::NewReview"
        );
        assert!(resolve_action_name(names, "Nope").is_err());
    }

    #[test]
    fn ambiguous_suffix_is_rejected() {
        let names: &[&'static str] = &["a::Go", "b::Go"];
        let err = resolve_action_name(names, "Go").unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
    }
}
