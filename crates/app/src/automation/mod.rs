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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
        /// Hold shift during the click (range selection etc.). Kept
        /// alongside `modifiers` (rather than folded into it) for backward
        /// compatibility with existing scripts; the two OR together.
        #[serde(default)]
        shift: bool,
        /// Dash-separated modifier names held during the click — e.g.
        /// `"ctrl"`, `"cmd"`, `"ctrl-shift"` (S8g: exercising ctrl/cmd-click
        /// go-to-definition, which reads `MouseDownEvent::modifiers.secondary()`,
        /// end-to-end needed a way to hold a real modifier through the
        /// synthesized click — see [`parse_modifiers`]). Accepted names:
        /// `ctrl`/`control`, `alt`/`option`, `shift`, `cmd`/`platform`/
        /// `super`/`win`, `fn`/`function`.
        #[serde(default)]
        modifiers: Option<String>,
    },
    /// Move the mouse to window coordinates (logical pixels) with no button
    /// pressed — the scripted stand-in for a hover (S8g: triggers the diff
    /// pane's hover-popover path the same real `MouseMoveEvent` a physical
    /// mouse sweep would, unlike `Cmd::Click`'s own internal move-then-click,
    /// which never lingers). Follow with `wait` (past the hover debounce)
    /// then `state`/`screenshot` — this command itself only dispatches the
    /// move and returns immediately.
    MouseMove { x: f32, y: f32 },
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
    /// `review_id`, not `id` — the request envelope already uses `id` for
    /// the protocol sequence number and the command fields parse from the
    /// same flattened map, so a payload field literally named `id` can
    /// never coexist with a sequence number (early scripts only "worked"
    /// via duplicate-key last-wins, which silently ate the sequence
    /// number; no alias either — serde would report the envelope's `id`
    /// as a duplicate of it).
    SelectReview { review_id: String },
    /// Open PR `number` in the active review's workspace
    /// (`Workspace::open_pr`) — replies once the fetch is dispatched, not
    /// once it completes; scripts follow with `wait_ready`.
    OpenPr { number: u64 },
    /// Apply one setting through the same code path the settings panel's
    /// own controls use (`AppShell::automation_set_setting`) — `key`
    /// matches `Settings`'s JSON field names (`theme`, `mono_font_size`,
    /// `view_mode_default`, ...).
    SetSetting { key: String, value: Value },
    /// Set the "open anything" quick-open input's text and submit it —
    /// same parse/resolve path as typing + Enter
    /// (`AppShell::quick_open_submit`); assert the outcome via
    /// `state.shell.quick_open_error` (null on success) plus the usual
    /// workspace fields after a `wait_ready`.
    QuickOpen { text: String },
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
    /// Sleep, for pacing scripted sessions. Capped at [`MAX_WAIT_MS`] and
    /// polled in short steps against [`quit_now`] — a stdin EOF while this
    /// is the last thing the driver ever sent (nothing else queued behind
    /// it — the backlog's target case: a huge `ms` right before the driver
    /// closes the pipe) interrupts it within roughly one poll step rather
    /// than running out the full duration. An ordinary script's `wait`
    /// mid-sequence (more real commands already queued behind it) is
    /// unaffected even though the file itself was fully read long ago.
    Wait { ms: u64 },
    /// Block until the app is settled (repo loaded, selected diff computed)
    /// or the timeout elapses. Makes one-shot piped scripts deterministic.
    /// Same [`quit_now`] polling as `Wait` — a huge `timeout_ms` that's the
    /// last thing the driver ever sent still exits promptly once stdin
    /// hits EOF, without affecting an ordinary script's leading
    /// `wait_ready` (real commands still queued behind it).
    WaitReady {
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// Quit the app (also triggered by stdin EOF).
    Quit,
}

/// Upper bound on `wait`/`wait_ready` durations — a backstop bounding how
/// long a dead driver's window could linger even if [`QUIT_REQUESTED`]
/// somehow went unnoticed; the polling below is what actually makes EOF
/// interrupt these promptly instead of relying on this cap.
const MAX_WAIT_MS: u64 = 60_000;

/// How often `Wait`/`WaitReady` re-check [`QUIT_REQUESTED`] while blocked —
/// the upper bound on how late a stdin-EOF quit can be noticed.
const QUIT_POLL_MS: u64 = 100;

/// `true` once [`start`] has wired the channel — i.e. this process is being
/// driven by a script, not a mouse. Exists for the one place synthetic
/// input and platform reality disagree: `Window::is_window_hovered()` is a
/// PLATFORM-level signal (WM_MOUSELEAVE-tracked, real OS cursor), so it
/// stays `false` for a scripted `mouse_move` no matter what events we
/// dispatch — which would make every hover-popover raise self-discard
/// under automation (the real cursor sits over the driver's terminal).
/// `Workspace`'s hover completion consults this to skip that one check
/// when scripted; every other input path hit-tests dispatched positions
/// and needs no such carve-out.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether this process is running under `--automation` (see [`ACTIVE`]).
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Set by the stdin-pump thread the instant it sees EOF (backlog: "automation
/// EOF-quit races long commands"). By itself this is NOT enough to
/// interrupt a long-blocking `Wait`/`WaitReady`: reading a redirected FILE
/// (the documented `< cmds.jsonl` usage) is effectively instantaneous, so
/// the pump thread races far ahead of the main command loop and this flag
/// is already `true` before the very first queued command even starts —
/// unconditionally bailing on it would wrongly cut short every ordinary
/// script's first `wait_ready`. [`PENDING`] is what disambiguates a
/// finished, well-formed script (more real commands already queued behind
/// the current one — not abandoned) from a genuinely abandoned wait (this
/// is the last thing the driver ever sent, and it's now gone) — see that
/// static's doc comment. Never cleared: a single process is only ever
/// driven by one stdin, and once EOF happened there is nothing left to
/// un-quit for.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Count of REAL (driver-sent, not the synthetic EOF quit) command lines
/// the stdin-pump thread has handed to the channel but the main command
/// loop hasn't dequeued yet. The pump thread increments this for every
/// line it reads from stdin (before sending it); the main loop decrements
/// it (saturating — the untracked synthetic quit line must never drive
/// this negative) the moment it dequeues ANY line, real or synthetic,
/// before dispatching it.
///
/// Combined with [`QUIT_REQUESTED`], `PENDING == 0` means "nothing else
/// the driver sent is still waiting behind the command currently running"
/// — for a normal multi-line script that's only ever true once the FINAL
/// line (by convention an explicit `quit`, per every committed automation
/// script and CLAUDE.md's own template) is what's dispatching, so an
/// ordinary script's `wait_ready`/`wait` calls run untouched even though
/// the file itself was fully read, and `QUIT_REQUESTED`, in an eyeblink.
/// Only a `Wait`/`WaitReady` that truly is the last thing ever sent (the
/// backlog's target case: a script issues one huge-timeout wait then
/// closes stdin with nothing queued after it) sees `PENDING == 0` while
/// still in flight, and interrupts promptly instead of running out its
/// full duration.
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// `Wait`/`WaitReady` poll this — see [`QUIT_REQUESTED`] and [`PENDING`]'s
/// doc comments for why both conditions are required.
fn quit_now() -> bool {
    QUIT_REQUESTED.load(Ordering::Relaxed) && PENDING.load(Ordering::Relaxed) == 0
}

/// Wire up the channel: a thread pumps stdin lines into the foreground
/// executor, which handles commands strictly in order (a command finishes
/// before the next is read, so scripts need no client-side pacing).
pub fn start(window: WindowHandle<Root>, shell: Entity<AppShell>, cx: &mut App) {
    ACTIVE.store(true, Ordering::Relaxed);
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
                // Counted BEFORE sending — see `PENDING`'s doc comment: the
                // main loop must never observe a real line as "sent" before
                // it's reflected here, or a `PENDING == 0` false negative
                // could let a genuinely-last wait run its full timeout.
                PENDING.fetch_add(1, Ordering::Relaxed);
                if tx.unbounded_send(line).is_err() {
                    return; // app side is gone
                }
            }
            // EOF: the driver hung up — quit rather than linger headless.
            // Flip the flag so a `Wait`/`WaitReady` that's already the last
            // real command in flight (see `PENDING`) notices on its very
            // next poll, rather than only learning about it once this
            // synthetic quit (deliberately NOT counted in `PENDING`) is
            // dequeued behind whatever's currently running.
            QUIT_REQUESTED.store(true, Ordering::Relaxed);
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
            // Saturating: the synthetic quit line was never counted (it's
            // not a real driver command), so dequeuing it here must not
            // drive a real, already-zeroed count negative.
            PENDING
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    Some(n.saturating_sub(1))
                })
                .ok();
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
            modifiers,
        } => {
            let button = match button.as_deref() {
                None | Some("left") => MouseButton::Left,
                Some("right") => MouseButton::Right,
                Some("middle") => MouseButton::Middle,
                Some(other) => bail!("unknown button: {other}"),
            };
            let mut modifiers = match modifiers.as_deref() {
                Some(spec) => parse_modifiers(spec)?,
                None => Modifiers::default(),
            };
            modifiers.shift |= shift; // OR, not overwrite — see the field's own doc comment
            cx.update_window(window, |_, window, cx| {
                let position = point(px(x), px(y));
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

        Cmd::MouseMove { x, y } => cx.update_window(window, |_, window, cx| {
            let position = point(px(x), px(y));
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button: None,
                    modifiers: Modifiers::default(),
                }),
                cx,
            );
            json!({"moved": {"x": x, "y": y}})
        }),

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

        Cmd::SelectReview { review_id } => cx.update_window(window, |_, window, cx| {
            shell.update(cx, |shell, cx| {
                shell
                    .automation_select_review(review_id.clone(), window, cx)
                    .map(|()| json!({"selected_review": review_id}))
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

        Cmd::QuickOpen { text } => cx.update_window(window, |_, window, cx| {
            shell.update(cx, |shell, cx| {
                shell.automation_quick_open(&text, window, cx);
                Ok(json!({"submitted": text}))
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
            let target = Duration::from_millis(ms);
            let started = Instant::now();
            loop {
                let remaining = target.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Ok(json!({"waited_ms": ms}));
                }
                if quit_now() {
                    bail!(
                        "wait interrupted by stdin EOF after {}ms",
                        started.elapsed().as_millis()
                    );
                }
                cx.background_executor()
                    .timer(remaining.min(Duration::from_millis(QUIT_POLL_MS)))
                    .await;
            }
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
                if quit_now() {
                    bail!(
                        "wait_ready interrupted by stdin EOF after {}ms",
                        started.elapsed().as_millis()
                    );
                }
                if started.elapsed() > timeout {
                    bail!("not ready after {}ms", timeout.as_millis());
                }
                // Already well under QUIT_POLL_MS — settling itself wants a
                // tight poll, and the quit check rides along for free.
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
        }

        Cmd::Quit => Ok(json!({"quitting": true})), // caller quits after replying
    }
}

/// Parse a dash-separated modifier spec (`"ctrl"`, `"ctrl-shift"`, `"cmd"`,
/// …) into [`Modifiers`] — `Cmd::Click`'s `modifiers` field (S8g: added so a
/// script can drive real ctrl/cmd-click go-to-definition end-to-end, the
/// same real `MouseDownEvent::modifiers.secondary()` path a physical click
/// exercises). Named after each field's own meaning rather than the
/// platform key that happens to produce it on this OS, since a script
/// should read the same regardless of which desk it's driven from:
/// `ctrl`/`control` -> `control`, `alt`/`option` -> `alt`, `shift` -> `shift`,
/// `cmd`/`platform`/`super`/`win` -> `platform`, `fn`/`function` -> `function`.
/// An unknown token is a hard error (bad script, not a silent no-op).
fn parse_modifiers(spec: &str) -> anyhow::Result<Modifiers> {
    let mut modifiers = Modifiers::default();
    for token in spec.split(['-', '+']) {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.control = true,
            "alt" | "option" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            "cmd" | "platform" | "super" | "win" => modifiers.platform = true,
            "fn" | "function" => modifiers.function = true,
            other => bail!("unknown modifier: {other}"),
        }
    }
    Ok(modifiers)
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
    use super::{Cmd, parse_line, parse_modifiers, resolve_action_name};

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

    #[test]
    fn parse_modifiers_single_ctrl() {
        let modifiers = parse_modifiers("ctrl").unwrap();
        assert!(modifiers.control);
        assert!(!modifiers.platform && !modifiers.shift && !modifiers.alt);
    }

    #[test]
    fn parse_modifiers_accepts_cmd_alias_for_platform() {
        assert!(parse_modifiers("cmd").unwrap().platform);
        assert!(parse_modifiers("super").unwrap().platform);
        assert!(parse_modifiers("win").unwrap().platform);
    }

    #[test]
    fn parse_modifiers_combines_dash_separated_tokens() {
        let modifiers = parse_modifiers("ctrl-shift").unwrap();
        assert!(modifiers.control);
        assert!(modifiers.shift);
        assert!(!modifiers.platform);
    }

    #[test]
    fn parse_modifiers_rejects_unknown_token() {
        let err = parse_modifiers("meta").unwrap_err().to_string();
        assert!(err.contains("unknown modifier"), "{err}");
    }

    #[test]
    fn click_command_parses_optional_modifiers_field() {
        let (_, cmd) = parse_line(r#"{"cmd":"click","x":1.0,"y":2.0,"modifiers":"ctrl"}"#);
        let Ok(Cmd::Click { modifiers, .. }) = cmd else {
            panic!("expected Cmd::Click");
        };
        assert_eq!(modifiers.as_deref(), Some("ctrl"));
    }

    #[test]
    fn mouse_move_command_parses() {
        let (_, cmd) = parse_line(r#"{"cmd":"mouse_move","x":10.0,"y":20.0}"#);
        assert!(matches!(cmd, Ok(Cmd::MouseMove { x, y }) if x == 10.0 && y == 20.0));
    }
}
