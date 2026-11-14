//! dv's icons (DESIGN §1.6): the few Lucide glyphs the views use, the Windows caption glyphs,
//! the model families' marks and dv's mark, embedded from `assets/icons/` (its README names each file's source and licence).
//!
//! gpui paints `svg()` as a mask tinted with the text colour, so every icon is single-colour; an
//! SVG's own `opacity` shades the mask (the mark's dimmed half). Draw one with
//! `gpui::svg().path(icon.path())`; [`crate::Assets`] serves the bytes.

macro_rules! dv_icons {
    ($($variant:ident => $file:literal,)*) => {
        /// An icon in `assets/icons/`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum DvIcon {
            $($variant,)*
        }

        impl DvIcon {
            /// Every icon, in declaration order.
            pub const ALL: &[DvIcon] = &[$(DvIcon::$variant,)*];

            /// The asset path, e.g. `icons/folder.svg`.
            pub const fn path(self) -> &'static str {
                match self {
                    $(DvIcon::$variant => concat!("icons/", $file, ".svg"),)*
                }
            }
        }

        /// Every icon file as `(asset path, bytes)`, for [`crate::Assets`].
        pub(crate) const FILES: &[(&str, &[u8])] = &[
            $((
                concat!("icons/", $file, ".svg"),
                include_bytes!(concat!("../../../assets/icons/", $file, ".svg")).as_slice(),
            ),)*
        ];
    };
}

dv_icons! {
    // dv's mark: two panes, the old side dimmed (title bar, DESIGN §4.1).
    Mark => "dv-mark",
    // Windows caption buttons (10 grid, 1px stroke).
    WindowMinimize => "window-minimize",
    WindowMaximize => "window-maximize",
    WindowRestore => "window-restore",
    WindowClose => "window-close",
    // Lucide.
    ArrowDown => "arrow-down",
    ArrowLeftRight => "arrow-left-right",
    ArrowUp => "arrow-up",
    Binary => "binary",
    Bot => "bot",
    Check => "check",
    ChevronDown => "chevron-down",
    ChevronRight => "chevron-right",
    ChevronsUpDown => "chevrons-up-down",
    CircleCheck => "circle-check",
    CircleMinus => "circle-minus",
    // A setup check's states (DESIGN §4.3): missing, failed.
    Circle => "circle",
    CircleX => "circle-x",
    Columns => "columns-2",
    Copy => "copy",
    Ellipsis => "ellipsis",
    File => "file",
    FileDiff => "file-diff",
    Folder => "folder",
    FolderOpen => "folder-open",
    GitBranch => "git-branch",
    GitCommit => "git-commit-horizontal",
    ListFilter => "list-filter",
    MessageSquare => "message-square",
    MessageSquarePlus => "message-square-plus",
    // This machine among the setup screen's environments (DESIGN §4.3).
    Monitor => "monitor",
    Package => "package",
    Play => "play",
    Plus => "plus",
    RotateCw => "rotate-cw",
    Rows => "rows-2",
    Search => "search",
    Terminal => "square-terminal",
    TriangleAlert => "triangle-alert",
    X => "x",
    // The model families' marks, beside what an agent wrote (`crate::agents`).
    ModelClaude => "model-claude",
    ModelDeepSeek => "model-deepseek",
    ModelGemini => "model-gemini",
    ModelKimi => "model-kimi",
    ModelOpenAi => "model-openai",
    ModelQwen => "model-qwen",
    ModelZai => "model-zai",
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_an_embedded_svg() {
        assert_eq!(DvIcon::ALL.len(), FILES.len());
        for (icon, (path, bytes)) in DvIcon::ALL.iter().zip(FILES) {
            assert_eq!(icon.path(), *path);
            let text = std::str::from_utf8(bytes).expect("an SVG is UTF-8 text");
            assert!(
                text.trim_start().starts_with("<svg"),
                "{path} is not an SVG"
            );
            assert!(
                !text.contains("fill=\"#") && !text.contains("stroke=\"#"),
                "{path} hard-codes a colour; icons take the text colour (currentColor)"
            );
        }
    }
}
