//! Pure ranking/mixing logic for the ctrl-k command palette
//! (docs/backlog.md: "one fuzzy surface over commands AND destinations").
//!
//! Deliberately dependency-free (no gpui) so the group-ordering/filtering
//! rules are unit-testable headless, same split as [`crate::fuzzy`] — the
//! gpui-facing half (the input entity, keybindings, actually dispatching a
//! chosen command) lives on `AppShell` in `shell.rs`, which gathers the
//! candidate lists from live app state (the action registry, the Phase-6
//! review index, the active workspace's file list, the PR-picker's cached
//! list) and hands them to [`rank_and_mix`] here.

/// Which of the four palette groups a candidate belongs to. Order here is
/// the task's own numbered list (Commands, Reviews, Files, PRs) and is what
/// [`rank_and_mix`] renders in, group by group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteKind {
    Command,
    Review,
    File,
    Pr,
}

/// One row's worth of palette content, already resolved from live app
/// state by the caller (shell.rs) — this module never reaches back into
/// gpui or dv-core itself.
#[derive(Debug, Clone, PartialEq)]
pub struct PaletteCandidate {
    pub kind: PaletteKind,
    /// What activating this row does, interpreted by `kind`: an action
    /// name ("workspace::ToggleSplit"), a review id, a file index (as a
    /// string), or a PR number (as a string).
    pub id: String,
    /// Primary row text.
    pub label: String,
    /// Secondary row text (keybinding hint / repo+age / file status /
    /// PR author) — empty string if there's nothing to show.
    pub subtitle: String,
    /// What [`crate::fuzzy::rank`] matches the query against — usually
    /// `label` plus any extra searchable text (a review's title, a PR's
    /// author) that isn't itself displayed in the condensed row.
    pub search_text: String,
}

/// Per-group cap on an empty query, so the default view is "a sensible
/// default" (task's own words) rather than the whole index/file list
/// dumped onto the screen.
const DEFAULT_LIMIT_PER_GROUP: usize = 6;
/// Per-group cap once there's a real query — generous, since fuzzy
/// matching already did the real narrowing.
const QUERY_LIMIT_PER_GROUP: usize = 20;

/// Prefix filters (task: "nice touch if cheap, implement only if it stays
/// simple"): `>` restricts to Commands, `#` to PRs — chosen to match the
/// mnemonics VS Code's own command palette and GitHub's PR-number shorthand
/// already trained users on. Returns the restriction (if any) and the
/// query text with the prefix stripped.
pub fn parse_prefix(query: &str) -> (Option<PaletteKind>, &str) {
    if let Some(rest) = query.strip_prefix('>') {
        (Some(PaletteKind::Command), rest)
    } else if let Some(rest) = query.strip_prefix('#') {
        (Some(PaletteKind::Pr), rest)
    } else {
        (None, query)
    }
}

/// Rank and mix `candidates` for `query`: fuzzy-filters each group
/// independently (so a file named "review.rs" can't crowd real Review-group
/// hits out of the results) using [`crate::fuzzy::rank`], then concatenates
/// groups in the fixed Commands/Reviews/Files/PRs order. An empty (post-
/// prefix-strip) query keeps every candidate (score 0, `fuzzy::rank`'s own
/// documented empty-query behavior) in original order, capped to
/// [`DEFAULT_LIMIT_PER_GROUP`] per group instead of the query cap — the
/// caller is expected to have already ordered each group so the first few
/// entries are the ones worth defaulting to (recency for reviews, a small
/// curated priority for commands — see [`command_priority`] — natural
/// order for files/PRs).
pub fn rank_and_mix<'a>(
    query: &str,
    candidates: &'a [PaletteCandidate],
) -> Vec<&'a PaletteCandidate> {
    let (restrict, query) = parse_prefix(query);
    let query = query.trim();
    let limit = if query.is_empty() {
        DEFAULT_LIMIT_PER_GROUP
    } else {
        QUERY_LIMIT_PER_GROUP
    };

    [
        PaletteKind::Command,
        PaletteKind::Review,
        PaletteKind::File,
        PaletteKind::Pr,
    ]
    .into_iter()
    .filter(|kind| restrict.is_none_or(|r| r == *kind))
    .flat_map(|kind| {
        let group: Vec<&PaletteCandidate> = candidates.iter().filter(|c| c.kind == kind).collect();
        crate::fuzzy::rank(query, group.iter().map(|c| c.search_text.as_str()))
            .into_iter()
            .take(limit)
            .map(move |i| group[i])
    })
    .collect()
}

/// Action namespaces eligible for the Commands group at all. `menu::` (mac
/// native-menu-only `About`/`Quit`, S8i) is deliberately excluded: those two
/// actions only ever get a live `cx.on_action` handler under
/// `#[cfg(target_os = "macos")]` (see `menu.rs`'s module doc), so listing
/// them here would show a dead row on Windows/Linux — the platforms this
/// was actually built and clippy-checked on.
const ALLOWED_NAMESPACES: &[&str] = &["shell", "workspace"];

/// Actions that exist in the registry (so `dv --automation`'s `actions`
/// command and keybindings both reach them fine) but don't make sense as a
/// *standalone* palette row — kept small and grouped by why, per the task's
/// own instruction:
///
/// - The next/prev/choose/close quartet for every OTHER overlay (jump-to-
///   file palette, PR picker, theme picker, the S10 find-references
///   panel) plus this palette's own —
///   these only make sense while that specific overlay already has input
///   focus and its own keybindings scoped to it; invoking e.g.
///   "PrPickerNext" with no PR picker open does nothing.
/// - Context-menu-scoped actions (`ToggleArchiveReview`, `DeleteReview*`,
///   `NewReviewForRepo`, `RemoveRepoFromSidebar`): these read
///   `AppShell::menu_review`/`delete_confirm`/`menu_repo`, populated only
///   by a card's/row's right-click, so invoked from the palette they'd
///   silently no-op — or worse: the `menu_repo` stash is only consumed by
///   a chosen menu item, so after a dismissed right-click a palette
///   invocation would act on that stale repo with zero visible connection
///   to the gesture (post-hoc review P1).
/// - Selection/edit-scoped workspace actions (`ClearSelection`,
///   `CancelComment`): only meaningful mid-selection/mid-edit, same reason.
/// - `OpenCommandPalette` itself: redundant while already inside it.
const EXCLUDED_ACTIONS: &[&str] = &[
    "PaletteNext",
    "PalettePrev",
    "PaletteClose",
    "PaletteChoose",
    "PrPickerNext",
    "PrPickerPrev",
    "PrPickerClose",
    "PrPickerChoose",
    "ThemePickerNext",
    "ThemePickerPrev",
    "ThemePickerClose",
    "ThemePickerChoose",
    "CommandPaletteNext",
    "CommandPalettePrev",
    "CommandPaletteClose",
    "CommandPaletteChoose",
    "ReferencesNext",
    "ReferencesPrev",
    "ReferencesChoose",
    "ReferencesClose",
    "OpenCommandPalette",
    "SettingsClose",
    "OnboardingClose",
    "CloseTargetViewer",
    "ToggleArchiveReview",
    "DeleteReviewPrompt",
    "DeleteReviewConfirm",
    "DeleteReviewCancel",
    "NewReviewForRepo",
    "RemoveRepoFromSidebar",
    "ClearSelection",
    "CancelComment",
];

/// Whether `full_name` (as `cx.all_action_names()` returns it, e.g.
/// `"workspace::ToggleSplit"`) belongs in the Commands group — the palette
/// reuses the SAME registry automation's `actions` command enumerates
/// rather than a hand-copied list; this is only a narrow, documented
/// allow/deny filter on top of it (see [`ALLOWED_NAMESPACES`]/
/// [`EXCLUDED_ACTIONS`]'s own doc comments), so a renamed or removed action
/// falls out automatically instead of leaving a stale dead entry.
pub fn is_palette_command(full_name: &str) -> bool {
    let Some((namespace, short)) = full_name.split_once("::") else {
        return false;
    };
    ALLOWED_NAMESPACES.contains(&namespace) && !EXCLUDED_ACTIONS.contains(&short)
}

/// A small curated ordering for the empty-query default view (task: "show a
/// sensible default... rather than nothing" — for commands specifically,
/// "a few common commands"). Purely a *display-order* hint on top of the
/// live registry, not a second source of truth for existence: an entry
/// here that no longer exists in `cx.all_action_names()` is simply never
/// looked up (see `AppShell::command_palette_candidates`), so a rename
/// doesn't leave a dangling reference. Anything not listed sorts after
/// (alphabetically, by the caller), which only matters for the empty-query
/// cap — a real query re-ranks by fuzzy score regardless of this order.
const COMMON_COMMAND_PRIORITY: &[&str] = &[
    "ToggleSplit",
    "JumpToFile",
    "OpenPrPicker",
    "NewReview",
    "OpenThemePicker",
    "OpenSettings",
    "ToggleSidebar",
    "ToggleSummary",
];

/// Sort key for a short action name: its index in [`COMMON_COMMAND_PRIORITY`]
/// if present, else `usize::MAX` (sorts last).
pub fn command_priority(short_name: &str) -> usize {
    COMMON_COMMAND_PRIORITY
        .iter()
        .position(|&n| n == short_name)
        .unwrap_or(usize::MAX)
}

/// Turns a PascalCase action short name into a human label, with a small
/// override table for names that read better with a word the identifier
/// itself doesn't spell out (task's own example: "ToggleSplit" ->
/// "Toggle Split View"). Anything not overridden falls back to a generic
/// "insert a space before every interior capital" split — good enough for
/// the common `VerbNoun` shape every action here already follows, and,
/// crucially, still produces SOME reasonable label for a future action
/// nobody remembered to add to the table.
pub fn humanize_action_name(short: &str) -> String {
    match short {
        "ToggleSplit" => "Toggle Split View".to_string(),
        "ToggleSummary" => "Toggle Summary Panel".to_string(),
        "ToggleSidebar" => "Toggle Sidebar".to_string(),
        "JumpToFile" => "Jump to File".to_string(),
        "OpenPrPicker" => "Open PR Picker".to_string(),
        "RefreshPr" => "Refresh PR Status".to_string(),
        "NavBack" => "Go Back".to_string(),
        "NavForward" => "Go Forward".to_string(),
        "NextFile" => "Next Changed File".to_string(),
        "PrevFile" => "Previous Changed File".to_string(),
        "NextHunk" => "Next Hunk".to_string(),
        "PrevHunk" => "Previous Hunk".to_string(),
        "NewReview" => "New Review".to_string(),
        "RefreshBadges" => "Refresh Review Badges".to_string(),
        "OpenThemePicker" => "Change Theme\u{2026}".to_string(),
        "OpenSettings" => "Open Settings".to_string(),
        "OpenOnboarding" => "Open Setup Page".to_string(),
        _ => split_pascal_case(short),
    }
}

fn split_pascal_case(short: &str) -> String {
    let mut out = String::with_capacity(short.len() + 4);
    for (i, ch) in short.chars().enumerate() {
        if i > 0 && ch.is_uppercase() {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(id: &str, label: &str) -> PaletteCandidate {
        PaletteCandidate {
            kind: PaletteKind::Command,
            id: id.to_string(),
            label: label.to_string(),
            subtitle: String::new(),
            search_text: label.to_string(),
        }
    }

    fn review(id: &str, label: &str) -> PaletteCandidate {
        PaletteCandidate {
            kind: PaletteKind::Review,
            id: id.to_string(),
            label: label.to_string(),
            subtitle: String::new(),
            search_text: label.to_string(),
        }
    }

    fn file(id: &str, label: &str) -> PaletteCandidate {
        PaletteCandidate {
            kind: PaletteKind::File,
            id: id.to_string(),
            label: label.to_string(),
            subtitle: String::new(),
            search_text: label.to_string(),
        }
    }

    fn pr(id: &str, label: &str) -> PaletteCandidate {
        PaletteCandidate {
            kind: PaletteKind::Pr,
            id: id.to_string(),
            label: label.to_string(),
            subtitle: String::new(),
            search_text: label.to_string(),
        }
    }

    #[test]
    fn empty_query_keeps_group_order_and_caps_each_group() {
        let candidates: Vec<PaletteCandidate> = (0..10)
            .map(|i| file(&i.to_string(), &format!("file{i}")))
            .collect();
        let ranked = rank_and_mix("", &candidates);
        assert_eq!(ranked.len(), DEFAULT_LIMIT_PER_GROUP);
        assert_eq!(ranked[0].label, "file0");
        assert_eq!(ranked[5].label, "file5");
    }

    #[test]
    fn groups_render_in_fixed_order_regardless_of_input_order() {
        let candidates = vec![
            pr("1", "pr one"),
            file("f", "shared"),
            review("r", "shared"),
            cmd("c", "shared"),
        ];
        let ranked = rank_and_mix("shared", &candidates);
        let kinds: Vec<PaletteKind> = ranked.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![PaletteKind::Command, PaletteKind::Review, PaletteKind::File]
        );
    }

    #[test]
    fn query_filters_within_each_group_independently() {
        let candidates = vec![
            cmd("split", "Toggle Split View"),
            cmd("sidebar", "Toggle Sidebar"),
            file("f1", "workspace.rs"),
        ];
        let ranked = rank_and_mix("split", &candidates);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].id, "split");
    }

    #[test]
    fn a_file_named_review_does_not_crowd_out_the_review_group() {
        let candidates = vec![file("f1", "review.rs"), review("r1", "review title")];
        let ranked = rank_and_mix("review", &candidates);
        assert_eq!(ranked.len(), 2);
        // Fixed order is Commands, Reviews, Files, PRs — Review group first.
        assert_eq!(ranked[0].kind, PaletteKind::Review);
        assert_eq!(ranked[1].kind, PaletteKind::File);
    }

    #[test]
    fn prefix_gt_restricts_to_commands_only() {
        let candidates = vec![cmd("c1", "split view"), file("f1", "split.rs")];
        let ranked = rank_and_mix(">split", &candidates);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].kind, PaletteKind::Command);
    }

    #[test]
    fn prefix_hash_restricts_to_prs_only() {
        let candidates = vec![cmd("c1", "123 issue"), pr("42", "123 fix thing")];
        let ranked = rank_and_mix("#123", &candidates);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].kind, PaletteKind::Pr);
    }

    #[test]
    fn parse_prefix_strips_and_reports_restriction() {
        assert_eq!(parse_prefix(">foo"), (Some(PaletteKind::Command), "foo"));
        assert_eq!(parse_prefix("#12"), (Some(PaletteKind::Pr), "12"));
        assert_eq!(parse_prefix("plain"), (None, "plain"));
    }

    #[test]
    fn is_palette_command_allows_shell_and_workspace_only() {
        assert!(is_palette_command("shell::NewReview"));
        assert!(is_palette_command("workspace::ToggleSplit"));
        assert!(!is_palette_command("menu::About"));
        assert!(!is_palette_command("input::Undo"));
        assert!(!is_palette_command("no-namespace-here"));
    }

    #[test]
    fn is_palette_command_excludes_contextual_actions() {
        assert!(!is_palette_command("workspace::ClearSelection"));
        assert!(!is_palette_command("shell::ToggleArchiveReview"));
        assert!(!is_palette_command("workspace::PrPickerNext"));
        assert!(!is_palette_command("shell::OpenCommandPalette"));
    }

    #[test]
    fn command_priority_orders_curated_names_first() {
        assert!(command_priority("ToggleSplit") < command_priority("ToggleSummary"));
        assert_eq!(command_priority("SomeUncuratedAction"), usize::MAX);
    }

    #[test]
    fn humanize_uses_override_table_then_falls_back() {
        assert_eq!(humanize_action_name("ToggleSplit"), "Toggle Split View");
        assert_eq!(humanize_action_name("NextFile"), "Next Changed File");
        assert_eq!(
            humanize_action_name("SomeUncuratedAction"),
            "Some Uncurated Action"
        );
    }
}
