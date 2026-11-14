//! The keymap (DESIGN §5). One table, [`SHORTCUTS`], drives both the gpui bindings and the
//! keycaps views draw ([`label`]), so the two can't drift.
//!
//! gpui gives every matching binding a turn, deepest key context first, and stops at the first
//! handler that consumes the key. gpui-component's text fields (key context `Input`) matter
//! here: a single-line field takes `↑`/`↓` without using them, so list keys driven from a filter
//! field are bound ahead of it ([`Bind::AheadOfInputs`]); it lets `Enter` and an idle `Esc`
//! through, so those rows need nothing special; and single-letter keys must stay out of text
//! fields ([`Bind::OutsideInputs`], DESIGN §5: "single-letter keys are inactive while a text
//! field has focus"). A multi-line field (the composer, the sheet's summary) uses `Enter` and
//! `Ctrl Enter` itself, inserting a newline, so `Ctrl Enter` rows there sit ahead of it too.
//!
//! `secondary` is gpui's platform modifier: `Ctrl` on Windows and Linux, `Cmd` on macOS
//! (DESIGN §5: "macOS uses Cmd for every Ctrl").

use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate, SharedString};

use crate::actions::{
    app, composer, diff_pane, field, file_finder, file_list, find, first_run, palette, settings,
    sheet, shell, source_picker, switcher,
};

/// Key context of the shell's root element: everything in the main window.
pub const SHELL: &str = "Shell";
/// Key context of the main area, where the diff stream is (inside [`SHELL`]). It has the
/// keyboard whenever no overlay or text field does.
pub const DIFF_PANE: &str = "DiffPane";
/// Key context of the open switcher (inside [`SHELL`]).
pub const SWITCHER: &str = "Switcher";
/// Key context of the open source picker (inside [`SHELL`]).
pub const SOURCE_PICKER: &str = "SourcePicker";
/// Key context of the open file finder (inside [`SHELL`]).
pub const FILE_FINDER: &str = "FileFinder";
/// Key context of the open palette (inside [`SHELL`]).
pub const PALETTE: &str = "Palette";
/// Key context of the panel's file list (inside [`SHELL`]).
pub const FILE_LIST: &str = "FileList";
/// Key context of the composer, a new thread's or a reply's (inside [`DIFF_PANE`]).
pub const COMPOSER: &str = "Composer";
/// Key context of the open "Finish round" sheet (inside [`SHELL`]).
pub const SHEET: &str = "Sheet";
/// Key context of the open Settings sheet (inside [`SHELL`]).
pub const SETTINGS: &str = "Settings";
/// Key context of the first-run screen while it shows (inside [`SHELL`]).
pub const FIRST_RUN: &str = "FirstRun";
/// Key context of the stream's find bar (inside [`DIFF_PANE`]).
pub const FIND: &str = "Find";
/// Key context gpui-component sets on its text fields; gpui-base keeps its constant private.
pub const INPUT: &str = "Input";
/// Key context of gpui-base's Markdown views (the hover card's, selectable); its constant is
/// private too.
pub const TEXT_VIEW: &str = "TextView";

/// How a [`Shortcut`] reaches gpui.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bind {
    /// Bound in the row's context, text fields included: it fires from a focused field whenever
    /// the field doesn't consume the key itself.
    InContext,
    /// Bound in the row's context except while a text field has focus (`Context && !Input`).
    OutsideInputs,
    /// Bound on the text fields inside the row's context (`Context > Input`), ahead of the
    /// field's own binding for the key: for keys a field takes without using them.
    AheadOfInputs,
}

/// One row of the keymap.
#[derive(Clone, Copy)]
pub struct Shortcut {
    /// gpui keystroke syntax, e.g. `secondary-o`.
    pub keys: &'static str,
    /// The key context the row belongs to (one of the constants above).
    pub context: &'static str,
    pub action: fn() -> Box<dyn Action>,
    pub bind: Bind,
}

impl std::fmt::Debug for Shortcut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shortcut")
            .field("keys", &self.keys)
            .field("context", &self.context)
            .field("action", &(self.action)().name())
            .field("bind", &self.bind)
            .finish()
    }
}

impl Shortcut {
    /// The context predicate the row is bound under.
    pub fn predicate(&self) -> String {
        match self.bind {
            Bind::InContext => self.context.to_string(),
            Bind::OutsideInputs => format!("{} && !{INPUT}", self.context),
            Bind::AheadOfInputs => format!("{} > {INPUT}", self.context),
        }
    }

    /// The gpui binding for this row, its keys parsed for the app's OS and keyboard.
    pub fn key_binding(&self, cx: &App) -> anyhow::Result<KeyBinding> {
        let predicate = KeyBindingContextPredicate::parse(&self.predicate())?;
        Ok(KeyBinding::load(
            self.keys,
            (self.action)(),
            Some(predicate.into()),
            false,
            None,
            cx,
        )?)
    }

    /// The keycap text, e.g. `Ctrl O`.
    pub fn label(&self) -> SharedString {
        label(self.keys)
    }
}

macro_rules! shortcut {
    ($keys:literal, $context:expr, $action:expr) => {
        shortcut!($keys, $context, $action, InContext)
    };
    ($keys:literal, $context:expr, $action:expr, $bind:ident) => {
        Shortcut {
            keys: $keys,
            context: $context,
            action: || Box::new($action),
            bind: Bind::$bind,
        }
    };
}

/// Every binding dv defines.
pub const SHORTCUTS: &[Shortcut] = &[
    // The shell. Text fields bind neither, so both work from a focused field too.
    shortcut!("secondary-o", SHELL, shell::ToggleSwitcher),
    shortcut!("secondary-b", SHELL, shell::TogglePanel),
    shortcut!("secondary-p", SHELL, shell::FindFile),
    shortcut!("secondary-,", SHELL, shell::ToggleSettings),
    shortcut!("secondary-k", SHELL, shell::TogglePalette),
    // Settings and the first-run screen have the keyboard themselves (no text field).
    shortcut!("escape", SETTINGS, settings::Dismiss),
    shortcut!("enter", FIRST_RUN, first_run::Continue),
    // Finishing the round stays out of text fields: in the composer `Ctrl Enter` posts, in the
    // sheet it submits (both bound in their own contexts, below).
    shortcut!("secondary-enter", SHELL, shell::FinishReview, OutsideInputs),
    // The switcher: its filter field has the keyboard and takes ↑/↓ itself, so those rows sit
    // ahead of it; Enter and an idle Esc pass through the field to the plain rows.
    shortcut!("down", SWITCHER, switcher::SelectNext, AheadOfInputs),
    shortcut!("up", SWITCHER, switcher::SelectPrevious, AheadOfInputs),
    shortcut!("enter", SWITCHER, switcher::Confirm),
    // The filter field deletes text with Backspace and its Ctrl, Alt and Shift forms, never
    // with Ctrl Shift Backspace, which the field would otherwise swallow.
    shortcut!(
        "secondary-shift-backspace",
        SWITCHER,
        switcher::RemoveRepo,
        AheadOfInputs
    ),
    shortcut!(
        "secondary-shift-w",
        SWITCHER,
        switcher::CloseReview,
        AheadOfInputs
    ),
    shortcut!("escape", SWITCHER, switcher::Dismiss),
    // The file finder: as the switcher, its filter field has the keyboard.
    shortcut!("down", FILE_FINDER, file_finder::SelectNext, AheadOfInputs),
    shortcut!(
        "up",
        FILE_FINDER,
        file_finder::SelectPrevious,
        AheadOfInputs
    ),
    shortcut!("enter", FILE_FINDER, file_finder::Confirm),
    shortcut!("escape", FILE_FINDER, file_finder::Dismiss),
    // The palette: as the switcher, its filter field has the keyboard.
    shortcut!("down", PALETTE, palette::SelectNext, AheadOfInputs),
    shortcut!("up", PALETTE, palette::SelectPrevious, AheadOfInputs),
    shortcut!("enter", PALETTE, palette::Confirm),
    shortcut!("escape", PALETTE, palette::Dismiss),
    // The source picker has the keyboard itself (no text field).
    shortcut!("down", SOURCE_PICKER, source_picker::SelectNext),
    shortcut!("up", SOURCE_PICKER, source_picker::SelectPrevious),
    shortcut!("enter", SOURCE_PICKER, source_picker::Confirm),
    shortcut!("escape", SOURCE_PICKER, source_picker::Dismiss),
    // The file list's filter field lets Enter and an idle Esc through.
    shortcut!("enter", FILE_LIST, file_list::RevealFirst),
    shortcut!("escape", FILE_LIST, file_list::ClearFilter),
    // The diff stream (DESIGN §5). Its keys stay out of the text fields in the main area, so the
    // composer and the find bar never lose their letters to them.
    shortcut!("j", DIFF_PANE, diff_pane::NextFile, OutsideInputs),
    shortcut!("k", DIFF_PANE, diff_pane::PreviousFile, OutsideInputs),
    shortcut!("n", DIFF_PANE, diff_pane::NextChange, OutsideInputs),
    shortcut!("p", DIFF_PANE, diff_pane::PreviousChange, OutsideInputs),
    shortcut!("v", DIFF_PANE, diff_pane::ToggleViewed, OutsideInputs),
    shortcut!("e", DIFF_PANE, diff_pane::ExpandGap, OutsideInputs),
    shortcut!(
        "shift-e",
        DIFF_PANE,
        diff_pane::ExpandGapFully,
        OutsideInputs
    ),
    shortcut!("s", DIFF_PANE, diff_pane::ToggleSplit, OutsideInputs),
    shortcut!("up", DIFF_PANE, diff_pane::CursorUp, OutsideInputs),
    shortcut!("down", DIFF_PANE, diff_pane::CursorDown, OutsideInputs),
    shortcut!("shift-up", DIFF_PANE, diff_pane::SelectUp, OutsideInputs),
    shortcut!(
        "shift-down",
        DIFF_PANE,
        diff_pane::SelectDown,
        OutsideInputs
    ),
    shortcut!("left", DIFF_PANE, diff_pane::CursorLeft, OutsideInputs),
    shortcut!("right", DIFF_PANE, diff_pane::CursorRight, OutsideInputs),
    shortcut!("pageup", DIFF_PANE, diff_pane::PageUp, OutsideInputs),
    shortcut!("pagedown", DIFF_PANE, diff_pane::PageDown, OutsideInputs),
    shortcut!(
        "escape",
        DIFF_PANE,
        diff_pane::ClearSelection,
        OutsideInputs
    ),
    shortcut!("c", DIFF_PANE, diff_pane::Comment, OutsideInputs),
    shortcut!(
        "shift-c",
        DIFF_PANE,
        diff_pane::CommentOnFile,
        OutsideInputs
    ),
    shortcut!("r", DIFF_PANE, diff_pane::Reply, OutsideInputs),
    shortcut!("]", DIFF_PANE, diff_pane::NextThread, OutsideInputs),
    shortcut!("[", DIFF_PANE, diff_pane::PreviousThread, OutsideInputs),
    shortcut!("x", DIFF_PANE, diff_pane::ResolveThread, OutsideInputs),
    shortcut!("i", DIFF_PANE, diff_pane::ToggleComparison, OutsideInputs),
    shortcut!("a", DIFF_PANE, diff_pane::ToggleAnnotations, OutsideInputs),
    shortcut!("f", DIFF_PANE, diff_pane::ViewFile, OutsideInputs),
    // The history (PLAN §18.1): the shell's, from wherever the main area has the keyboard.
    shortcut!("alt-left", DIFF_PANE, diff_pane::GoBack, OutsideInputs),
    shortcut!("alt-right", DIFF_PANE, diff_pane::GoForward, OutsideInputs),
    // A text field copies its own selection.
    shortcut!("secondary-c", DIFF_PANE, diff_pane::Copy, OutsideInputs),
    // Code intelligence at the caret (PLAN §18.1).
    shortcut!(
        "secondary-i",
        DIFF_PANE,
        diff_pane::HoverAtCaret,
        OutsideInputs
    ),
    shortcut!("f12", DIFF_PANE, diff_pane::GoToDefinition, OutsideInputs),
    shortcut!(
        "shift-f12",
        DIFF_PANE,
        diff_pane::FindReferences,
        OutsideInputs
    ),
    shortcut!("secondary-f", DIFF_PANE, diff_pane::Find, OutsideInputs),
    // The find bar: its field searches on `Ctrl F` itself, so the row sits ahead of it (again,
    // the field's text selected); a single-line field lets `Enter`, `Shift Enter` and an idle
    // `Esc` through to the plain rows.
    shortcut!("secondary-f", FIND, diff_pane::Find, AheadOfInputs),
    shortcut!("enter", FIND, find::Next),
    shortcut!("shift-enter", FIND, find::Previous),
    shortcut!("escape", FIND, find::Close),
    // The composer's text field inserts a newline on `Ctrl Enter` itself (it is multi-line),
    // so posting sits ahead of it; it lets an idle `Esc` through to the plain row.
    shortcut!("secondary-enter", COMPOSER, composer::Post, AheadOfInputs),
    shortcut!("escape", COMPOSER, composer::Cancel),
    // The sheet: its verdicts have the keyboard until `Tab` moves it into the summary field,
    // where `Ctrl Enter` and `Tab` sit ahead of the field's own newline and indent.
    shortcut!("secondary-enter", SHEET, sheet::Submit),
    shortcut!("secondary-enter", SHEET, sheet::Submit, AheadOfInputs),
    shortcut!("escape", SHEET, sheet::Dismiss),
    shortcut!("down", SHEET, sheet::SelectNext, OutsideInputs),
    shortcut!("up", SHEET, sheet::SelectPrevious, OutsideInputs),
    shortcut!("1", SHEET, sheet::PickApprove, OutsideInputs),
    shortcut!("2", SHEET, sheet::PickRequestChanges, OutsideInputs),
    shortcut!("tab", SHEET, sheet::SwitchFocus, OutsideInputs),
    shortcut!("tab", SHEET, sheet::SwitchFocus, AheadOfInputs),
    shortcut!("shift-tab", SHEET, sheet::SwitchFocus, OutsideInputs),
    shortcut!("shift-tab", SHEET, sheet::SwitchFocus, AheadOfInputs),
    // Every text field: its copy and cut go through the clipboard seam, not gpui's clipboard
    // (dv's rows win over the field's own at the same depth).
    shortcut!("secondary-c", INPUT, field::Copy),
    // A selectable Markdown view's own copy writes the clipboard itself: dv's goes through the
    // seam (`crate::system`).
    shortcut!("secondary-c", TEXT_VIEW, field::Copy),
    shortcut!("secondary-x", INPUT, field::Cut),
];

/// The keys every Mac app has (DESIGN §5, "macOS"), bound on macOS only, anywhere in the window:
/// the app menu shows them (`crate::app_menu`).
pub const MACOS_SHORTCUTS: &[Shortcut] = &[
    shortcut!("cmd-q", SHELL, app::Quit),
    shortcut!("cmd-h", SHELL, app::Hide),
    shortcut!("alt-cmd-h", SHELL, app::HideOthers),
    shortcut!("cmd-m", SHELL, app::Minimize),
];

/// The rows this platform binds: [`SHORTCUTS`], and on macOS [`MACOS_SHORTCUTS`].
pub fn platform_shortcuts() -> impl Iterator<Item = &'static Shortcut> {
    let macos: &[Shortcut] = if cfg!(target_os = "macos") {
        MACOS_SHORTCUTS
    } else {
        &[]
    };
    SHORTCUTS.iter().chain(macos)
}

/// Binds [`platform_shortcuts`]. Call after `gpui_component::init`, so dv's rows are registered
/// last and win within a context depth.
pub fn init(cx: &mut App) -> anyhow::Result<()> {
    let bindings = platform_shortcuts()
        .map(|shortcut| shortcut.key_binding(cx))
        .collect::<anyhow::Result<Vec<_>>>()?;
    cx.bind_keys(bindings);
    Ok(())
}

/// The first row for `action` in `context`.
pub fn shortcut_for(action: &dyn Action, context: &str) -> Option<&'static Shortcut> {
    SHORTCUTS
        .iter()
        .find(|shortcut| shortcut.context == context && (shortcut.action)().partial_eq(action))
}

/// How keycaps spell modifiers (DESIGN §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keycaps {
    /// Windows and Linux: words, `Ctrl O`, `Ctrl Shift K`, `Alt ←`.
    Words,
    /// macOS: the symbols of its menus in its order (`⌃⌥⇧⌘`) against the key, `⌘O`, `⇧⌘K`,
    /// `⌥←`, `⌘↩`.
    Symbols,
}

impl Keycaps {
    /// This platform's.
    pub const NATIVE: Self = if cfg!(target_os = "macos") {
        Self::Symbols
    } else {
        Self::Words
    };
}

/// Keycap text for gpui keystroke syntax in this platform's spelling (DESIGN §5 writes `Ctrl O`,
/// `Ctrl Enter`; macOS `⌘O`, `⌘↩`): `secondary` is `Ctrl`, or `⌘` on macOS; arrows are drawn as
/// arrows; a sequence joins with spaces.
pub fn label(keys: &str) -> SharedString {
    label_as(keys, Keycaps::NATIVE)
}

/// [`label`] in the spelling `caps`.
pub fn label_as(keys: &str, caps: Keycaps) -> SharedString {
    keys.split_whitespace()
        .map(|stroke| keystroke_label(stroke, caps))
        .collect::<Vec<_>>()
        .join(" ")
        .into()
}

fn keystroke_label(stroke: &str, caps: Keycaps) -> String {
    let (modifiers, key) = match stroke.rsplit_once('-') {
        Some((modifiers, key)) if !key.is_empty() => (modifiers, key),
        _ => ("", stroke),
    };
    let modifiers = modifiers.split('-').filter(|modifier| !modifier.is_empty());
    let key = match key {
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "enter" if caps == Keycaps::Symbols => "↩".to_string(),
        "escape" => "Esc".to_string(),
        "pageup" => "PgUp".to_string(),
        "pagedown" => "PgDn".to_string(),
        other if other.chars().count() > 1 => capitalize(other),
        other => other.to_uppercase(),
    };
    match caps {
        Keycaps::Words => {
            let mut parts: Vec<String> = modifiers
                .map(|modifier| match modifier {
                    "secondary" | "ctrl" => "Ctrl".to_string(),
                    "cmd" | "platform" => "Cmd".to_string(),
                    "alt" => "Alt".to_string(),
                    "shift" => "Shift".to_string(),
                    other => capitalize(other),
                })
                .collect();
            parts.push(key);
            parts.join(" ")
        }
        Keycaps::Symbols => {
            // macOS's order, whatever order the keystroke names them in; a modifier without a
            // symbol (`fn`) spelled out ahead of them (`sort_by_key` puts `None` first).
            const ORDER: [&str; 4] = ["⌃", "⌥", "⇧", "⌘"];
            let mut symbols: Vec<String> = modifiers
                .map(|modifier| match modifier {
                    "ctrl" => "⌃".to_string(),
                    "alt" => "⌥".to_string(),
                    "shift" => "⇧".to_string(),
                    "secondary" | "cmd" | "platform" => "⌘".to_string(),
                    other => capitalize(other),
                })
                .collect();
            symbols.sort_by_key(|symbol| ORDER.iter().position(|it| it == symbol));
            symbols.concat() + &key
        }
    }
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[gpui::test]
    fn every_row_binds_and_no_keystroke_is_bound_twice_under_one_predicate(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut seen = HashSet::new();
        for shortcut in SHORTCUTS.iter().chain(MACOS_SHORTCUTS) {
            cx.update(|cx| shortcut.key_binding(cx))
                .unwrap_or_else(|error| panic!("{shortcut:?}: {error}"));
            assert!(
                seen.insert((shortcut.keys, shortcut.predicate())),
                "{shortcut:?} is bound twice"
            );
        }
    }

    #[test]
    fn labels_read_as_design_writes_them() {
        let words = |keys| label_as(keys, Keycaps::Words);
        assert_eq!(words("secondary-o"), "Ctrl O");
        assert_eq!(words("secondary-enter"), "Ctrl Enter");
        assert_eq!(words("shift-down"), "Shift ↓");
        assert_eq!(words("alt-left"), "Alt ←");
        assert_eq!(words("escape"), "Esc");
        assert_eq!(words("v j"), "V J");
        assert_eq!(words("secondary-,"), "Ctrl ,");

        let symbols = |keys| label_as(keys, Keycaps::Symbols);
        assert_eq!(symbols("secondary-o"), "⌘O");
        assert_eq!(symbols("secondary-enter"), "⌘↩");
        assert_eq!(symbols("secondary-shift-k"), "⇧⌘K");
        assert_eq!(symbols("shift-down"), "⇧↓");
        assert_eq!(symbols("alt-left"), "⌥←");
        assert_eq!(symbols("ctrl-alt-shift-cmd-x"), "⌃⌥⇧⌘X");
        assert_eq!(symbols("cmd-shift-alt-ctrl-x"), "⌃⌥⇧⌘X");
        // A modifier with no symbol (gpui's `fn`) is spelled out ahead of the symbols.
        assert_eq!(symbols("shift-fn-x"), "Fn⇧X");
        assert_eq!(symbols("escape"), "Esc");
        assert_eq!(symbols("v j"), "V J");
        assert_eq!(symbols("secondary-,"), "⌘,");

        let native = if cfg!(target_os = "macos") {
            "⌘O"
        } else {
            "Ctrl O"
        };
        assert_eq!(
            shortcut_for(&shell::ToggleSwitcher, SHELL).map(Shortcut::label),
            Some(native.into())
        );
    }

    #[test]
    fn ctrl_enter_posts_ahead_of_the_composer_field_and_finishes_only_outside_fields() {
        let post = shortcut_for(&composer::Post, COMPOSER).unwrap();
        assert_eq!(post.predicate(), "Composer > Input");
        let finish = shortcut_for(&shell::FinishReview, SHELL).unwrap();
        assert_eq!(finish.predicate(), "Shell && !Input");
        let submits: Vec<String> = SHORTCUTS
            .iter()
            .filter(|shortcut| (shortcut.action)().partial_eq(&sheet::Submit))
            .map(Shortcut::predicate)
            .collect();
        assert_eq!(submits, ["Sheet", "Sheet > Input"]);
    }

    #[test]
    fn list_keys_sit_ahead_of_the_filter_field() {
        let down = shortcut_for(&switcher::SelectNext, SWITCHER).unwrap();
        assert_eq!(down.predicate(), "Switcher > Input");
        let enter = shortcut_for(&switcher::Confirm, SWITCHER).unwrap();
        assert_eq!(enter.predicate(), "Switcher");
        // `Ctrl F` in the find field is dv's, not the field's own search.
        let find_again = shortcut_for(&diff_pane::Find, FIND).unwrap();
        assert_eq!(find_again.predicate(), "Find > Input");
        let next = shortcut_for(&find::Next, FIND).unwrap();
        assert_eq!(next.predicate(), "Find");
    }
}
