//! The palette's commands (DESIGN §4.1 "Palette"): the shell's and the stream's actions worth
//! finding by name, each run as its key would run it. The stream's run in the main area, which
//! gets the keyboard back first; they are offered only while a review is open, and a pull
//! request's only while a PR review is ([`Scope`]).

use gpui::{Action, SharedString};

use crate::actions::{diff_pane, shell};
use crate::keymap::{self, DIFF_PANE, SHELL};

/// What must be open for a command to be listed, and what is open in the palette's window:
/// a command lists when what it needs is no more than what is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Nothing: the shell's own commands.
    Always,
    /// A review.
    Review,
    /// A PR review.
    PrReview,
}

/// A command the palette lists.
#[derive(Clone, Copy)]
pub struct Command {
    /// What its row reads.
    pub title: &'static str,
    /// Other words it is found by, beside its title's.
    pub words: &'static str,
    pub action: fn() -> Box<dyn Action>,
    /// The key context it runs in: [`SHELL`] (anywhere) or [`DIFF_PANE`] (the main area).
    pub context: &'static str,
    /// What must be open for it to be listed.
    pub needs: Scope,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("title", &self.title)
            .field("action", &(self.action)().name())
            .finish()
    }
}

impl PartialEq for Command {
    fn eq(&self, other: &Self) -> bool {
        self.title == other.title && (self.action)().partial_eq(&*(other.action)())
    }
}

impl Eq for Command {}

impl Command {
    /// The action's name, `diff_pane::NextFile`: the row's key.
    pub fn name(&self) -> &'static str {
        (self.action)().name()
    }

    /// Its keycap, from the keymap row that runs it in its context.
    pub fn keys(&self) -> Option<SharedString> {
        keymap::shortcut_for(&*(self.action)(), self.context).map(keymap::Shortcut::label)
    }

    /// Its title and words, lowercased, for filtering.
    pub fn text(&self) -> String {
        format!("{} {}", self.title, self.words).to_lowercase()
    }
}

macro_rules! command {
    ($title:literal, $words:literal, $action:expr, $context:expr, $needs:ident) => {
        Command {
            title: $title,
            words: $words,
            action: || Box::new($action),
            context: $context,
            needs: Scope::$needs,
        }
    };
}

/// Every command, in the order an empty palette lists them.
pub const COMMANDS: &[Command] = &[
    command!("Go to file", "find open", shell::FindFile, SHELL, Review),
    command!(
        "Switch repository or review",
        "open switcher",
        shell::ToggleSwitcher,
        SHELL,
        Always
    ),
    command!(
        "Open folder…",
        "add repository",
        shell::OpenFolder,
        SHELL,
        Always
    ),
    command!(
        "Open a pull request…",
        "pr github picker review",
        shell::TogglePrPicker,
        SHELL,
        Always
    ),
    command!(
        "Refresh pull request",
        "pr github fetch reload",
        shell::RefreshPr,
        SHELL,
        PrReview
    ),
    command!(
        "Finish round",
        "approve request changes submit review",
        shell::FinishReview,
        SHELL,
        Review
    ),
    command!("Next file", "", diff_pane::NextFile, DIFF_PANE, Review),
    command!(
        "Previous file",
        "",
        diff_pane::PreviousFile,
        DIFF_PANE,
        Review
    ),
    command!(
        "Next change",
        "hunk",
        diff_pane::NextChange,
        DIFF_PANE,
        Review
    ),
    command!(
        "Previous change",
        "hunk",
        diff_pane::PreviousChange,
        DIFF_PANE,
        Review
    ),
    command!(
        "Next unresolved thread",
        "comment",
        diff_pane::NextThread,
        DIFF_PANE,
        Review
    ),
    command!(
        "Previous unresolved thread",
        "comment",
        diff_pane::PreviousThread,
        DIFF_PANE,
        Review
    ),
    command!(
        "Comment on the file",
        "thread",
        diff_pane::CommentOnFile,
        DIFF_PANE,
        Review
    ),
    command!(
        "Mark file viewed",
        "unviewed",
        diff_pane::ToggleViewed,
        DIFF_PANE,
        Review
    ),
    command!(
        "Split or unified view",
        "toggle diff layout",
        diff_pane::ToggleSplit,
        DIFF_PANE,
        Review
    ),
    command!(
        "Since last round or all changes",
        "interdiff comparison toggle",
        diff_pane::ToggleComparison,
        DIFF_PANE,
        Review
    ),
    command!(
        "Show or hide annotations",
        "toggle agent",
        diff_pane::ToggleAnnotations,
        DIFF_PANE,
        Review
    ),
    command!(
        "Expand the gap",
        "context lines",
        diff_pane::ExpandGap,
        DIFF_PANE,
        Review
    ),
    command!(
        "Find in the review",
        "search",
        diff_pane::Find,
        DIFF_PANE,
        Review
    ),
    command!(
        "View the whole file",
        "file view open",
        diff_pane::ViewFile,
        DIFF_PANE,
        Review
    ),
    command!("Go back", "history", diff_pane::GoBack, DIFF_PANE, Review),
    command!(
        "Go forward",
        "history",
        diff_pane::GoForward,
        DIFF_PANE,
        Review
    ),
    command!(
        "Go to definition",
        "symbol",
        diff_pane::GoToDefinition,
        DIFF_PANE,
        Review
    ),
    command!(
        "Find references",
        "symbol usages",
        diff_pane::FindReferences,
        DIFF_PANE,
        Review
    ),
    command!(
        "Show or hide the panel",
        "toggle sidebar files comments",
        shell::TogglePanel,
        SHELL,
        Always
    ),
    command!(
        "Settings",
        "appearance theme mode material dark light setup preferences",
        shell::ToggleSettings,
        SHELL,
        Always
    ),
];
