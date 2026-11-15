//! The palette in the shell (`Ctrl K`, the title bar's command field; DESIGN §4.1 "Palette"):
//! opening it like the other menus, and running what it picked. A command runs as its key would:
//! a stream command once the main area has the keyboard back, a shell command from wherever the
//! keyboard was before the palette opened.

use gpui::{AppContext as _, Context, Entity, Window};

use super::{Overlay, Shell};
use crate::actions::shell::{OpenFolder, TogglePalette};
use crate::keymap;
use crate::palette::{Palette, PaletteEvent};
use crate::services::ActiveServices as _;

impl Shell {
    /// The palette, while it is open.
    pub fn palette(&self) -> Option<&Entity<Palette>> {
        self.palette.as_ref().map(|overlay| &overlay.view)
    }

    /// `Ctrl K`: the palette, or closed when it is open.
    pub(super) fn toggle_palette(
        &mut self,
        _: &TogglePalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette.is_some() {
            self.close_menu(window, cx);
        } else {
            self.open_palette(window, cx);
        }
    }

    /// Opens the palette in place of an open menu, over an open sheet. Every known repository's
    /// row is recomputed meanwhile, as for the switcher.
    pub fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let return_focus = self.take_menu().unwrap_or_else(|| window.focused(cx));
        self.close_comments_menu(cx);
        let reviews = cx.services().reviews.clone();
        reviews.update(cx, |reviews, cx| reviews.refresh_all(cx));
        // Over the "Finish round" sheet the review's commands and files would act under it (a
        // file view opening beneath it): the palette offers the rest.
        let session = self.session.clone().filter(|_| self.sheet.is_none());
        let github = cx.services().github.clone();
        let in_view = self.repo_in_view(cx);
        let view = cx.new(|cx| Palette::new(reviews, github, in_view, session, window, cx));
        let subscription = cx.subscribe_in(&view, window, Self::on_palette_event);
        view.update(cx, |palette, cx| palette.focus(window, cx));
        self.palette = Some(Overlay {
            view,
            return_focus,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn on_palette_event(
        &mut self,
        palette: &Entity<Palette>,
        event: &PaletteEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PaletteEvent::Dismiss => {
                self.close_menu(window, cx);
            }
            PaletteEvent::Open { location, request } => {
                self.close_menu(window, cx);
                self.open(location.clone(), request.clone(), window, cx)
                    .detach();
            }
            PaletteEvent::ChooseFolder(reference) => {
                let asker = super::pr::FolderAsker::Palette(palette.entity_id());
                self.choose_pr_folder(reference.clone(), asker, window, cx);
            }
            PaletteEvent::OpenFile(path) => {
                // The keyboard goes to the file view, not back where the palette opened from.
                self.palette = None;
                self.open_file_view(path, None, true, window, cx);
            }
            PaletteEvent::Run(command) => {
                self.close_menu(window, cx);
                if command.context == keymap::DIFF_PANE {
                    self.main_focus(cx).focus(window, cx);
                }
                window.dispatch_action((command.action)(), cx);
            }
        }
    }

    /// The palette's "Open folder…".
    pub(super) fn open_folder(
        &mut self,
        _: &OpenFolder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_folder(window, cx);
    }
}
