//! Diff computation: blob pair in, renderable hunk model out.
//! Powered by `imara-diff` (line level) plus a word-level intraline pass.

use std::ops::Range;

mod intraline;
mod line_diff;
mod token_source;

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
    if old.is_some_and(is_binary) || new.is_some_and(is_binary) {
        return FileDiff {
            is_binary: true,
            ..Default::default()
        };
    }

    let old_text = old.map(|d| String::from_utf8_lossy(d).into_owned());
    let new_text = new.map(|d| String::from_utf8_lossy(d).into_owned());

    let old_lines = raw_lines(old_text.as_deref().unwrap_or(""));
    let new_lines = raw_lines(new_text.as_deref().unwrap_or(""));

    let old_line_count = old_lines.len() as u32;
    let new_line_count = new_lines.len() as u32;

    let hunks = line_diff::build_hunks(&old_lines, &new_lines, options);

    FileDiff {
        hunks,
        is_binary: false,
        old_line_count,
        new_line_count,
    }
}

fn is_binary(data: &[u8]) -> bool {
    let n = data.len().min(8192);
    data[..n].contains(&0)
}

// `split_inclusive` keeps a trailing partial line without its own newline
// (e.g. "a\nb" -> ["a\n", "b"]), which is exactly the raw-line semantics the
// diff needs so a missing final newline shows up as a change.
fn raw_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split_inclusive('\n').collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(old: u32, new: u32, text: &str) -> DiffLine {
        DiffLine {
            kind: LineKind::Context,
            old_line: Some(old),
            new_line: Some(new),
            text: text.to_string(),
            intraline: vec![],
        }
    }
    fn rem(old: u32, text: &str) -> DiffLine {
        DiffLine {
            kind: LineKind::Removed,
            old_line: Some(old),
            new_line: None,
            text: text.to_string(),
            intraline: vec![],
        }
    }
    fn add(new: u32, text: &str) -> DiffLine {
        DiffLine {
            kind: LineKind::Added,
            old_line: None,
            new_line: Some(new),
            text: text.to_string(),
            intraline: vec![],
        }
    }

    fn make_lines(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("L{i}\n")).collect()
    }

    // T1: identical inputs.
    #[test]
    fn t1_identical_inputs_no_hunks() {
        let text = make_lines(5).concat();
        let d = diff_blobs(
            Some(text.as_bytes()),
            Some(text.as_bytes()),
            &DiffOptions::default(),
        );
        assert!(d.hunks.is_empty());
        assert_eq!(d.old_line_count, 5);
        assert_eq!(d.new_line_count, 5);
        assert!(!d.is_binary);
    }

    // T2: single-line modification, default options.
    #[test]
    fn t2_single_line_modified_default_context() {
        let old_lines = make_lines(10);
        let mut new_lines = old_lines.clone();
        new_lines[4] = "MODIFIED\n".to_string();
        let old = old_lines.concat();
        let new = new_lines.concat();

        let d = diff_blobs(
            Some(old.as_bytes()),
            Some(new.as_bytes()),
            &DiffOptions::default(),
        );
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(h.old_start, 2);
        assert_eq!(h.new_start, 2);
        assert_eq!(h.old_count, 7);
        assert_eq!(h.new_count, 7);

        let kinds: Vec<LineKind> = h.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Context,
                LineKind::Context,
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Context,
                LineKind::Context,
                LineKind::Context,
            ]
        );

        let old_line_nums: Vec<Option<u32>> = h.lines.iter().map(|l| l.old_line).collect();
        assert_eq!(
            old_line_nums,
            [
                Some(2),
                Some(3),
                Some(4),
                Some(5),
                None,
                Some(6),
                Some(7),
                Some(8)
            ]
        );

        let new_line_nums: Vec<Option<u32>> = h.lines.iter().map(|l| l.new_line).collect();
        assert_eq!(
            new_line_nums,
            [
                Some(2),
                Some(3),
                Some(4),
                None,
                Some(5),
                Some(6),
                Some(7),
                Some(8)
            ]
        );
    }

    // T3: change with no leading context (first line) and no trailing
    // context (last line) — a 10-line file keeps context from swallowing
    // the whole file so the clipped side is visible.
    #[test]
    fn t3_change_on_first_line_no_leading_context() {
        let old_lines = make_lines(10);
        let mut new_lines = old_lines.clone();
        new_lines[0] = "X0\n".to_string();
        let old = old_lines.concat();
        let new = new_lines.concat();

        let d = diff_blobs(
            Some(old.as_bytes()),
            Some(new.as_bytes()),
            &DiffOptions::default(),
        );
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.old_count, 4);
        assert_eq!(h.new_count, 4);
    }

    #[test]
    fn t3_change_on_last_line_no_trailing_context() {
        let old_lines = make_lines(10);
        let mut new_lines = old_lines.clone();
        new_lines[9] = "X9\n".to_string();
        let old = old_lines.concat();
        let new = new_lines.concat();

        let d = diff_blobs(
            Some(old.as_bytes()),
            Some(new.as_bytes()),
            &DiffOptions::default(),
        );
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(h.old_start, 7);
        assert_eq!(h.new_start, 7);
        assert_eq!(h.old_count, 4);
        assert_eq!(h.new_count, 4);
    }

    // T4: two single-line changes; gap of 6 (== 2*context) merges into one
    // hunk, gap of 7 splits into two.
    #[test]
    fn t4_changes_within_merge_threshold_become_one_hunk() {
        let old_lines = make_lines(15);
        let mut new_lines = old_lines.clone();
        new_lines[2] = "X2\n".to_string();
        new_lines[9] = "X9\n".to_string(); // gap = 9 - 3 = 6 <= 2*3
        let old = old_lines.concat();
        let new = new_lines.concat();

        let d = diff_blobs(
            Some(old.as_bytes()),
            Some(new.as_bytes()),
            &DiffOptions::default(),
        );
        assert_eq!(d.hunks.len(), 1);
        assert_eq!(d.hunks[0].old_start, 1);
        assert_eq!(d.hunks[0].new_start, 1);
    }

    #[test]
    fn t4_changes_beyond_merge_threshold_stay_separate_hunks() {
        let old_lines = make_lines(15);
        let mut new_lines = old_lines.clone();
        new_lines[2] = "X2\n".to_string();
        new_lines[10] = "X10\n".to_string(); // gap = 10 - 3 = 7 > 2*3
        let old = old_lines.concat();
        let new = new_lines.concat();

        let d = diff_blobs(
            Some(old.as_bytes()),
            Some(new.as_bytes()),
            &DiffOptions::default(),
        );
        assert_eq!(d.hunks.len(), 2);
        assert_eq!(d.hunks[0].old_start, 1);
        assert_eq!(d.hunks[1].old_start, 8);
    }

    // T5: whole-file add, mirrored as whole-file delete.
    #[test]
    fn t5_added_file() {
        let d = diff_blobs(None, Some(b"a\nb\nc\n"), &DiffOptions::default());
        assert_eq!(d.old_line_count, 0);
        assert_eq!(d.new_line_count, 3);
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(h.old_start, 0);
        assert_eq!(h.old_count, 0);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_count, 3);
        assert!(
            h.lines
                .iter()
                .all(|l| l.kind == LineKind::Added && l.old_line.is_none())
        );
        let new_line_nums: Vec<Option<u32>> = h.lines.iter().map(|l| l.new_line).collect();
        assert_eq!(new_line_nums, [Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn t5_deleted_file() {
        let d = diff_blobs(Some(b"a\nb\nc\n"), None, &DiffOptions::default());
        assert_eq!(d.old_line_count, 3);
        assert_eq!(d.new_line_count, 0);
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(h.new_start, 0);
        assert_eq!(h.new_count, 0);
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_count, 3);
        assert!(
            h.lines
                .iter()
                .all(|l| l.kind == LineKind::Removed && l.new_line.is_none())
        );
        let old_line_nums: Vec<Option<u32>> = h.lines.iter().map(|l| l.old_line).collect();
        assert_eq!(old_line_nums, [Some(1), Some(2), Some(3)]);
    }

    // T6: present-but-empty old side behaves like None.
    #[test]
    fn t6_empty_old_side() {
        let d = diff_blobs(Some(b""), Some(b"x\n"), &DiffOptions::default());
        assert_eq!(d.old_line_count, 0);
        assert_eq!(d.hunks.len(), 1);
        assert!(d.hunks[0].lines.iter().all(|l| l.kind == LineKind::Added));
    }

    // T7: missing trailing newline is a real change (raw lines differ),
    // even though the stripped, stored text is identical on both sides.
    #[test]
    fn t7_missing_trailing_newline_is_a_change() {
        let d = diff_blobs(Some(b"a\nb"), Some(b"a\nb\n"), &DiffOptions::default());
        assert_eq!(d.old_line_count, 2);
        assert_eq!(d.new_line_count, 2);
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.old_count, 2);
        assert_eq!(h.new_count, 2);
        assert_eq!(h.lines, vec![ctx(1, 1, "a"), rem(2, "b"), add(2, "b")]);
    }

    // T8: CRLF-only change on one line; stored text never contains \r.
    #[test]
    fn t8_crlf_change_strips_cr_from_stored_text() {
        let d = diff_blobs(
            Some(b"a\r\nb\r\n"),
            Some(b"a\r\nc\r\n"),
            &DiffOptions::default(),
        );
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        let texts: Vec<(&str, LineKind)> =
            h.lines.iter().map(|l| (l.text.as_str(), l.kind)).collect();
        assert_eq!(
            texts,
            [
                ("a", LineKind::Context),
                ("b", LineKind::Removed),
                ("c", LineKind::Added)
            ]
        );
        assert!(h.lines.iter().all(|l| !l.text.contains('\r')));
    }

    // T9: intraline on a plain-ASCII line, offsets computed by hand.
    #[test]
    fn t9_intraline_number_change() {
        let d = diff_blobs(
            Some(b"let count = 5;"),
            Some(b"let count = 10;"),
            &DiffOptions::default(),
        );
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(h.lines.len(), 2);
        assert_eq!(h.lines[0].kind, LineKind::Removed);
        assert_eq!(h.lines[1].kind, LineKind::Added);
        assert_eq!(h.lines[0].text, "let count = 5;");
        assert_eq!(h.lines[1].text, "let count = 10;");
        assert_eq!(h.lines[0].intraline, vec![12..13]); // "5"
        assert_eq!(h.lines[1].intraline, vec![12..14]); // "10"
        assert_eq!(&h.lines[0].text[12..13], "5");
        assert_eq!(&h.lines[1].text[12..14], "10");
    }

    // T10: intraline over multibyte (CJK) text; ranges verified by slicing
    // rather than hand-computed byte offsets, which also proves no panic.
    #[test]
    fn t10_intraline_multibyte() {
        let d = diff_blobs(
            Some("名前 = 東京".as_bytes()),
            Some("名前 = 大阪".as_bytes()),
            &DiffOptions::default(),
        );
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(h.lines[0].intraline.len(), 1);
        assert_eq!(h.lines[1].intraline.len(), 1);
        let r = h.lines[0].intraline[0].clone();
        let a = h.lines[1].intraline[0].clone();
        assert_eq!(&h.lines[0].text[r], "東京");
        assert_eq!(&h.lines[1].text[a], "大阪");
    }

    // T11: 2 removed lines vs 1 added line — only the first removed line is
    // paired (and gets intraline); the second is left unpaired and empty.
    // The paired lines share most of their text so the pairing assertion
    // isn't masked by the >70%-changed suppression (R3 — see
    // `intraline::MAX_CHANGED_FRACTION`; the old `foo1`/`foo9` fixture was
    // a 100%-changed token pair, which that rule now correctly blanks).
    #[test]
    fn t11_unequal_block_lengths_pair_positionally() {
        let d = diff_blobs(
            Some(b"shared words one\nsecond line\n"),
            Some(b"shared words two\n"),
            &DiffOptions::default(),
        );
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        let removed: Vec<&DiffLine> = h
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Removed)
            .collect();
        let added: Vec<&DiffLine> = h
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Added)
            .collect();
        assert_eq!(removed.len(), 2);
        assert_eq!(added.len(), 1);
        assert_eq!(removed[0].intraline, vec![13..16]);
        assert!(removed[1].intraline.is_empty());
        assert_eq!(added[0].intraline, vec![13..16]);
    }

    // T12: binary detection via NUL byte in the new side.
    #[test]
    fn t12_binary_new_side() {
        let mut new = vec![b'a'; 10];
        new[5] = 0;
        let d = diff_blobs(Some(b"abc"), Some(&new), &DiffOptions::default());
        assert!(d.is_binary);
        assert!(d.hunks.is_empty());
        assert_eq!(d.old_line_count, 0);
        assert_eq!(d.new_line_count, 0);
    }

    // T13: large file sanity check, no timing assertions.
    #[test]
    fn t13_large_file_single_change() {
        let n = 5000usize;
        let old_lines = make_lines(n);
        let mut new_lines = old_lines.clone();
        new_lines[2500] = "CHANGED\n".to_string();
        let old = old_lines.concat();
        let new = new_lines.concat();

        let d = diff_blobs(
            Some(old.as_bytes()),
            Some(new.as_bytes()),
            &DiffOptions::default(),
        );
        assert_eq!(d.old_line_count, n as u32);
        assert_eq!(d.new_line_count, n as u32);
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(h.old_start, 2498);
        assert_eq!(h.new_start, 2498);
        assert_eq!(h.old_count, 7);
        assert_eq!(h.new_count, 7);
    }

    // T14: intraline disabled — every line's ranges stay empty even though
    // the changed pair clearly differs.
    #[test]
    fn t14_intraline_disabled_leaves_ranges_empty() {
        let options = DiffOptions {
            context_lines: 3,
            intraline: false,
        };
        let d = diff_blobs(Some(b"let count = 5;"), Some(b"let count = 10;"), &options);
        let h = &d.hunks[0];
        assert!(h.lines.iter().all(|l| l.intraline.is_empty()));
    }

    // T15: with zero context, a mid-file pure insertion must anchor
    // old_start to the line BEFORE the insertion point (unified
    // convention: `@@ -2,0 +3,1 @@`), not 0 — 0 is only for insertions at
    // the very top / whole-file adds.
    #[test]
    fn t15_zero_context_mid_insertion_anchor() {
        let options = DiffOptions {
            context_lines: 0,
            intraline: true,
        };
        let d = diff_blobs(Some(b"a\nb\nc\n"), Some(b"a\nb\nX\nc\n"), &options);
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!((h.old_start, h.old_count), (2, 0));
        assert_eq!((h.new_start, h.new_count), (3, 1));

        // Mirror: mid-file pure deletion anchors new_start the same way.
        let d = diff_blobs(Some(b"a\nb\nX\nc\n"), Some(b"a\nb\nc\n"), &options);
        let h = &d.hunks[0];
        assert_eq!((h.old_start, h.old_count), (3, 1));
        assert_eq!((h.new_start, h.new_count), (2, 0));
    }

    // T16: the slider/indent heuristic (`postprocess_lines`). Appending a
    // new function to a file whose functions end in identical `}\n` lines
    // makes the added run's boundary ambiguous — it can slide to start at
    // the old `}` or at the new `fn`. git's heuristic (and now ours) picks
    // the block that starts at the new function, keeping the original
    // closing brace as context.
    #[test]
    fn t16_indent_heuristic_slides_added_block_to_function_start() {
        let options = DiffOptions {
            context_lines: 0,
            intraline: false,
        };
        let old = b"fn a() {\n    1\n}\n";
        let new = b"fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n";
        let d = diff_blobs(Some(old), Some(new), &options);
        assert_eq!(d.hunks.len(), 1);
        let added: Vec<&str> = d.hunks[0]
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Added)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(
            added,
            vec!["", "fn b() {", "    2", "}"],
            "added block must start at the blank line + new fn, not slide up over the old }}"
        );
    }
}
