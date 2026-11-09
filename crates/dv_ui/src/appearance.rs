//! Which theme is on screen: the settings' palette, material and mode (PLAN D8, DESIGN §1, §3).
//!
//! toolbox-rs's `gpui_appearance` holds the rest: the OS's light/dark setting, followed live,
//! and what the platform can show (Mica and Acrylic on Windows 11 with Transparency effects on,
//! vibrancy on macOS without Reduce transparency), and it applies what they resolve to to each
//! window the shell tracks (`gpui_appearance::track_window`). dv narrows its settings by the
//! palette before handing them over (DESIGN §1.3: Claude and Dracula are opaque, Dracula dark).
//!
//! [`init`] runs after [`crate::services::init`] and before the first window, so the first frame
//! already uses the resolved theme, and from then on follows every settings change and every
//! change the crate sees. Each change installs the theme ([`crate::theme::install`]), which
//! redraws every window.

use gpui::App;
/// What the platform can show, which [`init`] starts from (`MaterialSupport::probe`).
pub use gpui_appearance::MaterialSupport;
use gpui_appearance::{AppearanceState, Mode, Preferences, Setup, Vibrancy};

use crate::services::{ActiveServices as _, SettingsEvent};
use crate::theme::{self, Appearance, Material, Palette};

/// The settings resolved against the platform and the OS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub palette: Palette,
    pub appearance: Appearance,
    pub material: Material,
}

/// The material setting's default (PLAN D8): Mica on Windows, Opaque elsewhere. On macOS too,
/// where vibrancy is a setting until DESIGN §3 has tokens tuned for it, so dv keeps Opaque rather
/// than `gpui_appearance`'s platform default.
pub fn default_material() -> Material {
    if cfg!(windows) {
        Material::Mica
    } else {
        Material::Opaque
    }
}

/// Starts following the settings with `support` as the platform's abilities (from
/// [`MaterialSupport::probe`]) and installs the resolved theme.
pub fn init(support: MaterialSupport, cx: &mut App) {
    gpui_appearance::init(
        Setup {
            support,
            preferences: preferences(cx),
            vibrancy: Vibrancy::default(),
        },
        cx,
    );
    let settings = cx.services().settings.clone();
    cx.subscribe(&settings, |_, event: &SettingsEvent, cx| match event {
        SettingsEvent::Changed => {
            gpui_appearance::set_preferences(preferences(cx), cx);
            // A palette change alone may leave the crate's state as it was.
            sync(cx);
        }
    })
    .detach();
    cx.observe_global::<AppearanceState>(sync).detach();
    sync(cx);
}

/// What dv asks the crate for: the settings' mode and material, as far as the palette has them.
fn preferences(cx: &App) -> Preferences {
    let settings = cx.services().settings.read(cx);
    let palette = settings.palette();
    Preferences {
        mode: if palette.has_light() {
            settings.mode()
        } else {
            Mode::Dark
        },
        material: palette.material(settings.material()),
    }
}

/// The settings resolved now.
pub fn resolved(cx: &App) -> Resolved {
    let gpui_appearance::Resolved {
        appearance,
        material,
    } = gpui_appearance::resolved(cx);
    Resolved {
        palette: cx.services().settings.read(cx).palette(),
        appearance,
        material,
    }
}

fn sync(cx: &mut App) {
    let Resolved {
        palette,
        appearance,
        material,
    } = resolved(cx);
    theme::install(palette, appearance, material, cx);
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;
    use crate::ActiveTheme as _;
    use crate::services::ServicesConfig;

    fn setup(cx: &mut TestAppContext, support: MaterialSupport) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_path_buf();
        cx.update(|cx| {
            crate::init(cx).unwrap();
            crate::services::init(
                ServicesConfig {
                    setup: crate::services::SetupConfig::sandboxed(
                        data_dir.join("home"),
                        data_dir.join("user-path.txt"),
                        false,
                    ),
                    data_dir,
                    wsl: crate::services::WslAccess::scripted(Vec::new()),
                },
                cx,
            );
            init(support, cx);
        });
        dir
    }

    fn on_screen(cx: &mut TestAppContext) -> (Appearance, Material) {
        cx.update(|cx| {
            let theme = cx.dv_theme();
            (theme.appearance, theme.material)
        })
    }

    #[gpui::test]
    fn the_theme_follows_the_settings(cx: &mut TestAppContext) {
        let _dir = setup(cx, MaterialSupport::ALL);
        let settings = cx.update(|cx| cx.services().settings.clone());
        settings.update(cx, |settings, cx| {
            settings.set_mode(Mode::Light, cx);
            settings.set_material(Material::Acrylic, cx);
        });
        assert_eq!(on_screen(cx), (Appearance::Light, Material::Acrylic));
        settings.update(cx, |settings, cx| {
            settings.set_mode(Mode::Dark, cx);
            settings.set_material(Material::Vibrancy, cx);
        });
        assert_eq!(on_screen(cx), (Appearance::Dark, Material::Vibrancy));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn system_mode_follows_the_os(cx: &mut TestAppContext) {
        let _dir = setup(cx, MaterialSupport::ALL);
        let settings = cx.update(|cx| cx.services().settings.clone());
        settings.update(cx, |settings, cx| settings.set_mode(Mode::System, cx));
        cx.update(|cx| gpui_appearance::set_system(Appearance::Light, cx));
        assert_eq!(on_screen(cx).0, Appearance::Light);
        cx.update(|cx| gpui_appearance::set_system(Appearance::Dark, cx));
        assert_eq!(on_screen(cx).0, Appearance::Dark);
        // An explicit mode ignores the OS.
        settings.update(cx, |settings, cx| settings.set_mode(Mode::Light, cx));
        cx.update(|cx| gpui_appearance::set_system(Appearance::Dark, cx));
        assert_eq!(on_screen(cx).0, Appearance::Light);
        cx.run_until_parked();
    }

    #[gpui::test]
    fn the_palette_narrows_the_mode_and_the_material(cx: &mut TestAppContext) {
        let _dir = setup(cx, MaterialSupport::ALL);
        let settings = cx.update(|cx| cx.services().settings.clone());
        settings.update(cx, |settings, cx| {
            settings.set_mode(Mode::Light, cx);
            settings.set_material(Material::Mica, cx);
            settings.set_palette(Palette::Dracula, cx);
        });
        assert_eq!(on_screen(cx), (Appearance::Dark, Material::Opaque));
        settings.update(cx, |settings, cx| settings.set_palette(Palette::Aura, cx));
        assert_eq!(on_screen(cx), (Appearance::Light, Material::Mica));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn the_platform_narrows_the_material(cx: &mut TestAppContext) {
        let _dir = setup(cx, MaterialSupport::OPAQUE_ONLY);
        let settings = cx.update(|cx| cx.services().settings.clone());
        settings.update(cx, |settings, cx| settings.set_material(Material::Mica, cx));
        assert_eq!(on_screen(cx).1, Material::Opaque, "transparency off");
        // Transparency switched back on.
        cx.update(|cx| gpui_appearance::set_support(MaterialSupport::ALL, cx));
        assert_eq!(on_screen(cx).1, Material::Mica);
        assert_eq!(
            cx.update(|cx| resolved(cx).material),
            Material::Mica,
            "resolved matches the screen"
        );
        cx.run_until_parked();
    }
}
