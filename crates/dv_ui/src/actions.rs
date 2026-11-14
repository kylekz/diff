//! dv's actions (DESIGN §5), one module per key context; the bindings live in
//! [`crate::keymap`]. Each module is also the action namespace, so names read
//! `shell::ToggleSwitcher` or `diff_pane::NextFile`, and automation's `actions` command lists
//! them all.

/// The app's own actions on macOS (`crate::app_menu`): its menu bar's and the keys every Mac
/// app has. Handled by the app, wherever the keyboard is.
pub mod app {
    use gpui::actions;

    actions!(
        app,
        [
            /// Show dv's name and version (dv › About dv).
            About,
            /// Quit dv (`⌘Q`).
            Quit,
            /// Hide dv (`⌘H`).
            Hide,
            /// Hide the other apps (`⌥⌘H`).
            HideOthers,
            /// Show every app again.
            ShowAll,
            /// Minimize the window to the Dock (`⌘M`).
            Minimize,
        ]
    );
}

/// The shell's own actions, dispatched in the `Shell` key context.
pub mod shell {
    use gpui::actions;

    actions!(
        shell,
        [
            /// Open the switcher (repos and reviews, DESIGN §4.1), or close it (`Ctrl O`).
            ToggleSwitcher,
            /// Show or hide the panel (`Ctrl B`, DESIGN §2).
            TogglePanel,
            /// Open the "Finish round" sheet (`Ctrl Enter` while no text field has the keyboard,
            /// DESIGN §4.2).
            FinishReview,
            /// Open the file finder, or close it (`Ctrl P`, PLAN §18.1).
            FindFile,
            /// Open Settings, or close it (`Ctrl ,`, DESIGN §4.3).
            ToggleSettings,
            /// Open the palette, or close it (`Ctrl K`, DESIGN §4.1).
            TogglePalette,
            /// Add a repository through the folder picker (the switcher's "Open folder…").
            OpenFolder,
        ]
    );
}

/// The palette (`Ctrl K`, DESIGN §4.1), dispatched in the `Palette` key context while it is
/// open.
pub mod palette {
    use gpui::actions;

    actions!(
        palette,
        [
            /// Select the next row (`↓`), wrapping to the first.
            SelectNext,
            /// Select the previous row (`↑`), wrapping to the last.
            SelectPrevious,
            /// Open or run the selected row (`Enter`).
            Confirm,
            /// Close without doing anything (`Esc`).
            Dismiss,
        ]
    );
}

/// The Settings sheet (DESIGN §4.3), dispatched in the `Settings` key context while it is open.
pub mod settings {
    use gpui::actions;

    actions!(
        settings,
        [
            /// Close Settings (`Esc`).
            Dismiss,
        ]
    );
}

/// The first-run screen (DESIGN §4.3), dispatched in the `FirstRun` key context while it shows.
pub mod first_run {
    use gpui::actions;

    actions!(
        first_run,
        [
            /// Continue: the first run is over (`Enter`).
            Continue,
        ]
    );
}

/// The switcher's list, dispatched in the `Switcher` key context while it is open.
pub mod switcher {
    use gpui::actions;

    actions!(
        switcher,
        [
            /// Select the next row (`↓`), wrapping to the first.
            SelectNext,
            /// Select the previous row (`↑`), wrapping to the last.
            SelectPrevious,
            /// Open what the selected row stands for (`Enter`).
            Confirm,
            /// Remove the selected row's repository from the list (`Ctrl Shift Backspace`).
            RemoveRepo,
            /// Ask whether to close the selected row's review (`Ctrl Shift W`); `Enter` then
            /// closes it.
            CloseReview,
            /// Close without opening anything (`Esc`).
            Dismiss,
        ]
    );
}

/// The source picker's menu, dispatched in the `SourcePicker` key context while it is open.
pub mod source_picker {
    use gpui::actions;

    actions!(
        source_picker,
        [
            /// Select the next target (`↓`), wrapping to the first.
            SelectNext,
            /// Select the previous target (`↑`), wrapping to the last.
            SelectPrevious,
            /// Open or resume the selected target's review (`Enter`).
            Confirm,
            /// Close without opening anything (`Esc`).
            Dismiss,
        ]
    );
}

/// The diff stream's keys (DESIGN §5), dispatched in the `DiffPane` key context: the main
/// area, which has the keyboard whenever no overlay or text field does.
pub mod diff_pane {
    use gpui::actions;

    actions!(
        diff_pane,
        [
            /// Show the next file: its header at the top, the cursor on its first change (`J`).
            NextFile,
            /// Show the previous file (`K`).
            PreviousFile,
            /// Move the cursor to the next change block (`N`).
            NextChange,
            /// Move the cursor to the previous change block (`P`).
            PreviousChange,
            /// Mark the current file viewed, or not viewed (`V`).
            ToggleViewed,
            /// Reveal one step of the gap nearest the cursor, on the cursor's side (`E`).
            ExpandGap,
            /// Reveal the whole gap nearest the cursor (`Shift E`).
            ExpandGapFully,
            /// Switch between split and unified, remembered per review (`S`).
            ToggleSplit,
            /// Move the cursor up a line (`↑`).
            CursorUp,
            /// Move the cursor down a line (`↓`).
            CursorDown,
            /// Extend the selection up a line (`Shift ↑`).
            SelectUp,
            /// Extend the selection down a line (`Shift ↓`).
            SelectDown,
            /// Put the cursor on the old side, in split view (`←`).
            CursorLeft,
            /// Put the cursor on the new side, in split view (`→`).
            CursorRight,
            /// Scroll up a page, carrying the cursor (`PageUp`).
            PageUp,
            /// Scroll down a page, carrying the cursor (`PageDown`).
            PageDown,
            /// Close the file menu, else cancel the composer, else clear the selection (`Esc`).
            ClearSelection,
            /// Comment on the selected lines, else the cursor's line (`C`).
            Comment,
            /// Comment on the whole of the current file (`Shift C`, PLAN §8).
            CommentOnFile,
            /// Reply to the focused thread (`R`).
            Reply,
            /// Focus the next unresolved thread (`]`).
            NextThread,
            /// Focus the previous unresolved thread (`[`).
            PreviousThread,
            /// Resolve the focused thread, or reopen it when it is resolved (`X`).
            ResolveThread,
            /// Switch between the interdiff since the last round and all changes (`I`, PLAN
            /// §2.3).
            ToggleComparison,
            /// Show or hide the agent's annotations (`A`, PLAN §19.3).
            ToggleAnnotations,
            /// Copy the selected text, else the selected lines (`Ctrl C`, PLAN §18.1).
            Copy,
            /// Open the find bar with its field focused, or focus it again with its text
            /// selected (`Ctrl F`, PLAN §18.1).
            Find,
            /// Open the current file whole in the file view, at the cursor's line (`F`, PLAN
            /// §18.1).
            ViewFile,
            /// Go back to the position before the last jump (`Alt ←`, PLAN §18.1).
            GoBack,
            /// Go forward again (`Alt →`).
            GoForward,
            /// Show the hover card for the symbol at the caret (`Ctrl I`, PLAN §18.1).
            HoverAtCaret,
            /// Go to the definition of the symbol at the caret (`F12`).
            GoToDefinition,
            /// Find the references of the symbol at the caret (`Shift F12`).
            FindReferences,
        ]
    );
}

/// The stream's find bar (DESIGN §4.1), dispatched in the `Find` key context while its field
/// has the keyboard.
pub mod find {
    use gpui::actions;

    actions!(
        find,
        [
            /// Go to the next match, wrapping (`Enter`).
            Next,
            /// Go to the previous match, wrapping (`Shift Enter`).
            Previous,
            /// Close the bar and give the keyboard back to the stream (`Esc`).
            Close,
        ]
    );
}

/// The file finder (`Ctrl P`, DESIGN §4.1), dispatched in the `FileFinder` key context while
/// it is open.
pub mod file_finder {
    use gpui::actions;

    actions!(
        file_finder,
        [
            /// Select the next file (`↓`), wrapping to the first.
            SelectNext,
            /// Select the previous file (`↑`), wrapping to the last.
            SelectPrevious,
            /// Open the selected file in the file view (`Enter`).
            Confirm,
            /// Close without opening anything (`Esc`).
            Dismiss,
        ]
    );
}

/// The composer (a new thread or a reply, DESIGN §4.1), dispatched in the `Composer` key
/// context while its text field has the keyboard.
pub mod composer {
    use gpui::actions;

    actions!(
        composer,
        [
            /// Post the comment (`Ctrl Enter`).
            Post,
            /// Discard the comment and give the keyboard back to the stream (`Esc`).
            Cancel,
        ]
    );
}

/// The "Finish round" sheet (DESIGN §4.2), dispatched in the `Sheet` key context while it is
/// open.
pub mod sheet {
    use gpui::actions;

    actions!(
        sheet,
        [
            /// Finish the round with the chosen verdict (`Ctrl Enter`).
            Submit,
            /// Close the sheet without finishing anything (`Esc`).
            Dismiss,
            /// Choose the next verdict (`↓`).
            SelectNext,
            /// Choose the previous verdict (`↑`).
            SelectPrevious,
            /// Choose Approve (`1`).
            PickApprove,
            /// Choose Request changes (`2`).
            PickRequestChanges,
            /// Move the keyboard between the verdicts and the summary (`Tab`).
            SwitchFocus,
        ]
    );
}

/// The file list's filter field, dispatched in the `FileList` key context.
pub mod file_list {
    use gpui::actions;

    actions!(
        file_list,
        [
            /// Show the first file the filter leaves and hand the keyboard back to the stream
            /// (`Enter` in the filter).
            RevealFirst,
            /// Clear the filter and hand the keyboard back to the stream (`Esc` in the filter).
            ClearFilter,
        ]
    );
}

/// Every text field dv shows (gpui-component's `Input`: the find field, the composer, the
/// filters, the sheet's summary), dispatched in the `Input` key context ahead of the field's own
/// copy and cut, so the text goes through `dv_ui::system::write_clipboard` (CLAUDE.md: every
/// clipboard write goes through the seam).
pub mod field {
    use gpui::actions;

    actions!(
        field,
        [
            /// Copy the field's selected text (`Ctrl C`).
            Copy,
            /// Copy the field's selected text and remove it (`Ctrl X`).
            Cut,
        ]
    );
}
