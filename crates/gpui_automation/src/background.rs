//! Background windows: automation that never disturbs the person using the
//! machine (tests/e2e/README.md "Background runs").
//!
//! An app started with a flag such as `--automation-background` calls
//! [`set_background`] before it opens its first window and opens every
//! window through [`open_window`]. In background mode each window:
//!
//! - opens without activating (`WindowOptions::focus` off), so it never
//!   takes the keyboard focus or the foreground from whatever the person is
//!   using, and opens windowed even when asked for maximized bounds (see
//!   below);
//! - on Windows, goes straight to the bottom of the z-order without being
//!   activated (`SetWindowPos(HWND_BOTTOM, SWP_NOACTIVATE)`) and becomes a
//!   tool window that can't be activated (`WS_EX_TOOLWINDOW |
//!   WS_EX_NOACTIVATE`), so it stays out of the taskbar and Alt+Tab, and
//!   Windows never hands it the foreground when the person minimises or
//!   closes their own foreground window. A click on it doesn't activate it
//!   either: gpui answers `WM_MOUSEACTIVATE` with `MA_ACTIVATE` itself, so the
//!   window is subclassed to answer `MA_NOACTIVATE`. It also loses its
//!   system menu (`WS_SYSMENU`), without which DWM draws no tool-window
//!   caption glyph behind a translucent title bar. It stays on screen:
//!   DirectComposition keeps rendering an occluded window, and
//!   `PrintWindow(PW_RENDERFULLCONTENT)` still captures it (moving it off
//!   screen or minimising it would stop both);
//! - renders at full rate although it is inactive (gpui otherwise throttles
//!   inactive windows to ~30 fps, which would slow every scripted step).
//!
//! Scripted input never needs the OS focus: the driver dispatches events
//! into the gpui window directly, and gpui tracks keyboard focus per window.
//!
//! The app must also not activate or raise anything itself while
//! [`is_background`] holds. On Windows these gpui calls do:
//!
//! - `App::activate` and `Window::activate_window` (the latter even injects a
//!   synthetic Alt press to win the foreground);
//! - `Window::resize`: its `SetWindowPos` has neither `SWP_NOACTIVATE` nor
//!   `SWP_NOZORDER`, so it brings the window to the top and activates it.
//!   Open the window at the size it needs instead; scenarios resize with the
//!   driver's `resize` command, which keeps the window where it is;
//! - `Window::zoom_window` and opening with `WindowBounds::Maximized`: both
//!   end in `ShowWindow(SW_MAXIMIZE)`, which activates the window and raises
//!   it. [`window_options`] (and so [`open_window`]) opens such a window
//!   windowed at its restore bounds instead.
//!
//! Fullscreen is fine: gpui switches with `SWP_NOACTIVATE | SWP_NOZORDER`.
//!
//! On Linux and macOS only the unfocused, windowed and unthrottled parts
//! apply; on macOS the app also leaves the Dock and the app switcher (an
//! accessory app), and its window stays in front of others, unfocused,
//! because macOS stops drawing a window that is fully covered.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use gpui::{App, Entity, Render, Window, WindowBounds, WindowHandle, WindowOptions};

static BACKGROUND: AtomicBool = AtomicBool::new(false);

/// Switch background mode on or off for windows opened from now on. Call it
/// before opening the first window.
pub fn set_background(background: bool) {
    BACKGROUND.store(background, Ordering::Relaxed);
}

/// Whether windows open in the background (see the [module docs](self)).
pub fn is_background() -> bool {
    BACKGROUND.load(Ordering::Relaxed)
}

/// `options` adjusted for the current mode: in the background the window opens
/// unfocused, windowed rather than maximized, and renders at full rate while
/// inactive; otherwise unchanged.
pub fn window_options(options: WindowOptions) -> WindowOptions {
    if is_background() {
        background_options(options)
    } else {
        options
    }
}

/// `options` for a background window: unfocused and unthrottled (none of
/// gpui's caps while inactive, in Low Power Mode or under thermal pressure, so
/// a run on battery steps as fast as one on the desk), and windowed at the
/// restore bounds of maximized bounds, since gpui maximizes with
/// `ShowWindow(SW_MAXIMIZE)`, which activates the window.
fn background_options(options: WindowOptions) -> WindowOptions {
    let window_bounds = match options.window_bounds {
        Some(WindowBounds::Maximized(bounds)) => Some(WindowBounds::Windowed(bounds)),
        other => other,
    };
    WindowOptions {
        focus: false,
        inactive_frame_interval: None,
        low_power_frame_interval: None,
        thermal_frame_interval: None,
        window_bounds,
        ..options
    }
}

/// [`App::open_window`] for apps that run under automation: in background
/// mode the window opens unfocused and windowed; on Windows also behind every
/// other window, out of the taskbar and unable to be activated; on macOS in
/// front of the others (macOS stops drawing a covered window) with the app
/// out of the Dock (see the [module docs](self)); otherwise this is exactly
/// `cx.open_window(options, build_root_view)`.
/// Open every window of the app through it, not just the first.
pub fn open_window<V: 'static + Render>(
    cx: &mut App,
    options: WindowOptions,
    build_root_view: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> Result<WindowHandle<V>> {
    if !is_background() {
        return cx.open_window(options, build_root_view);
    }
    cx.open_window(window_options(options), |window, cx| {
        // Before the root view is built: the platform window has just been
        // shown (without activation) and has not drawn yet, so on Windows it
        // moves to the back before it ever appears in front (on macOS the app
        // only leaves the Dock: `send_to_back` there).
        if let Err(err) = send_to_back(window) {
            log::warn!("automation: could not move the window to the background: {err:#}");
        }
        build_root_view(window, cx)
    })
}

/// Windows: the bottom of the z-order, a tool window that can't be activated
/// (`background_windows`).
#[cfg(windows)]
fn send_to_back(window: &Window) -> Result<()> {
    crate::background_windows::send_to_back(crate::capture::hwnd(window)?)
}

/// macOS sends nothing to the back, despite the name: the window stays where
/// gpui ordered it (in front, but not key: an inactive app's window), since
/// AppKit stops drawing a window that is fully covered (gpui stops its
/// display link when the window is occluded), which would stall every
/// frame-synced step. The app becomes an accessory
/// instead, out of the Dock and the app switcher, so nothing invites the
/// person at the machine to switch to it.
#[cfg(target_os = "macos")]
fn send_to_back(_window: &Window) -> Result<()> {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    /// `NSApplicationActivationPolicyAccessory`.
    const ACCESSORY: isize = 1;
    // SAFETY: `sharedApplication` and `setActivationPolicy:` are NSApplication's own class and
    // instance methods, called on the main thread (window creation runs there).
    let changed: bool = unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let Some(app) = app.as_ref() else {
            anyhow::bail!("there is no NSApplication");
        };
        msg_send![app, setActivationPolicy: ACCESSORY]
    };
    if !changed {
        anyhow::bail!("NSApplication refused the accessory activation policy");
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn send_to_back(_window: &Window) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn background_windows_open_unfocused_and_unthrottled() {
        let options = WindowOptions::default();
        assert!(options.focus);
        assert_eq!(
            options.inactive_frame_interval,
            Some(Duration::from_micros(33_333))
        );
        let background = background_options(options);
        assert!(!background.focus);
        assert!(
            background.show,
            "the window stays visible so it keeps rendering"
        );
        assert_eq!(background.inactive_frame_interval, None);
        assert_eq!(background.low_power_frame_interval, None);
        assert_eq!(background.thermal_frame_interval, None);
        assert_eq!(background.window_bounds, None);
    }

    #[test]
    fn background_windows_open_windowed_instead_of_maximized() {
        let bounds = gpui::Bounds::new(
            gpui::point(gpui::px(10.), gpui::px(20.)),
            gpui::size(gpui::px(800.), gpui::px(600.)),
        );
        let opened_as = |window_bounds| {
            background_options(WindowOptions {
                window_bounds: Some(window_bounds),
                ..Default::default()
            })
            .window_bounds
        };
        assert_eq!(
            opened_as(WindowBounds::Maximized(bounds)),
            Some(WindowBounds::Windowed(bounds)),
            "maximizing activates the window, so it opens at its restore bounds"
        );
        assert_eq!(
            opened_as(WindowBounds::Windowed(bounds)),
            Some(WindowBounds::Windowed(bounds))
        );
        assert_eq!(
            opened_as(WindowBounds::Fullscreen(bounds)),
            Some(WindowBounds::Fullscreen(bounds)),
            "gpui enters fullscreen without activating"
        );
    }

    struct Focusable {
        handle: gpui::FocusHandle,
        focus_events: usize,
        _subscription: gpui::Subscription,
    }

    impl Render for Focusable {
        fn render(
            &mut self,
            _: &mut Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            use gpui::{InteractiveElement as _, Styled as _};
            gpui::div().track_focus(&self.handle).size_full()
        }
    }

    /// Focuses a fresh window's view, active or not, and reports whether the
    /// view is focused and how many focus events it received.
    fn focus_in_window(active: bool, cx: &mut gpui::TestAppContext) -> (bool, usize) {
        let window = cx.add_window(|window, cx| {
            let handle = cx.focus_handle();
            let subscription = cx.on_focus(&handle, window, |this: &mut Focusable, _, _| {
                this.focus_events += 1
            });
            Focusable {
                handle,
                focus_events: 0,
                _subscription: subscription,
            }
        });
        let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
        if active {
            cx.update(|window, _| window.activate_window());
            cx.run_until_parked();
        } else {
            cx.deactivate_window();
        }
        let view = window.root(cx).unwrap();
        cx.update(|window, cx| {
            let handle = view.read(cx).handle.clone();
            handle.focus(window, cx);
            // Focus events are dispatched when the next frame is drawn.
            window.refresh();
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let view = view.read(cx);
            (view.handle.is_focused(window), view.focus_events)
        })
    }

    /// What tests/e2e/README.md "Background runs" promises: an inactive window
    /// (every background window) tracks focus but sends no focus events.
    #[gpui::test]
    fn inactive_windows_track_focus_without_focus_events(cx: &mut gpui::TestAppContext) {
        assert_eq!(focus_in_window(true, cx), (true, 1), "an active window");
        assert_eq!(focus_in_window(false, cx), (true, 0), "an inactive window");
    }
}
