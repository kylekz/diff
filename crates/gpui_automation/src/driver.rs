//! The command loop: reads requests from the transport, runs each to
//! completion on the foreground executor (so scripts need no client-side
//! pacing) and writes exactly one reply line per request, to the client that
//! sent it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::StreamExt as _;
use futures::channel::mpsc;
use futures::future::Either;
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, ExternalPaths, FileDropEvent,
    KeyUpEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, PlatformInput, Point, ScrollDelta, ScrollEvent, Task, TouchPhase, Window, point, px,
};
use serde_json::{Map, Value, json};

use crate::AutomationHost;
use crate::capture::CaptureSource;
use crate::commands::{
    ActionArgs, Builtin, ButtonArgs, ClickArgs, DEFAULT_TIMEOUT_MS, DragArgs, DropFilesArgs,
    FullscreenArgs, Parsed, Position, ResizeArgs, ScrollArgs, WaitForArgs, WindowArgs, position,
};
use crate::frames;
use crate::keys;
use crate::matcher::compact;
use crate::protocol::{self, Request, WindowSelector};
use crate::targets::{self, TargetSet};
use crate::transport::{ClientId, Clients, Inbound, Output, STDIO_CLIENT};
use crate::windows::{self, Listed, SharedRegistry};

/// Upper bound for every wait-like command, so a script bug can't park a
/// window on the desktop indefinitely.
const MAX_WAIT: Duration = Duration::from_secs(120);

/// How often long waits re-check for a hung-up driver and poll their
/// condition.
const POLL: Duration = Duration::from_millis(50);

/// Longest a click on a target waits for it to stop moving before pressing where it was last
/// drawn (a target that never settles, such as text under auto-scroll, is still clicked).
const STEADY_TIMEOUT: Duration = Duration::from_secs(1);

/// How far a target's centre may move between two frames and still count as steady: an eased
/// entrance creeps the last sub-pixel for a few frames.
const STEADY_EPSILON: f32 = 0.5;

/// Synthetic pointer state per window: what a real mouse would be doing.
#[derive(Debug, Clone, Default)]
struct Pointer {
    position: Point<Pixels>,
    pressed: Option<MouseButton>,
    /// Files currently dragged over the window by `drop_files`.
    file_drag: Option<Vec<PathBuf>>,
}

pub(crate) struct Driver<H: AutomationHost> {
    host: Entity<H>,
    registry: SharedRegistry,
    clients: Arc<Clients>,
    output: Output,
    /// The client whose request is being handled.
    client: ClientId,
    pointers: HashMap<AnyWindowHandle, Pointer>,
}

impl<H: AutomationHost> Driver<H> {
    pub fn new(
        host: Entity<H>,
        registry: SharedRegistry,
        clients: Arc<Clients>,
        output: Output,
    ) -> Self {
        Self {
            host,
            registry,
            clients,
            output,
            client: STDIO_CLIENT,
            pointers: HashMap::new(),
        }
    }

    pub async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Inbound>, cx: &mut AsyncApp) {
        // Nothing is answered before the first frame, so a driver never
        // clicks into an empty window.
        self.first_frame(cx).await;
        while let Some(message) = rx.next().await {
            self.clients.dequeued();
            let (client, line) = match message {
                Inbound::Connected(client) => {
                    self.announce_ready(client, cx);
                    continue;
                }
                Inbound::Line(client, line) => (client, line),
            };
            self.client = client;
            let (id, quit, result) = match protocol::parse_request(&line) {
                Ok(request) => {
                    let id = request.id.clone();
                    let quit = request.cmd == "quit";
                    let result = self.handle(request, cx).await;
                    (id, quit && result.is_ok(), result)
                }
                Err(bad) => (bad.id, false, Err(anyhow!(bad.message))),
            };
            let reply = match &result {
                Ok(value) => protocol::ok_reply(id, value.clone()),
                Err(error) => protocol::error_reply(id, error),
            };
            self.output.send(client, &reply);
            if quit {
                self.output.flush();
                cx.update(quit_app);
                break;
            }
        }
    }

    async fn first_frame(&mut self, cx: &mut AsyncApp) {
        match self.current_window(None, cx) {
            Ok(window) => {
                if let Err(err) = frames::drawn_frame(window.handle, cx, false).await {
                    log::warn!("automation: announcing ready without a first frame: {err:#}");
                }
            }
            Err(err) => log::warn!("automation: {err:#}"),
        }
    }

    /// Greet a newly connected driver with `{"event":"ready", ...}`.
    fn announce_ready(&mut self, client: ClientId, cx: &mut AsyncApp) {
        let windows = self.windows_json(cx);
        let mut fields = Map::new();
        fields.insert("windows".into(), windows);
        fields.insert("pid".into(), json!(std::process::id()));
        fields.insert("background".into(), json!(crate::is_background()));
        self.output.send(client, &protocol::event("ready", fields));
    }

    async fn handle(&mut self, request: Request, cx: &mut AsyncApp) -> Result<Value> {
        let Request {
            cmd, window, args, ..
        } = request;
        let window = window.as_ref();
        let builtin = match Builtin::parse(&cmd, args) {
            Parsed::Builtin(builtin) => builtin?,
            Parsed::Custom(args) => return self.custom(&cmd, args, window, cx).await,
        };
        match builtin {
            Builtin::State => {
                let window = self.current_window(window, cx)?;
                self.state(&window, cx)
            }
            Builtin::Windows => Ok(self.windows_json(cx)),
            Builtin::Actions => Ok(cx.update(|cx| json!(cx.all_action_names()))),
            Builtin::Action(args) => self.action(args, window, cx).await,
            Builtin::Key(args) => {
                let keystroke = keys::parse_keystroke(&args.keystroke)?;
                let keystroke = cx.update(|cx| keys::as_typed(keystroke, cx));
                let window = self.current_window(window, cx)?;
                let handled = cx.update_window(window.handle, |_, window, cx| {
                    window.dispatch_keystroke(keystroke.clone(), cx)
                })?;
                self.after_input(&window, cx).await?;
                self.release(keystroke, &window, cx).await?;
                Ok(json!({"handled": handled}))
            }
            Builtin::Keys(args) => {
                let keystrokes = keys::parse_keystroke_sequence(&args.keys.joined())?;
                let keystrokes: Vec<Keystroke> = cx.update(|cx| {
                    keystrokes
                        .into_iter()
                        .map(|keystroke| keys::as_typed(keystroke, cx))
                        .collect()
                });
                let window = self.current_window(window, cx)?;
                let mut handled = Vec::with_capacity(keystrokes.len());
                for keystroke in keystrokes {
                    handled.push(cx.update_window(window.handle, |_, window, cx| {
                        window.dispatch_keystroke(keystroke.clone(), cx)
                    })?);
                    // Key dispatch routes through the rendered frame's
                    // dispatch tree, so let a key that opened a palette render
                    // it before the next key is meant for it.
                    self.after_input(&window, cx).await?;
                    self.release(keystroke, &window, cx).await?;
                }
                Ok(json!({"handled": handled}))
            }
            Builtin::Type(args) => {
                let window = self.current_window(window, cx)?;
                let keystrokes = keys::text_keystrokes(&args.text);
                let count = keystrokes.len();
                cx.update_window(window.handle, |_, window, cx| {
                    for keystroke in keystrokes {
                        window.dispatch_keystroke(keystroke, cx);
                    }
                })?;
                self.after_input(&window, cx).await?;
                Ok(json!({"typed": count}))
            }
            Builtin::Click(args) => self.click(args, window, cx).await,
            Builtin::MouseMove(args) => {
                let window = self.current_window(window, cx)?;
                let modifiers = parse_modifiers(args.modifiers.as_deref())?;
                let at = self
                    .resolve(position(args.x, args.y, args.target)?, &window, cx)
                    .await?;
                let pressed = self.pointer(&window).pressed;
                self.move_to(&window, at, pressed, modifiers, cx).await?;
                Ok(json!({"moved": point_json(at)}))
            }
            Builtin::MouseDown(args) => self.mouse_button(args, true, window, cx).await,
            Builtin::MouseUp(args) => self.mouse_button(args, false, window, cx).await,
            Builtin::Drag(args) => self.drag(args, window, cx).await,
            Builtin::Scroll(args) => self.scroll(args, window, cx).await,
            Builtin::DropFiles(args) => self.drop_files(args, window, cx).await,
            Builtin::Resize(args) => self.resize(args, window, cx).await,
            Builtin::Fullscreen(args) => self.fullscreen(args, window, cx).await,
            Builtin::Window(args) => self.select_window(args, window, cx),
            Builtin::Screenshot(args) => {
                let window = self.current_window(window, cx)?;
                self.screenshot(&window, &args.path, args.source, cx).await
            }
            Builtin::Wait(args) => {
                let duration = Duration::from_millis(args.ms);
                check_wait_limit("wait ms", duration)?;
                let started = Instant::now();
                loop {
                    let remaining = duration.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return Ok(json!({"waited_ms": args.ms}));
                    }
                    self.bail_if_abandoned("wait", started)?;
                    frames::sleep(cx, remaining.min(POLL)).await;
                }
            }
            Builtin::WaitReady(args) => {
                let timeout = Duration::from_millis(args.timeout_ms.unwrap_or(15_000));
                check_wait_limit("wait_ready timeout_ms", timeout)?;
                let started = Instant::now();
                loop {
                    if cx.update(|cx| self.host.read(cx).settled(cx)) {
                        return Ok(json!({"ready": true, "elapsed_ms": elapsed_ms(started)}));
                    }
                    self.bail_if_abandoned("wait_ready", started)?;
                    if started.elapsed() > timeout {
                        bail!("not ready after {}ms", timeout.as_millis());
                    }
                    frames::sleep(cx, POLL).await;
                }
            }
            Builtin::WaitFor(args) => self.wait_for(args, window, cx).await,
            Builtin::Assert(expectation) => {
                let window = self.current_window(window, cx)?;
                let state = self.state(&window, cx)?;
                expectation.evaluate(&state)?;
                Ok(json!({"actual": state.pointer(&expectation.pointer)}))
            }
            Builtin::Settle(args) => {
                let window = self.current_window(window, cx)?;
                let timeout = Duration::from_millis(args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
                check_wait_limit("settle timeout_ms", timeout)?;
                let started = Instant::now();
                let mut frames_waited = 0u32;
                loop {
                    frames::drawn_frame(window.handle, cx, false).await?;
                    frames_waited += 1;
                    if cx.update(|cx| self.host.read(cx).settled(cx)) {
                        // One more so whatever settling changed is on screen.
                        frames::drawn_frame(window.handle, cx, false).await?;
                        return Ok(json!({
                            "frames": frames_waited + 1,
                            "elapsed_ms": elapsed_ms(started),
                        }));
                    }
                    self.bail_if_abandoned("settle", started)?;
                    if started.elapsed() > timeout {
                        bail!("not settled after {}ms", timeout.as_millis());
                    }
                }
            }
            Builtin::WaitFrames(args) => {
                if args.n == 0 || args.n > 600 {
                    bail!("n must be between 1 and 600, got {}", args.n);
                }
                let window = self.current_window(window, cx)?;
                let started = Instant::now();
                frames::frames(window.handle, cx, args.n).await?;
                Ok(json!({"frames": args.n, "elapsed_ms": elapsed_ms(started)}))
            }
            Builtin::Bounds(args) => {
                let window = self.current_window(window, cx)?;
                let set = self.capture_targets(&window, cx).await?;
                let (record, count) = set
                    .get(&args.target)
                    .ok_or_else(|| missing_target(&args.target, &window, &set))?;
                Ok(targets::target_json(record, count))
            }
            Builtin::Query(args) => {
                let window = self.current_window(window, cx)?;
                let set = self.capture_targets(&window, cx).await?;
                Ok(Value::Array(
                    set.with_prefix(&args.prefix)
                        .map(|(record, count)| targets::target_json(record, count))
                        .collect(),
                ))
            }
            Builtin::Quit => Ok(json!({"quitting": true})),
        }
    }

    // ---- windows and state

    fn current_window(
        &self,
        selector: Option<&WindowSelector>,
        cx: &mut AsyncApp,
    ) -> Result<Listed> {
        let registry = self.registry.clone();
        cx.update(|cx| {
            let open = cx.windows();
            let mut registry = registry.0.borrow_mut();
            match selector {
                Some(selector) => registry.find(selector, &open),
                None => registry.current(&open),
            }
        })
    }

    fn windows_json(&self, cx: &mut AsyncApp) -> Value {
        let registry = self.registry.clone();
        cx.update(|cx| {
            let open = cx.windows();
            let mut registry = registry.0.borrow_mut();
            let current = registry.current(&open).ok().map(|entry| entry.handle);
            Value::Array(
                registry
                    .list(&open)
                    .iter()
                    .map(|entry| {
                        json!({
                            "index": entry.index,
                            "name": entry.name,
                            "id": entry.handle.window_id().as_u64(),
                            "selected": Some(entry.handle) == current,
                        })
                    })
                    .collect(),
            )
        })
    }

    fn state(&self, window: &Listed, cx: &mut AsyncApp) -> Result<Value> {
        let windows = self.windows_json(cx);
        let host = self.host.clone();
        let window_entry = window.clone();
        cx.update_window(window.handle, move |_, window, cx| {
            let info = window_json(&window_entry, window, cx);
            match host.read(cx).state(window, cx) {
                Value::Object(mut map) => {
                    map.insert("window".into(), info);
                    map.insert("windows".into(), windows);
                    Value::Object(map)
                }
                other => json!({"app": other, "window": info, "windows": windows}),
            }
        })
    }

    fn select_window(
        &mut self,
        args: WindowArgs,
        selector: Option<&WindowSelector>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let selector = match (args.index, args.name) {
            (Some(index), None) => WindowSelector::Index(index),
            (None, Some(name)) => WindowSelector::Name(name),
            (None, None) => selector
                .cloned()
                .ok_or_else(|| anyhow!("window needs an index or a name"))?,
            (Some(_), Some(_)) => bail!("give either index or name, not both"),
        };
        let entry = self.current_window(Some(&selector), cx)?;
        self.registry.0.borrow_mut().select(entry.handle);
        log::info!("automation: selected window {}", windows::label(&entry));
        self.state(&entry, cx).map(|state| state["window"].clone())
    }

    async fn wait_for(
        &mut self,
        args: WaitForArgs,
        selector: Option<&WindowSelector>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let timeout = Duration::from_millis(args.timeout_ms);
        check_wait_limit("wait_for timeout_ms", timeout)?;
        let started = Instant::now();
        loop {
            // Re-resolved every poll: the window may open while we wait.
            let last = match self.current_window(selector, cx) {
                Ok(window) => {
                    let state = self.state(&window, cx)?;
                    match args.expectation.evaluate(&state) {
                        Ok(()) => {
                            return Ok(json!({
                                "elapsed_ms": elapsed_ms(started),
                                "actual": state.pointer(&args.expectation.pointer),
                            }));
                        }
                        Err(mismatch) => mismatch.to_string(),
                    }
                }
                Err(err) => format!("{err:#}"),
            };
            self.bail_if_abandoned("wait_for", started)?;
            if started.elapsed() > timeout {
                bail!("timed out after {}ms: {last}", timeout.as_millis());
            }
            frames::sleep(cx, Duration::from_millis(args.interval_ms)).await;
        }
    }

    fn bail_if_abandoned(&self, what: &str, started: Instant) -> Result<()> {
        if self.clients.abandoned(self.client) {
            bail!(
                "{what} interrupted after {}ms: the driver hung up",
                started.elapsed().as_millis()
            );
        }
        Ok(())
    }

    // ---- actions and custom commands

    async fn action(
        &mut self,
        args: ActionArgs,
        selector: Option<&WindowSelector>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let window = self.current_window(selector, cx)?;
        let ActionArgs { name, data } = args;
        let full = cx.update_window(window.handle, |_, window, cx| -> Result<String> {
            let full = resolve_action_name(cx.all_action_names(), &name)?;
            let action = cx
                .build_action(&full, data)
                .map_err(|err| anyhow!("building action {full}: {err}"))?;
            window.dispatch_action(action, cx);
            Ok(full)
        })??;
        self.after_input(&window, cx).await?;
        Ok(json!({"action": full}))
    }

    async fn custom(
        &mut self,
        cmd: &str,
        args: Map<String, Value>,
        selector: Option<&WindowSelector>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let window = self.current_window(selector, cx)?;
        let host = self.host.clone();
        let args = Value::Object(args);
        let outcome = cx.update_window(window.handle, |_, window, cx| {
            host.update(cx, |host, cx| {
                match host.custom(cmd, args.clone(), window, cx) {
                    Some(result) => Some(Ok(result)),
                    None => host.custom_task(cmd, args, window, cx).map(Err),
                }
            })
        })?;
        let result = match outcome {
            None => bail!(
                "unknown command {cmd:?}; built-ins are {}",
                Builtin::NAMES.join(", ")
            ),
            Some(Ok(result)) => result,
            Some(Err(task)) => self.await_task(cmd, task, cx).await?,
        };
        let value = result.with_context(|| format!("{cmd} failed"))?;
        self.after_input(&window, cx).await?;
        Ok(value)
    }

    /// Wait for a host's command task, polling for a hung-up driver and the
    /// wait cap. The outer error is the wait's own; the inner one the task's.
    async fn await_task(
        &self,
        cmd: &str,
        mut task: Task<Result<Value>>,
        cx: &mut AsyncApp,
    ) -> Result<Result<Value>> {
        let started = Instant::now();
        loop {
            let tick = cx.background_executor().timer(POLL);
            match futures::future::select(&mut task, tick).await {
                Either::Left((result, _)) => return Ok(result),
                Either::Right(((), _)) => {
                    self.bail_if_abandoned(cmd, started)?;
                    if started.elapsed() > MAX_WAIT {
                        bail!("{cmd} did not finish within {}ms", MAX_WAIT.as_millis());
                    }
                }
            }
        }
    }

    /// Wait for input's effects to render. Input may legitimately close the
    /// window it was sent to (a Close button), which is not an error.
    /// Lets a pressed key come back up once a frame has shown what the press did, in an update
    /// of its own as the OS delivers it: gpui turns Enter or Space on a focused element into a
    /// click only on the release, and only if the key went down there. A press that closed the
    /// window has nothing to release.
    async fn release(
        &self,
        keystroke: Keystroke,
        window: &Listed,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let released = cx.update_window(window.handle, |_, window, cx| {
            window.dispatch_event(PlatformInput::KeyUp(KeyUpEvent { keystroke }), cx);
        });
        match released {
            Ok(()) => self.after_input(window, cx).await,
            Err(_) => Ok(()),
        }
    }

    async fn after_input(&self, window: &Listed, cx: &mut AsyncApp) -> Result<()> {
        match frames::drawn_frame(window.handle, cx, false).await {
            Ok(()) => Ok(()),
            Err(err) => {
                if cx.update(|cx| cx.windows().contains(&window.handle)) {
                    Err(err)
                } else {
                    Ok(())
                }
            }
        }
    }

    // ---- pointer input

    fn pointer(&mut self, window: &Listed) -> &mut Pointer {
        self.pointers.entry(window.handle).or_default()
    }

    /// Pressing a button while files hover over the window has no real
    /// counterpart (the OS owns the pointer during a file drag).
    fn ensure_no_file_drag(&mut self, window: &Listed) -> Result<()> {
        if self.pointer(window).file_drag.is_some() {
            bail!("files are still hovering over the window; finish with drop_files first");
        }
        Ok(())
    }

    /// Resolve a position to window coordinates. Coordinates must lie inside
    /// the viewport: a drag that leaves the window could hand gpui's drag to
    /// the OS as a real, blocking drag-and-drop.
    async fn resolve(
        &mut self,
        position: Option<Position>,
        window: &Listed,
        cx: &mut AsyncApp,
    ) -> Result<Point<Pixels>> {
        match position {
            None => Ok(self.pointer(window).position),
            Some(Position::Point(x, y)) => {
                let viewport =
                    cx.update_window(window.handle, |_, window, _| window.viewport_size())?;
                let (w, h) = (f32::from(viewport.width), f32::from(viewport.height));
                if !(0.0..w).contains(&x) || !(0.0..h).contains(&y) {
                    bail!("({x}, {y}) is outside the {w}x{h} window");
                }
                Ok(point(px(x), px(y)))
            }
            Some(Position::TargetOffset(name, dx, dy)) => {
                let centre =
                    Box::pin(self.resolve(Some(Position::Target(name)), window, cx)).await?;
                let (x, y) = (f32::from(centre.x) + dx, f32::from(centre.y) + dy);
                Box::pin(self.resolve(Some(Position::Point(x, y)), window, cx)).await
            }
            Some(Position::Target(name)) => {
                let set = self.capture_targets(window, cx).await?;
                let (record, _) = set
                    .get(&name)
                    .ok_or_else(|| missing_target(&name, window, &set))?;
                let visible = record.visible.ok_or_else(|| {
                    anyhow!(
                        "target {name:?} is rendered but clipped out of view at {}",
                        compact(&targets::rect_json(record.bounds))
                    )
                })?;
                Ok(point(
                    visible.origin.x + visible.size.width / 2.,
                    visible.origin.y + visible.size.height / 2.,
                ))
            }
        }
    }

    /// Like [`Self::resolve`], but a target's centre only once two frames in a row drew it in
    /// the same place (within [`STEADY_TIMEOUT`]): a press resolved while its target slides in
    /// (gpui-component's dialogs enter over a quarter second, moving their buttons tens of
    /// pixels) would otherwise land where the target was, not where it is.
    async fn resolve_steady(
        &mut self,
        position: Position,
        window: &Listed,
        cx: &mut AsyncApp,
    ) -> Result<Point<Pixels>> {
        let targeted = matches!(position, Position::Target(_) | Position::TargetOffset(..));
        let mut at = self.resolve(Some(position.clone()), window, cx).await?;
        if !targeted {
            return Ok(at);
        }
        let started = Instant::now();
        loop {
            let next = self.resolve(Some(position.clone()), window, cx).await?;
            if is_steady(at, next) || started.elapsed() >= STEADY_TIMEOUT {
                return Ok(next);
            }
            at = next;
        }
    }

    async fn capture_targets(&self, window: &Listed, cx: &mut AsyncApp) -> Result<TargetSet> {
        let id = window.handle.window_id().as_u64();
        let records = frames::between_frames(
            window.handle,
            cx,
            true,
            move |_, _| targets::arm(id),
            move |_, _| targets::take(id),
        )
        .await;
        // Disarm even if the capture timed out, so prepaint stops recording.
        targets::take(id);
        let records = records?.ok_or_else(|| anyhow!("target capture was interrupted"))?;
        Ok(TargetSet::new(records))
    }

    fn dispatch(
        &mut self,
        window: &Listed,
        events: Vec<PlatformInput>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let pointer = self.pointer(window);
        for event in &events {
            match event {
                PlatformInput::MouseMove(e) => {
                    pointer.position = e.position;
                    pointer.pressed = e.pressed_button;
                }
                PlatformInput::MouseDown(e) => {
                    pointer.position = e.position;
                    pointer.pressed = Some(e.button);
                }
                PlatformInput::MouseUp(e) => {
                    pointer.position = e.position;
                    pointer.pressed = None;
                }
                PlatformInput::Scroll(e) => pointer.position = e.position,
                PlatformInput::FileDrop(
                    FileDropEvent::Entered { position, .. }
                    | FileDropEvent::Pending { position }
                    | FileDropEvent::Submit { position },
                ) => pointer.position = *position,
                _ => {}
            }
        }
        cx.update_window(window.handle, |_, window, cx| {
            let _synthetic = crate::guard::SyntheticInput::begin();
            for event in events {
                window.dispatch_event(event, cx);
            }
        })
    }

    /// Move the pointer (a hover, or a drag step when a button is held) and
    /// let the result render.
    async fn move_to(
        &mut self,
        window: &Listed,
        position: Point<Pixels>,
        pressed_button: Option<MouseButton>,
        modifiers: Modifiers,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        self.dispatch(
            window,
            vec![PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button,
                modifiers,
            })],
            cx,
        )?;
        frames::drawn_frame(window.handle, cx, false).await
    }

    async fn click(
        &mut self,
        args: ClickArgs,
        selector: Option<&WindowSelector>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let window = self.current_window(selector, cx)?;
        let button = parse_button(args.button.as_deref())?;
        let mut modifiers = parse_modifiers(args.modifiers.as_deref())?;
        modifiers.shift |= args.shift;
        let count = args.count.unwrap_or(1);
        if !(1..=3).contains(&count) {
            bail!("count must be 1, 2 or 3, got {count}");
        }
        let Some(position_arg) = position(args.x, args.y, args.target)? else {
            bail!("click needs x and y, or a target");
        };
        let at = self.resolve_steady(position_arg, &window, cx).await?;
        self.ensure_no_file_drag(&window)?;
        if self.pointer(&window).pressed.is_some() {
            bail!("a mouse button is still held from an unreleased drag; send mouse_up first");
        }
        // Hover first and let it render, as a real pointer would, so
        // hover-revealed controls exist before the press lands.
        self.move_to(&window, at, None, modifiers, cx).await?;
        let mut events = Vec::with_capacity(count * 2);
        for click_count in 1..=count {
            events.push(PlatformInput::MouseDown(MouseDownEvent {
                button,
                position: at,
                modifiers,
                click_count,
                first_mouse: false,
            }));
            events.push(PlatformInput::MouseUp(MouseUpEvent {
                button,
                position: at,
                modifiers,
                click_count,
            }));
        }
        self.dispatch(&window, events, cx)?;
        self.after_input(&window, cx).await?;
        Ok(json!({"clicked": point_json(at), "count": count}))
    }

    async fn mouse_button(
        &mut self,
        args: ButtonArgs,
        down: bool,
        selector: Option<&WindowSelector>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let window = self.current_window(selector, cx)?;
        let button = parse_button(args.button.as_deref())?;
        let modifiers = parse_modifiers(args.modifiers.as_deref())?;
        let at = self
            .resolve(position(args.x, args.y, args.target)?, &window, cx)
            .await?;
        if down {
            self.ensure_no_file_drag(&window)?;
        }
        let pointer = self.pointer(&window).clone();
        if down && pointer.pressed.is_some() {
            bail!("a mouse button is already down");
        }
        if !down && pointer.pressed.is_none() {
            bail!("no mouse button is down");
        }
        if pointer.position != at {
            self.move_to(&window, at, pointer.pressed, modifiers, cx)
                .await?;
        }
        let event = if down {
            PlatformInput::MouseDown(MouseDownEvent {
                button,
                position: at,
                modifiers,
                click_count: 1,
                first_mouse: false,
            })
        } else {
            PlatformInput::MouseUp(MouseUpEvent {
                button,
                position: at,
                modifiers,
                click_count: 1,
            })
        };
        self.dispatch(&window, vec![event], cx)?;
        self.after_input(&window, cx).await?;
        Ok(json!({"at": point_json(at), "down": down}))
    }

    async fn drag(
        &mut self,
        args: DragArgs,
        selector: Option<&WindowSelector>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let window = self.current_window(selector, cx)?;
        let button = parse_button(args.button.as_deref())?;
        let modifiers = parse_modifiers(args.modifiers.as_deref())?;
        let steps = args.steps.unwrap_or(10);
        if !(1..=1000).contains(&steps) {
            bail!("steps must be between 1 and 1000, got {steps}");
        }
        let step_delay = Duration::from_millis(args.step_ms.unwrap_or(0));
        let hold = Duration::from_millis(args.hold_ms.unwrap_or(0));
        check_wait_limit("drag step_ms", step_delay * steps)?;
        check_wait_limit("drag hold_ms", hold)?;
        let release = args.release.unwrap_or(true);

        let to = self.resolve(Some(args.to.into()), &window, cx).await?;
        let from = match args.from {
            Some(from) => {
                self.ensure_no_file_drag(&window)?;
                if self.pointer(&window).pressed.is_some() {
                    bail!(
                        "a drag is already in progress: omit from to continue it, or send mouse_up"
                    );
                }
                let from = self.resolve(Some(from.into()), &window, cx).await?;
                self.move_to(&window, from, None, modifiers, cx).await?;
                self.dispatch(
                    &window,
                    vec![PlatformInput::MouseDown(MouseDownEvent {
                        button,
                        position: from,
                        modifiers,
                        click_count: 1,
                        first_mouse: false,
                    })],
                    cx,
                )?;
                frames::drawn_frame(window.handle, cx, false).await?;
                from
            }
            None => {
                let pointer = self.pointer(&window);
                if pointer.pressed != Some(button) {
                    bail!(
                        "drag without from continues a drag left held by \"release\": false, but none is"
                    );
                }
                pointer.position
            }
        };

        // Every step waits for a rendered frame so drag-over styling, the drag
        // preview and on_drag_move handlers see each position like they would
        // under a real mouse; the first step crosses gpui's 2px threshold.
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            let at = point(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
            self.move_to(&window, at, Some(button), modifiers, cx)
                .await?;
            frames::sleep(cx, step_delay).await;
        }
        frames::sleep(cx, hold).await;
        if release {
            self.dispatch(
                &window,
                vec![PlatformInput::MouseUp(MouseUpEvent {
                    button,
                    position: to,
                    modifiers,
                    click_count: 1,
                })],
                cx,
            )?;
            self.after_input(&window, cx).await?;
        }
        Ok(json!({
            "from": point_json(from),
            "to": point_json(to),
            "steps": steps,
            "released": release,
        }))
    }

    async fn scroll(
        &mut self,
        args: ScrollArgs,
        selector: Option<&WindowSelector>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let window = self.current_window(selector, cx)?;
        let modifiers = parse_modifiers(args.modifiers.as_deref())?;
        let delta = match args.unit.as_deref() {
            None | Some("px") => ScrollDelta::Pixels(point(px(args.dx), px(args.dy))),
            Some("lines") => ScrollDelta::Lines(point(args.dx, args.dy)),
            Some(other) => bail!("unit must be \"px\" or \"lines\", got {other:?}"),
        };
        let at = self
            .resolve(position(args.x, args.y, args.target)?, &window, cx)
            .await?;
        let pressed = self.pointer(&window).pressed;
        if self.pointer(&window).position != at {
            self.move_to(&window, at, pressed, modifiers, cx).await?;
        }
        self.dispatch(
            &window,
            vec![PlatformInput::Scroll(ScrollEvent {
                position: at,
                delta,
                modifiers,
                touch_phase: TouchPhase::Moved,
                source: gpui::ScrollSource::Pointer,
            })],
            cx,
        )?;
        self.after_input(&window, cx).await?;
        Ok(json!({"at": point_json(at), "dx": args.dx, "dy": args.dy}))
    }

    async fn drop_files(
        &mut self,
        args: DropFilesArgs,
        selector: Option<&WindowSelector>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let window = self.current_window(selector, cx)?;
        let hold = Duration::from_millis(args.hold_ms.unwrap_or(0));
        check_wait_limit("drop_files hold_ms", hold)?;
        let submit = args.submit.unwrap_or(true);
        if args.cancel && args.submit == Some(true) {
            bail!("give either cancel or submit, not both");
        }
        let at = self
            .resolve(position(args.x, args.y, args.target)?, &window, cx)
            .await?;

        let paths = match (args.paths, self.pointer(&window).file_drag.clone()) {
            (Some(_), Some(_)) => {
                bail!("files are already being dragged: omit paths to continue that drag")
            }
            (None, None) => bail!("drop_files needs paths (no file drag is in progress)"),
            (Some(paths), None) => {
                let paths = absolute_existing(paths)?;
                self.dispatch(
                    &window,
                    vec![PlatformInput::FileDrop(FileDropEvent::Entered {
                        position: at,
                        paths: ExternalPaths(paths.iter().cloned().collect()),
                    })],
                    cx,
                )?;
                frames::drawn_frame(window.handle, cx, false).await?;
                self.pointer(&window).file_drag = Some(paths.clone());
                paths
            }
            (None, Some(paths)) => paths,
        };
        self.dispatch(
            &window,
            vec![PlatformInput::FileDrop(FileDropEvent::Pending {
                position: at,
            })],
            cx,
        )?;
        frames::drawn_frame(window.handle, cx, false).await?;

        // OLE keeps calling DragOver while files hover, even when the mouse
        // is still; mirror that so hover timers behave as they would live.
        let started = Instant::now();
        while started.elapsed() < hold {
            frames::sleep(cx, POLL.min(hold.saturating_sub(started.elapsed()))).await;
            self.dispatch(
                &window,
                vec![PlatformInput::FileDrop(FileDropEvent::Pending {
                    position: at,
                })],
                cx,
            )?;
        }

        let outcome = if args.cancel {
            self.dispatch(
                &window,
                vec![PlatformInput::FileDrop(FileDropEvent::Exited)],
                cx,
            )?;
            self.pointer(&window).file_drag = None;
            "cancelled"
        } else if submit {
            self.dispatch(
                &window,
                vec![PlatformInput::FileDrop(FileDropEvent::Submit {
                    position: at,
                })],
                cx,
            )?;
            self.pointer(&window).file_drag = None;
            "dropped"
        } else {
            "hovering"
        };
        self.after_input(&window, cx).await?;
        Ok(json!({
            "paths": paths,
            "at": point_json(at),
            "outcome": outcome,
        }))
    }

    // ---- window geometry

    async fn resize(
        &mut self,
        args: ResizeArgs,
        selector: Option<&WindowSelector>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        if !(args.w >= 1.0 && args.h >= 1.0) {
            bail!("w and h must be positive, got {}x{}", args.w, args.h);
        }
        let window = self.current_window(selector, cx)?;
        let timeout = Duration::from_millis(args.timeout_ms.unwrap_or(5_000));
        check_wait_limit("resize timeout_ms", timeout)?;
        let (fullscreen, maximized) = cx.update_window(window.handle, |_, window, _| {
            (window.is_fullscreen(), window.is_maximized())
        })?;
        if fullscreen {
            bail!("the window is fullscreen; leave it (fullscreen with on: false) before resizing");
        }
        if maximized {
            bail!("the window is maximised; resize needs a normal window");
        }
        #[cfg(windows)]
        {
            let (hwnd, scale) = cx.update_window(window.handle, |_, window, _| {
                (crate::capture::hwnd(window), window.scale_factor())
            })?;
            // Rounded like gpui's own `to_device_pixels`.
            let physical = |logical: f32| (logical * scale).round() as i32;
            // Outside `update_window`: SetWindowPos sends WM_SIZE straight to
            // gpui's window procedure, which needs the app.
            crate::resize_windows::set_client_size(
                hwnd?,
                physical(args.w),
                physical(args.h),
                scale,
            )?;
        }
        #[cfg(not(windows))]
        cx.update_window(window.handle, |_, window, _| {
            window.resize(gpui::size(px(args.w), px(args.h)))
        })?;
        let started = Instant::now();
        loop {
            frames::drawn_frame(window.handle, cx, false).await?;
            let viewport =
                cx.update_window(window.handle, |_, window, _| window.viewport_size())?;
            let (w, h) = (f32::from(viewport.width), f32::from(viewport.height));
            // Physical pixels round, so allow a logical pixel of slack.
            if (w - args.w).abs() <= 1.0 && (h - args.h).abs() <= 1.0 {
                return Ok(json!({"w": w, "h": h}));
            }
            if started.elapsed() > timeout {
                // Windows sizes the window exactly; other platforms may clamp
                // it to the display.
                let hint = if cfg!(windows) {
                    ""
                } else {
                    " (larger than the display?)"
                };
                bail!(
                    "window is {w}x{h} after {}ms, asked for {}x{}{hint}",
                    timeout.as_millis(),
                    args.w,
                    args.h
                );
            }
        }
    }

    async fn fullscreen(
        &mut self,
        args: FullscreenArgs,
        selector: Option<&WindowSelector>,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        let window = self.current_window(selector, cx)?;
        let timeout = Duration::from_millis(args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        check_wait_limit("fullscreen timeout_ms", timeout)?;
        let toggled = cx.update_window(window.handle, |_, window, _| {
            let toggle = window.is_fullscreen() != args.on;
            if toggle {
                window.toggle_fullscreen();
            }
            toggle
        })?;
        let started = Instant::now();
        loop {
            // The platform applies the change asynchronously; wait for it to
            // report the new state and for a frame at the new size.
            frames::drawn_frame(window.handle, cx, false).await?;
            if cx.update_window(window.handle, |_, window, _| window.is_fullscreen())? == args.on {
                break;
            }
            if started.elapsed() > timeout {
                bail!(
                    "window did not {} fullscreen within {}ms",
                    if args.on { "enter" } else { "leave" },
                    timeout.as_millis()
                );
            }
        }
        if toggled {
            // The first frame after the switch can still be at the old size.
            frames::drawn_frame(window.handle, cx, true).await?;
        }
        self.state(&window, cx).map(|state| state["window"].clone())
    }

    async fn screenshot(
        &mut self,
        window: &Listed,
        path: &Path,
        source: CaptureSource,
        cx: &mut AsyncApp,
    ) -> Result<Value> {
        // macOS keeps a background window in front (`background`), so the screen shows it.
        if source == CaptureSource::Screen
            && crate::background::is_background()
            && !cfg!(target_os = "macos")
        {
            bail!(
                "a screen capture copies what the desktop shows, and a background window sits \
                 behind every other window; run this scenario in the foreground \
                 (\"foreground\": true in its header) or use the default window source"
            );
        }
        let path =
            std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))?;
        // A refreshed frame, drawn and presented, then one more frame so the
        // compositor has shown it: no fixed sleeps.
        frames::drawn_frame(window.handle, cx, true).await?;
        frames::at_next_frame(window.handle, cx, |_, _| ()).await?;
        let (target, scale, viewport) = cx.update_window(window.handle, |_, window, _| {
            (
                crate::capture::capture_target(window),
                window.scale_factor(),
                window.viewport_size(),
            )
        })?;
        let target = target?;
        let shown = path.display().to_string();
        let captured = cx
            .background_executor()
            .spawn(async move { crate::capture::capture_png(target, &path, source) })
            .await?;
        Ok(json!({
            "path": shown,
            "source": source,
            "w": captured.width,
            "h": captured.height,
            "scale": scale,
            "logical": {"w": f32::from(viewport.width), "h": f32::from(viewport.height)},
        }))
    }
}

/// Closes every window, then quits. Quitting with windows open leaves them for
/// `App::shutdown` to drop after the event loop has ended, and on Windows a
/// dropped window's `DestroyWindow` runs in a task that then never runs. The
/// platform window keeps itself alive through that task, and with it its input
/// handler, which holds the focused text field's entity past the App's end (a
/// leaked-handle panic in leak-detection builds). Windows closed here are
/// destroyed before the quit message is handled. An app that quits when its
/// last window closes has already quit by then; quitting again is harmless.
fn quit_app(cx: &mut App) {
    for window in cx.windows() {
        window
            .update(cx, |_, window, _| window.remove_window())
            .ok();
    }
    cx.quit();
}

fn window_json(entry: &Listed, window: &Window, cx: &App) -> Value {
    let viewport = window.viewport_size();
    json!({
        "index": entry.index,
        "name": entry.name,
        "id": entry.handle.window_id().as_u64(),
        "w": f32::from(viewport.width),
        "h": f32::from(viewport.height),
        "scale": window.scale_factor(),
        "fullscreen": window.is_fullscreen(),
        "maximized": window.is_maximized(),
        "active": window.is_window_active(),
        "background": crate::is_background(),
        "bounds": targets::rect_json(window.bounds()),
        "display": window.display(cx).map(|display| json!({
            "id": u64::from(display.id()),
            "bounds": targets::rect_json(display.bounds()),
        })),
    })
}

fn missing_target(name: &str, window: &Listed, set: &TargetSet) -> anyhow::Error {
    let suggestions = set.suggestions(name);
    if suggestions.is_empty() {
        anyhow!(
            "no target {name:?} is rendered in window {} (no targets are rendered there)",
            windows::label(window)
        )
    } else {
        anyhow!(
            "no target {name:?} is rendered in window {}; rendered targets include {}",
            windows::label(window),
            suggestions.join(", ")
        )
    }
}

fn point_json(point: Point<Pixels>) -> Value {
    json!([f32::from(point.x), f32::from(point.y)])
}

/// Whether a target drawn at `before`, then at `after` a frame later, has stopped moving.
fn is_steady(before: Point<Pixels>, after: Point<Pixels>) -> bool {
    f32::from(after.x - before.x).abs() <= STEADY_EPSILON
        && f32::from(after.y - before.y).abs() <= STEADY_EPSILON
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

fn check_wait_limit(what: &str, duration: Duration) -> Result<()> {
    if duration > MAX_WAIT {
        bail!(
            "{what} is capped at {}ms, got {}ms",
            MAX_WAIT.as_millis(),
            duration.as_millis()
        );
    }
    Ok(())
}

fn parse_button(name: Option<&str>) -> Result<MouseButton> {
    Ok(match name {
        None | Some("left") => MouseButton::Left,
        Some("right") => MouseButton::Right,
        Some("middle") => MouseButton::Middle,
        Some(other) => bail!("unknown button {other:?} (left, right or middle)"),
    })
}

fn parse_modifiers(spec: Option<&str>) -> Result<Modifiers> {
    spec.map_or(Ok(Modifiers::default()), keys::parse_modifiers)
}

/// Real file drops only ever carry existing absolute paths; hold scripts to
/// the same so the app sees what it would see live.
fn absolute_existing(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if paths.is_empty() {
        bail!("paths is empty");
    }
    paths
        .into_iter()
        .map(|path| {
            let absolute = std::path::absolute(&path)
                .with_context(|| format!("resolving {}", path.display()))?;
            if !absolute.exists() {
                bail!("{} does not exist", absolute.display());
            }
            Ok(absolute)
        })
        .collect()
}

/// Accept a full action name or a unique short one: `"ToggleSplit"`
/// resolves to `"workspace::ToggleSplit"` when no other namespace has one.
fn resolve_action_name(names: &[&'static str], query: &str) -> Result<String> {
    if names.contains(&query) {
        return Ok(query.to_string());
    }
    let suffix = format!("::{query}");
    let matches: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| name.ends_with(&suffix))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.to_string()),
        [] => bail!("unknown action {query}"),
        many => bail!("ambiguous action {query}: {}", many.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn action_names_resolve_by_unique_suffix() {
        let names: &[&'static str] = &[
            "workspace::ToggleSplit",
            "shell::NewReview",
            "a::Go",
            "b::Go",
        ];
        assert_eq!(
            resolve_action_name(names, "ToggleSplit").unwrap(),
            "workspace::ToggleSplit"
        );
        assert_eq!(
            resolve_action_name(names, "shell::NewReview").unwrap(),
            "shell::NewReview"
        );
        assert!(resolve_action_name(names, "Nope").is_err());
        let err = resolve_action_name(names, "Go").unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
    }

    #[test]
    fn buttons_parse() {
        assert_eq!(parse_button(None).unwrap(), MouseButton::Left);
        assert_eq!(parse_button(Some("right")).unwrap(), MouseButton::Right);
        assert_eq!(parse_button(Some("middle")).unwrap(), MouseButton::Middle);
        assert!(parse_button(Some("back")).is_err());
    }

    #[test]
    fn a_target_is_steady_once_it_stops_moving() {
        let at = |x: f32, y: f32| point(px(x), px(y));
        assert!(is_steady(at(100., 200.), at(100., 200.)));
        assert!(is_steady(at(100., 200.), at(100.4, 199.6)), "an eased tail");
        assert!(!is_steady(at(100., 180.), at(100., 200.)), "sliding in");
        assert!(!is_steady(at(99., 200.), at(100., 200.)));
    }

    #[test]
    fn wait_limit_is_enforced() {
        assert!(check_wait_limit("x", Duration::from_secs(1)).is_ok());
        assert!(check_wait_limit("x", MAX_WAIT + Duration::from_millis(1)).is_err());
    }

    #[test]
    fn dropped_paths_must_exist_and_become_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.png");
        std::fs::write(&file, b"x").unwrap();
        let resolved = absolute_existing(vec![file.clone()]).unwrap();
        assert!(resolved[0].is_absolute());
        let err = absolute_existing(vec![dir.path().join("missing.png")]).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
        assert!(absolute_existing(vec![]).is_err());
    }
}
