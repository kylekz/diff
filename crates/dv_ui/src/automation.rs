//! Names for the elements automation scripts address (PLAN §14, `tests/e2e/README.md`), and the
//! one way dv opens windows.
//!
//! Views call `.automation_target("titlebar/close")` unconditionally. With the `automation`
//! feature this is `gpui_automation`'s extension, which records the element's bounds while the
//! driver runs (and is a plain `into_any_element()` otherwise). Without the feature
//! `gpui_automation` (toolbox-rs) is only its no-op stand-ins, which compile to the same
//! `into_any_element()`, so the shipped build, which leaves the driver out, needs no `cfg` in
//! the views.
//!
//! Root views also render [`pointer_guard`] first, which keeps the real mouse from interleaving
//! with scripted input while the driver runs.
//!
//! **Every window dv opens goes through [`open_window`].** Under `--automation-background` it
//! opens the window unfocused, behind every other window and out of the taskbar, so test runs
//! never disturb the person using the machine; otherwise, and without the `automation` feature,
//! it is plain `App::open_window`. While [`is_background`] holds, nothing may activate a window
//! or the app (`Window::activate_window`, `App::activate`), resize it (`Window::resize`) or
//! maximize it (`Window::zoom_window`): on Windows each of these raises and activates it.
//!
//! Names are hierarchical so `query{prefix}` can list a group: `titlebar/…` for the title bar,
//! `shell/…` for the columns, `statusbar/…` for the status bar, then one prefix per view.

/// The name the main window has: the one automation starts with.
pub const MAIN_WINDOW: &str = "main";

pub use gpui_automation::{
    AutomationTargetExt, is_active, is_background, is_capturing, open_window, pointer_guard,
    record_target, register_window, set_background, unregister_window,
};
