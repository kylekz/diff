//! dv's views and services: the Glass shell, switcher, palette, file list, diff stream, threads,
//! submit sheet, onboarding and settings, over services installed as gpui globals
//! (docs/PLAN.md §11, docs/DESIGN.md).
//!
//! gpui: **yes**. Together with the `dv` binary and `gpui_automation`, this is the only crate that
//! may depend on gpui (PLAN §4); everything it shows comes from `dv_core`, `dv_backend` and
//! `dv_cli` through their headless APIs.
//!
//! Startup order (the `dv` binary): [`init`] (gpui-component, fonts, a first theme), then
//! [`system::install`], [`services::init`] and [`appearance::init`] (which resolves the settings'
//! material and mode into the theme the first frame uses), then the main window with a
//! [`shell::Shell`] as its root view, opened through [`automation::open_window`].
//!
//! An open review is a [`session::ReviewSession`] the shell holds and hands to the views around
//! the stream: the [`switcher`] (and its source picker) in the title bar, the [`file_list`] and
//! the [`comments_list`] in the panel, the [`diff_pane`] with its [`thread`] cards and composer
//! (and, in a one-file scope, the file view, with the [`file_finder`] and the [`history`] of
//! positions), and the [`submit`] sheet that finishes a round. [`syntax`] highlights its diffs into token
//! kinds and [`markdown`] renders its comment bodies, sanitised. [`keymap`] binds the
//! [`actions`]; [`banner`] is how a region shows a failure. [`setup`] is the first-run screen
//! and Settings › Setup over the setup checks (PLAN §12).

pub mod actions;
pub mod agents;
pub mod app_menu;
pub mod appearance;
pub mod assets;
pub mod automation;
pub mod banner;
pub mod comments_list;
pub mod components;
pub mod diff_pane;
pub mod failure;
pub mod file_finder;
pub mod file_list;
pub mod history;
pub mod icons;
pub mod intel;
pub mod keymap;
pub mod markdown;
pub mod palette;
pub mod services;
pub mod session;
pub mod setup;
pub mod shell;
pub mod submit;
pub mod switcher;
pub mod syntax;
pub mod system;
#[cfg(test)]
mod test_support;
pub mod theme;
pub mod thread;

pub use assets::Assets;
pub use failure::Failure;
pub use icons::DvIcon;
pub use services::{ActiveServices, Services};
pub use session::ReviewSession;
pub use shell::Shell;
pub use theme::{ActiveTheme, Appearance, DvTheme, Material};

use gpui::App;

/// Everything views need before the first window opens: gpui-component, dv's key bindings (after
/// gpui-component's, so dv's win within a context), macOS's menu bar, the bundled fonts, a first
/// theme and dv's corrections to gpui-component's grammars (`syntax::register_languages`). The
/// app must have been built with [`Assets`] as its asset source.
pub fn init(cx: &mut App) -> anyhow::Result<()> {
    gpui_component::init(cx);
    syntax::register_languages();
    keymap::init(cx)?;
    app_menu::init(cx);
    assets::load_fonts(cx)?;
    theme::apply(cx);
    Ok(())
}
