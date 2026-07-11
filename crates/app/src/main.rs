#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui::*;
use gpui_component::{button::*, *};

pub struct Placeholder;

impl Render for Placeholder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(TitleBar::new().child(dv_core::APP_NAME))
            .child(
                v_flex()
                    .gap_2()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .child(div().text_xl().child("dv"))
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child("a fast diff viewer — phase 0 scaffold"),
                    )
                    .child(
                        Button::new("phase-0")
                            .primary()
                            .label("Phase 0: it renders")
                            .on_click(|_, _, _| println!("clicked")),
                    ),
            )
    }
}

fn main() {
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(move |cx| {
        gpui_component::init(cx);

        cx.spawn(async move |cx| {
            let window_options = WindowOptions {
                titlebar: Some(TitleBar::title_bar_options()),
                ..Default::default()
            };

            cx.open_window(window_options, |window, cx| {
                let view = cx.new(|_| Placeholder);
                cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}
