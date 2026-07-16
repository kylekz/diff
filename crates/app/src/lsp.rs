//! Go-to-definition (S8f) and hover (S8g) client-side model
//! (docs/phase-8-lsp-and-polish.md § LSP). The pure data types that don't
//! need field access into `Workspace` live here; the gpui wiring — the
//! honest-view gate (needs `self.source`), click/hover hit-testing, the
//! lazy per-workspace `LspHandle` spawn, and the read-only target-viewer +
//! hover-popover renders — lives in `workspace.rs` alongside the rest of
//! `Workspace`'s private state (this module has no `Workspace` field
//! access, by construction).
//!
//! **Lazy, per-workspace spawn.** `Workspace::lsp_session` is spawned at
//! most once per `Workspace` entity — never eagerly at repo-open, only on
//! the first go-to-def attempt against an honest WSL TypeScript view (see
//! `dv_core::lsp`'s module doc for why: a passive spawn would be exactly
//! the boot-storm this whole spine has spent five phases eliminating).
//! `Workspace`'s own field holds the only long-lived [`dv_core::lsp::LspHandle`]
//! clone, so dropping the `Workspace` (repo close, app exit, or LRU
//! eviction — Phase 7 S7-3) drops the last `Arc` and runs `LspClient`'s
//! `Drop`, which kills the child.

/// A place [`NavStack`] can point at: a file URI plus a zero-based
/// line/character position — LSP's own coordinate system, never converted
/// to dv's 1-based diff-line numbering (which only ever applies to the
/// DIFF pane, not the read-only target viewer this drives).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Location {
    pub(crate) uri: String,
    pub(crate) line: u32,
    pub(crate) character: u32,
}

impl Location {
    pub(crate) fn from_link(link: &lsp_types::LocationLink) -> Self {
        Location {
            uri: link.target_uri.as_str().to_string(),
            line: link.target_selection_range.start.line,
            character: link.target_selection_range.start.character,
        }
    }
}

/// Back/forward history over target-viewer jumps (docs/phase-8-lsp-and-polish.md
/// § LSP: "back/forward history"). Distinct from `JumpToFile`/`current_hunk`,
/// which only ever navigate within the diff's own changed-file list — this
/// one follows go-to-definition jumps, including to files entirely outside
/// the diff.
#[derive(Debug, Clone, Default)]
pub(crate) struct NavStack {
    back: Vec<Location>,
    forward: Vec<Location>,
}

impl NavStack {
    /// Record `from` (the place a go-to-def jump is leaving) and clear
    /// `forward` — a fresh jump invalidates whatever "forward" meant
    /// relative to the old position, same as ordinary browser history.
    pub(crate) fn push(&mut self, from: Location) {
        self.back.push(from);
        self.forward.clear();
    }

    /// Step back one entry: `current` (what's on screen right now) is
    /// filed onto the forward stack so [`Self::go_forward`] can return to
    /// it, and the popped back-entry is handed back as the new "go show
    /// this" target. `None` when there's nowhere to go.
    pub(crate) fn go_back(&mut self, current: Location) -> Option<Location> {
        let target = self.back.pop()?;
        self.forward.push(current);
        Some(target)
    }

    pub(crate) fn go_forward(&mut self, current: Location) -> Option<Location> {
        let target = self.forward.pop()?;
        self.back.push(current);
        Some(target)
    }

    /// Read what [`Self::go_back`] *would* return, without mutating either
    /// stack. `workspace.rs`'s `on_nav_back` uses this to know whether
    /// there's somewhere to go BEFORE kicking off the target viewer's async
    /// load — the actual `go_back` mutation is deferred until that load
    /// succeeds (P3 finding: committing the stack eagerly desynced history
    /// from a target read that then failed).
    pub(crate) fn peek_back(&self) -> Option<Location> {
        self.back.last().cloned()
    }

    pub(crate) fn peek_forward(&self) -> Option<Location> {
        self.forward.last().cloned()
    }

    pub(crate) fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub(crate) fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

/// What `Workspace::open_target_at` should do to a `NavStack` once (and
/// only if) the target viewer's async load actually succeeds — see that
/// method's doc comment. Carries the value each `NavStack` mutation needs
/// (the jump's origin for a fresh `Push`, or the viewer's pre-jump location
/// for `Back`/`Forward`, which `NavStack::go_back`/`go_forward` file onto
/// the opposite stack).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NavCommit {
    Push(Location),
    Back(Location),
    Forward(Location),
}

/// The S8g hover popover, while shown — anchored to the WINDOW-relative
/// point the triggering mouse move landed at (`workspace.rs`'s
/// `render_hover_popover` converts this to a position relative to the
/// workspace root at render time, since that's the nearest positioned
/// ancestor an `.absolute()` overlay child resolves against — see that
/// method's doc comment). `line` is the 1-based diff line this popover was
/// raised on, carried purely so `Workspace::on_symbol_hover_leave` can tell
/// "the row I'm leaving still owns the shown popover" apart from "a fresher
/// hover (on a different row) has already replaced it" — order-independent
/// against the two racing sources of truth (this row's own mouse-exit vs.
/// another row's mouse-move), unlike clearing unconditionally on any exit.
#[derive(Debug, Clone)]
pub(crate) struct HoverPopover {
    pub(crate) anchor: gpui::Point<gpui::Pixels>,
    pub(crate) line: u32,
    pub(crate) markdown: String,
}

/// `Workspace::lsp_session`'s persistent state — spawned at most once per
/// workspace (see the module doc). Distinguished from the PER-CLICK
/// availability a symbol click actually renders (honest-view can flip
/// independently of this): a `Ready` session still declines to answer a
/// click on a non-honest view (see `workspace.rs`'s `lsp_view_is_honest`).
pub(crate) enum LspSessionState {
    /// No attempt made yet — the common case for every workspace that
    /// never sees a ctrl/cmd-click.
    Unattempted,
    /// The lazy background spawn is in flight.
    Spawning,
    Ready(dv_core::lsp::LspHandle),
    /// The lazy spawn's detection step found no usable vtsls (missing, or
    /// node/vtsls detection itself failed), or the spawn/handshake round
    /// trip failed outright. Carries the human-readable reason for
    /// `automation_state`'s dump. The one call site that installs this
    /// (`Workspace::on_symbol_click`'s spawn completion) is only ever
    /// reached after the WSL/TS/honest-view gates earlier in that fn have
    /// already passed, so nothing captured here is a structural, permanent
    /// block — it can change (most commonly: a consent-triggered vtsls
    /// install completing after this state was set). Not retried on its
    /// own, but a fresh ctrl/cmd-click resets it back to `Unattempted`
    /// before re-checking (see that fn's preamble) rather than trusting a
    /// stale verdict forever (P2 finding, phase-8 capstone review).
    Unavailable(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(n: u32) -> Location {
        Location {
            uri: format!("file:///f{n}.ts"),
            line: n,
            character: 0,
        }
    }

    #[test]
    fn nav_stack_starts_empty() {
        let stack = NavStack::default();
        assert!(!stack.can_go_back());
        assert!(!stack.can_go_forward());
    }

    #[test]
    fn push_then_go_back_and_forward_round_trips() {
        let mut stack = NavStack::default();
        stack.push(loc(1));
        assert!(stack.can_go_back());
        assert!(!stack.can_go_forward());

        let back_to = stack.go_back(loc(2)).expect("back entry exists");
        assert_eq!(back_to, loc(1));
        assert!(!stack.can_go_back());
        assert!(stack.can_go_forward());

        let forward_to = stack.go_forward(loc(1)).expect("forward entry exists");
        assert_eq!(forward_to, loc(2));
        assert!(stack.can_go_back());
        assert!(!stack.can_go_forward());
    }

    #[test]
    fn a_fresh_jump_clears_the_forward_stack() {
        let mut stack = NavStack::default();
        stack.push(loc(1));
        stack.go_back(loc(2));
        assert!(stack.can_go_forward());

        // A new jump from wherever we are now must invalidate "forward" —
        // same as a browser: navigating fresh after going back drops the
        // old forward history.
        stack.push(loc(3));
        assert!(!stack.can_go_forward());
        assert!(stack.can_go_back());
    }

    #[test]
    fn go_back_and_forward_are_none_when_empty() {
        let mut stack = NavStack::default();
        assert!(stack.go_back(loc(1)).is_none());
        assert!(stack.go_forward(loc(1)).is_none());
    }

    #[test]
    fn peek_back_does_not_mutate_either_stack() {
        let mut stack = NavStack::default();
        stack.push(loc(1));
        assert_eq!(stack.peek_back(), Some(loc(1)));
        // Peeking twice returns the same answer — no mutation happened.
        assert_eq!(stack.peek_back(), Some(loc(1)));
        assert!(!stack.can_go_forward());
        assert!(stack.can_go_back());
    }

    #[test]
    fn peek_forward_does_not_mutate_either_stack() {
        let mut stack = NavStack::default();
        stack.push(loc(1));
        stack.go_back(loc(2));
        assert_eq!(stack.peek_forward(), Some(loc(2)));
        // Peeking twice returns the same answer — no mutation happened.
        assert_eq!(stack.peek_forward(), Some(loc(2)));
        assert!(!stack.can_go_back());
    }

    #[test]
    fn peek_is_none_when_empty() {
        let stack = NavStack::default();
        assert!(stack.peek_back().is_none());
        assert!(stack.peek_forward().is_none());
    }

    #[test]
    fn location_from_link_uses_the_target_selection_range_start() {
        let link = lsp_types::LocationLink {
            origin_selection_range: None,
            target_uri: "file:///a.ts".parse().unwrap(),
            target_range: lsp_types::Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(10, 0),
            ),
            target_selection_range: lsp_types::Range::new(
                lsp_types::Position::new(4, 2),
                lsp_types::Position::new(4, 8),
            ),
        };
        let location = Location::from_link(&link);
        assert_eq!(location.uri, "file:///a.ts");
        assert_eq!(location.line, 4);
        assert_eq!(location.character, 2);
    }
}
