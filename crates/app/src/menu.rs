//! macOS native application menu (S8i, docs/phase-8-lsp-and-polish.md
//! §macOS) — "platform conventions: cmd-key bindings, native menu bar,
//! titlebar."
//!
//! Every item here drives an action that already exists and is already
//! reachable through its own keybinding (`NewReview`, `OpenSettings`,
//! `OpenThemePicker`, `OpenPrPicker`, `JumpToFile`) — the menu is just a
//! second way to reach the same handful of entry points, not new product
//! surface. `About`/`Quit` are the only two genuinely new actions, and
//! both are the minimum a native macOS App menu needs (the standard
//! About/Preferences/Quit triad).
//!
//! Nothing in this module is behind a platform `cfg` — `Menu`/`MenuItem`
//! are ordinary cross-platform gpui types, and leaving the module itself
//! ungated means it compiles (and would clippy-fail on dead code) on every
//! target, which is exactly the proof this slice's Windows verification
//! ceiling needs. Only the *call sites* — `cx.set_menus(..)` and the
//! `cmd-q` keybinding — are `#[cfg(target_os = "macos")]`, in `main.rs`.
//! On Windows/Linux this module compiles but is inert: `init` is never
//! called, `app_menus` is never called, nothing changes.

use gpui::{App, Menu, MenuItem, actions};
use gpui_component::WindowExt as _;

use crate::shell::{NewReview, OpenSettings, OpenThemePicker};
use crate::workspace::{JumpToFile, OpenPrPicker};

actions!(menu, [About, Quit]);

/// Registers the global handlers the App menu's `About`/`Quit` items (and
/// the `cmd-q` keybinding) dispatch to. Mirrors gpui's own `set_menus`
/// example (`crates/gpui/examples/set_menus.rs`): a plain `cx.on_action`
/// per action, `Quit`'s handler just calling `cx.quit()`. Call once,
/// before the window opens — `main.rs`'s `run_gui` does this right
/// alongside `cx.set_menus(app_menus())`, both `#[cfg(target_os =
/// "macos")]`.
// Both functions below are only ever called from `main.rs`'s
// `#[cfg(target_os = "macos")]` block — on every other target the module
// compiles (that's the point) but nothing reaches them, which `-D
// warnings` would otherwise flag as dead code.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn init(cx: &mut App) {
    // Global binding (`None` context) — cmd-q is the standard macOS
    // termination shortcut and isn't tied to any particular view's focus,
    // unlike every other binding in shell.rs/workspace.rs.
    cx.bind_keys([gpui::KeyBinding::new("cmd-q", Quit, None)]);

    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());

    cx.on_action(|_: &About, cx: &mut App| {
        // Deferred (review finding P2): this handler runs as a global
        // `cx.on_action` listener, and the menu's dispatch path
        // (`on_app_menu_action` -> `App::dispatch_action` ->
        // `Window::dispatch_action`) fires those listeners from *inside*
        // `App::update_window_id`'s in-flight update of the very window
        // this handler wants to open a dialog on — `dispatch_action`
        // `cx.defer()`s the actual dispatch, and the deferred closure runs
        // global bubble-phase listeners while `cx.windows.get_mut(id)`'s
        // slot is still `take()`n (gpui/src/window.rs
        // `dispatch_action_on_node_inner`; gpui/src/app.rs
        // `update_window_id`). A synchronous `window.update(cx, ..)` here
        // would target that same emptied slot and silently return `Err`
        // ("window not found"), so "About dv" would never open with the
        // window active — the common case. `cx.defer` schedules this body
        // for the end of the current effect cycle, after `update_window_id`
        // has returned the window to its slot, so the update succeeds
        // whether the window is active or (per the comment above) merely
        // the sole minimized one.
        cx.defer(|cx| {
            // Prefer the active/key window, but fall back to any open
            // window: on macOS the app menu stays usable while the sole
            // window is minimized (clicking the menu activates the app
            // without making a minimized window key), so `active_window()`
            // alone would make this a silent no-op instead of showing the
            // About dialog like a native mac app.
            let Some(window) = cx
                .active_window()
                .or_else(|| cx.windows().into_iter().next())
            else {
                return;
            };
            // `open_alert_dialog` requires the window's root view to be a
            // `gpui_component::Root` (it downcasts internally) — true here,
            // `main.rs::run_gui` always builds the window with `Root::new`.
            let _ = window.update(cx, |_, window, cx| {
                window.open_alert_dialog(cx, |alert, _, _| {
                    alert.title("About dv").description(format!(
                        "dv {}\nLocal diff viewer / PR reviewer",
                        env!("CARGO_PKG_VERSION")
                    ))
                });
            });
        });
    });
}

/// Builds the native macOS menu bar. Every item is an existing action —
/// dispatching it from the menu bubbles through the focused view's action
/// handlers exactly as the matching keybinding already does (`cx.
/// set_menus`'s `on_app_menu_action` hook dispatches via `cx.
/// dispatch_action`, the same path a keystroke takes).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu::new("dv").items(vec![
            MenuItem::action("About dv", About),
            MenuItem::separator(),
            MenuItem::action("Settings...", OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Quit dv", Quit),
        ]),
        Menu::new("File").items(vec![MenuItem::action("New Review", NewReview)]),
        // Standard cut/copy/paste/undo/redo — reusing gpui_component::
        // input's already-bound actions as-is (they already drive every
        // focused `InputState`), not new plumbing of our own.
        Menu::new("Edit").items(vec![
            MenuItem::action("Undo", gpui_component::input::Undo),
            MenuItem::action("Redo", gpui_component::input::Redo),
            MenuItem::separator(),
            MenuItem::action("Cut", gpui_component::input::Cut),
            MenuItem::action("Copy", gpui_component::input::Copy),
            MenuItem::action("Paste", gpui_component::input::Paste),
            MenuItem::separator(),
            MenuItem::action("Select All", gpui_component::input::SelectAll),
        ]),
        Menu::new("View").items(vec![MenuItem::action("Theme...", OpenThemePicker)]),
        Menu::new("Go").items(vec![
            MenuItem::action("Jump to File", JumpToFile),
            MenuItem::action("Open PR...", OpenPrPicker),
        ]),
    ]
}
