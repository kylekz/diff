//! Diff computation: blob pair in, renderable hunk model out.
//! Powered by `imara-diff` (line level) plus a word-level intraline pass.

use std::ops::Range;

/// Options for [`diff_blobs`].
#[derive(Debug, Clone)]
pub struct DiffOptions {
    /// Context lines around each change (git default: 3). Changes closer
    /// than `2 * context_lines` merge into one hunk.
    pub context_lines: u32,
    /// Compute word-level intraline ranges for paired changed lines.
    pub intraline: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            context_lines: 3,
            intraline: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Removed,
    Added,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// 1-based line number on the old side (`None` for Added).
    pub old_line: Option<u32>,
    /// 1-based line number on the new side (`None` for Removed).
    pub new_line: Option<u32>,
    /// Line content without its trailing newline.
    pub text: String,
    /// Byte ranges within `text` that differ from the paired line — the
    /// word-level highlight. Empty when unpaired or intraline is off.
    /// Ranges are always char-boundary-safe: `&text[r]` must never panic.
    pub intraline: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based first line of the hunk on the old side (0 when the old side
    /// is empty, matching unified-diff convention).
    pub old_start: u32,
    pub old_count: u32,
    /// 1-based first line on the new side (0 when empty).
    pub new_start: u32,
    pub new_count: u32,
    /// Context, removed, and added lines in display order (context /
    /// removed-block / added-block per change region, like git).
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileDiff {
    pub hunks: Vec<Hunk>,
    /// Either side looked binary (NUL byte within the first 8 KiB).
    /// Binary diffs have no hunks — the UI renders a placeholder.
    pub is_binary: bool,
    /// Total line counts of each side (0 for a missing side).
    pub old_line_count: u32,
    pub new_line_count: u32,
}

/// Diff two blobs. `None` = that side doesn't exist (added/deleted file).
/// Content is decoded lossily as UTF-8 — git blobs carry no encoding, and
/// review display beats strictness here.
pub fn diff_blobs(old: Option<&[u8]>, new: Option<&[u8]>, options: &DiffOptions) -> FileDiff {
    let _ = (old, new, options);
    todo!("implemented in phase 1")
}
