use pretty_assertions::assert_eq;

use super::*;

/// `#rrggbb` and the alpha in hundredths of a resolved colour.
fn css(color: Hsla) -> (u32, u32) {
    let c = color.to_rgb();
    let byte = |v: f32| (v * 255.).round() as u32;
    (
        (byte(c.r) << 16) | (byte(c.g) << 8) | byte(c.b),
        (c.a * 1000.).round() as u32,
    )
}

#[test]
fn surfaces_take_the_materials_values_from_design() {
    let acrylic = DvColors::new(Palette::Aura, Appearance::Dark, Material::Acrylic);
    let mica = DvColors::new(Palette::Aura, Appearance::Dark, Material::Mica);
    let opaque = DvColors::new(Palette::Aura, Appearance::Dark, Material::Opaque);
    // DESIGN §1.1: chrome rgba(26,24,36,.56) / rgba(28,26,37,.82) / #110f18.
    assert_eq!(css(acrylic.chrome), (0x1a1824, 560));
    assert_eq!(css(mica.chrome), (0x1c1a25, 820));
    assert_eq!(css(opaque.chrome), (0x110f18, 1000));
    assert_eq!(css(mica.code), (0x15141b, 975));
    assert_eq!(css(opaque.input_bg), (0x0f0e15, 1000));

    let light = DvColors::new(Palette::Aura, Appearance::Light, Material::Mica);
    // DESIGN §1.2: chrome Mica rgba(239,238,244,.85), code rgba(254,254,255,.975).
    assert_eq!(css(light.chrome), (0xefeef4, 850));
    assert_eq!(css(light.code), (0xfefeff, 975));
    assert_eq!(css(light.fg), (0x1d1b26, 1000));
    assert_eq!(css(light.accent), (0x6d43e0, 1000));
}

#[test]
fn solid_tokens_match_design() {
    let dark = DvColors::new(Palette::Aura, Appearance::Dark, Material::Mica);
    assert_eq!(css(dark.fg), (0xedecee, 1000));
    assert_eq!(css(dark.fg3), (0x9d9ab3, 1000));
    assert_eq!(css(dark.accent), (0xa277ff, 1000));
    assert_eq!(css(dark.accent_text), (0xc4a9ff, 1000));
    assert_eq!(css(dark.line), (0xffffff, 80));
    assert_eq!(css(dark.scrim), (0x06050a, 560));
    assert_eq!(css(dark.add_bg), (0x61ffca, 85));
    assert_eq!(css(dark.syntax.property), (0xf694ff, 1000));
    // Comments are the lowest text tier in both appearances.
    assert_eq!(dark.syntax.comment, dark.fg3);
    let light = DvColors::new(Palette::Aura, Appearance::Light, Material::Mica);
    assert_eq!(css(light.syntax.comment), (0x5e5a6f, 1000));
    assert_eq!(light.syntax.comment, light.fg3);
    // Solid tokens don't depend on the material.
    let acrylic = DvColors::new(Palette::Aura, Appearance::Dark, Material::Acrylic);
    assert_eq!(dark.fg, acrylic.fg);
    assert_eq!(dark.line2, acrylic.line2);
    assert_eq!(dark.syntax, acrylic.syntax);
}

#[test]
fn opaque_surfaces_are_opaque_and_translucent_ones_keep_design_minimums() {
    for appearance in [Appearance::Dark, Appearance::Light] {
        let opaque = DvColors::new(Palette::Aura, appearance, Material::Opaque);
        for (name, color) in [
            ("chrome", opaque.chrome),
            ("panel", opaque.panel),
            ("code", opaque.code),
            ("code_head", opaque.code_head),
            ("layer_strong", opaque.layer_strong),
            ("thread", opaque.thread),
            ("overlay", opaque.overlay),
            ("ctrl", opaque.ctrl),
            ("input_bg", opaque.input_bg),
        ] {
            assert_eq!(color.a, 1., "{appearance:?} opaque {name}");
        }
        // DESIGN §3: the diff, threads and overlays stay near-opaque over the material (code
        // >= 94%, though DESIGN §1.2 gives light Acrylic's code 93%; thread >= 98%; overlays
        // >= 96.5%).
        for material in [Material::Mica, Material::Acrylic] {
            let c = DvColors::new(Palette::Aura, appearance, material);
            assert!(
                c.code.a >= 0.93,
                "{appearance:?} {material:?} code {}",
                c.code.a
            );
            assert!(c.thread.a >= 0.98, "{appearance:?} {material:?} thread");
            assert!(c.overlay.a >= 0.965, "{appearance:?} {material:?} overlay");
        }
    }
}

#[test]
fn the_window_background_shows_the_material_except_when_opaque() {
    for appearance in [Appearance::Dark, Appearance::Light] {
        for material in [Material::Mica, Material::Acrylic] {
            let c = DvColors::new(Palette::Aura, appearance, material);
            assert_eq!(c.window_background.a, 0., "{appearance:?} {material:?}");
        }
        let opaque = DvColors::new(Palette::Aura, appearance, Material::Opaque);
        assert_eq!(opaque.window_background, opaque.chrome);
    }
}

#[test]
fn the_lowest_text_tier_meets_the_contrast_floor_on_solid_surfaces() {
    // DESIGN §1.1, §1.3: fg3 is >= 4.5:1 on every surface's Opaque value, in every theme; the
    // opaque-only themes' hues, accent text and syntax colours too, and their fg2 >= 7:1. Over
    // Acrylic and Mica the contrast also depends on the desktop behind the window (DESIGN §6),
    // which no test controls.
    for palette in Palette::ALL {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let c = DvColors::new(palette, appearance, Material::Opaque);
            let mut text = vec![("fg3", c.fg3, 4.5)];
            if !palette.is_glass() {
                let x = c.syntax;
                text.extend([
                    ("fg2", c.fg2, 7.),
                    ("accent_text", c.accent_text, 4.5),
                    ("green", c.green, 4.5),
                    ("red", c.red, 4.5),
                    ("yellow", c.yellow, 4.5),
                    ("blue", c.blue, 4.5),
                    ("keyword", x.keyword, 4.5),
                    ("function", x.function, 4.5),
                    ("string", x.string, 4.5),
                    ("type", x.ty, 4.5),
                    ("constant", x.constant, 4.5),
                    ("property", x.property, 4.5),
                ]);
            }
            for (name, surface) in [
                ("chrome", c.chrome),
                ("panel", c.panel),
                ("code", c.code),
                ("code_head", c.code_head),
                ("thread", c.thread),
                ("overlay", c.overlay),
            ] {
                // Syntax colours are only ever on code.
                let on_code = matches!(name, "code" | "code_head" | "thread");
                for &(tier, color, floor) in &text {
                    let syntax = !matches!(
                        tier,
                        "fg3" | "fg2" | "accent_text" | "green" | "red" | "yellow" | "blue"
                    );
                    if syntax && !on_code {
                        continue;
                    }
                    let ratio = contrast_ratio(color, surface);
                    assert!(
                        ratio >= floor,
                        "{palette:?} {appearance:?} {tier} on {name}: {ratio:.2}"
                    );
                }
            }
        }
    }
}

#[test]
fn the_opaque_themes_force_opaque_and_dracula_is_dark_only() {
    let claude = DvTheme::new(Palette::Claude, Appearance::Light, Material::Acrylic);
    assert_eq!(
        (claude.appearance, claude.material),
        (Appearance::Light, Material::Opaque)
    );
    assert_eq!(claude.key(), "claude-light");
    let dracula = DvTheme::new(Palette::Dracula, Appearance::Light, Material::Mica);
    assert_eq!(
        (dracula.appearance, dracula.material),
        (Appearance::Dark, Material::Opaque)
    );
    assert_eq!(
        dracula.colors.code,
        DvColors::new(Palette::Dracula, Appearance::Dark, Material::Opaque).code
    );
    assert_eq!(css(dracula.colors.code), (0x282a36, 1000));
    let aura = DvTheme::new(Palette::Aura, Appearance::Light, Material::Mica);
    assert_eq!(
        (aura.appearance, aura.material),
        (Appearance::Light, Material::Mica)
    );
    for palette in Palette::ALL {
        assert_eq!(Palette::from_key(palette.key()), Some(palette));
    }
    assert_eq!(Palette::from_key("Aura Dark"), None);
}

#[test]
fn materials_spell_their_keys_in_settings() {
    for material in Material::ALL {
        assert_eq!(Material::from_key(material.key()), Some(material));
        assert_eq!(
            serde_json::to_value(material).unwrap(),
            serde_json::Value::String(material.key().into())
        );
    }
    assert_eq!(Material::from_key("Mica"), None);
}

#[test]
fn vibrancy_takes_acrylics_surfaces() {
    for palette in Palette::ALL {
        for appearance in Appearance::ALL {
            let acrylic = DvColors::new(palette, appearance, Material::Acrylic);
            let vibrancy = DvColors::new(palette, appearance, Material::Vibrancy);
            assert_eq!(vibrancy.chrome, acrylic.chrome);
            assert_eq!(vibrancy.panel, acrylic.panel);
            assert_eq!(vibrancy.window_background, gpui::transparent_black());
        }
    }
}

#[test]
fn sizes_follow_design() {
    let sizes = Sizes::default();
    assert_eq!(sizes.title_bar_height, px(40.));
    assert_eq!(sizes.status_bar_height, px(28.));
    assert_eq!(sizes.panel_width, px(280.));
    assert!(sizes.panel_min_width <= sizes.panel_width);
    assert!(sizes.panel_width <= sizes.panel_max_width);
    assert!(sizes.window_min_width <= sizes.window_width);
    assert!(sizes.window_min_height <= sizes.window_height);
}

#[test]
fn the_type_scale_uses_the_bundled_families() {
    let theme = DvTheme::new(Palette::Aura, Appearance::Dark, Material::Mica);
    assert_eq!(theme.text.body.family.as_ref(), fonts::GEIST);
    assert_eq!(theme.text.body.size, px(13.));
    assert_eq!(theme.text.body.line_height, px(18.));
    assert_eq!(theme.text.kbd.family.as_ref(), fonts::JETBRAINS_MONO);
    assert_eq!(theme.text.code.line_height, theme.sizes.code_row_height);
}

#[test]
fn the_component_theme_takes_the_palette() {
    let dv = DvTheme::new(Palette::Aura, Appearance::Light, Material::Opaque);
    let mut theme = Theme::default();
    map_component_theme(&dv, &mut theme);
    assert_eq!(theme.mode, ThemeMode::Light);
    assert_eq!(theme.background, dv.colors.chrome);
    assert_eq!(theme.tokens.background.color, dv.colors.chrome);
    assert_eq!(theme.foreground, dv.colors.fg);
    assert_eq!(theme.primary, dv.colors.accent);
    assert_eq!(theme.popover, dv.colors.overlay);
    assert_eq!(theme.font_family, dv.fonts.ui);
    assert_eq!(theme.mono_font_family, dv.fonts.mono);

    let mica = DvTheme::new(Palette::Aura, Appearance::Dark, Material::Mica);
    map_component_theme(&mica, &mut theme);
    assert_eq!(theme.mode, ThemeMode::Dark);
    assert_eq!(theme.tokens.background.color.a, 0., "Root lets Mica show");
}

#[gpui::test]
fn install_switches_the_theme_once(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        apply(cx);
        assert_eq!(cx.dv_theme().material, Material::Opaque);
        assert!(Theme::global(cx).is_dark());

        install(Palette::Aura, Appearance::Light, Material::Acrylic, cx);
        let dv = cx.dv_theme();
        assert_eq!(
            (dv.appearance, dv.material),
            (Appearance::Light, Material::Acrylic)
        );
        let theme = Theme::global(cx);
        assert!(!theme.is_dark());
        assert_eq!(theme.background.a, 0.);
        assert_eq!(theme.foreground, dv.colors.fg);

        // Dracula over the same settings: dark and opaque, its own colours to the components.
        install(Palette::Dracula, Appearance::Light, Material::Acrylic, cx);
        let dv = cx.dv_theme();
        assert_eq!(
            (dv.palette, dv.appearance, dv.material),
            (Palette::Dracula, Appearance::Dark, Material::Opaque)
        );
        let theme = Theme::global(cx);
        assert!(theme.is_dark());
        assert_eq!(theme.foreground, dv.colors.fg);
    });
}

#[test]
fn markdowns_code_takes_dvs_syntax_colours() {
    use gpui_base::input::HighlightStyleResolver as _;
    for palette in Palette::ALL {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let dv = DvTheme::new(palette, appearance, Material::Opaque);
            let theme = highlight_theme(&dv);
            let x = dv.colors.syntax;
            for (capture, color) in [
                ("keyword", x.keyword),
                ("function", x.function),
                ("string", x.string),
                ("type", x.ty),
                ("comment", x.comment),
            ] {
                let style = theme.style(capture);
                assert_eq!(
                    style.and_then(|style| style.color).map(css),
                    Some(css(color)),
                    "{palette:?} {appearance:?} {capture}"
                );
            }
        }
    }
}
