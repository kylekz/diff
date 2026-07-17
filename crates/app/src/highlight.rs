//! Syntax highlighting for diff lines, plus overlaying the word-level
//! intraline ranges. Runs off the UI thread (inside the diff-compute task);
//! output is line-relative style runs consumed by `StyledText::with_highlights`.

use std::collections::HashMap;
use std::ops::Range;

use gpui::{HighlightStyle, Hsla};
use gpui_component::highlighter::{HighlightTheme, SyntaxHighlighter};
use ropey::Rope;

/// Line-relative syntax style runs, keyed by 1-based line number. A line
/// absent from the map (or the whole map empty) means "no syntax styling".
pub type LineRuns = HashMap<u32, Vec<(Range<usize>, HighlightStyle)>>;

/// Files larger than this are rendered unhighlighted — parsing a multi-MB
/// generated/minified blob is neither useful nor cheap.
const MAX_HIGHLIGHT_BYTES: usize = 2_000_000;

/// Lines longer than this skip styling entirely (see [`merge_line_runs`]
/// callers): one pathological minified line would otherwise carry so many
/// style runs that layout dominates. Generous for hand-written code.
pub const MAX_HIGHLIGHT_LINE: usize = 20_000;

/// Map a repo path to the language name gpui-component's highlighter expects.
/// `None` → render the file unhighlighted. Extensions are lowercased.
fn language_for_path(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next().filter(|e| *e != path)?;
    let name = match ext.to_ascii_lowercase().as_str() {
        "ts" | "cts" | "mts" => "typescript",
        "tsx" => "tsx",
        "js" | "cjs" | "mjs" | "jsx" => "javascript",
        "rs" => "rust",
        "py" | "pyi" => "python",
        "go" => "go",
        "css" => "css",
        "html" | "htm" => "html",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "sh" | "bash" => "bash",
        "md" | "markdown" => "markdown",
        "json" => "json",
        _ => return None,
    };
    Some(name)
}

/// Parse `text` as `path`'s language and return per-line style runs. Empty
/// when the language is unknown. The whole file is parsed (tree-sitter needs
/// full context); runs are then bucketed to lines and made line-relative.
pub fn highlight_file(text: &str, path: &str, theme: &HighlightTheme) -> LineRuns {
    let Some(lang) = language_for_path(path) else {
        return LineRuns::new();
    };
    if text.is_empty() || text.len() > MAX_HIGHLIGHT_BYTES {
        return LineRuns::new();
    }

    let mut highlighter = SyntaxHighlighter::new(lang);
    let rope = Rope::from_str(text);
    highlighter.update(None, &rope, None);
    let styles = highlighter.styles(&(0..text.len()), theme);
    bucket_by_line(text, styles)
}

/// Byte offset where each 0-based line begins.
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// Assign each whole-file style run to the line(s) it covers, clipped to each
/// line's *content* (excluding the trailing `\r?\n`) and rebased to be
/// line-relative — matching the newline-stripped text dv-core stores per line.
fn bucket_by_line(text: &str, styles: Vec<(Range<usize>, HighlightStyle)>) -> LineRuns {
    let starts = line_starts(text);
    let bytes = text.as_bytes();
    let mut out: LineRuns = HashMap::new();

    for (range, style) in styles {
        if is_blank_style(&style) {
            continue;
        }
        let mut pos = range.start;
        while pos < range.end {
            // Line (0-based) containing `pos`.
            let line = starts.partition_point(|&s| s <= pos) - 1;
            let line_start = starts[line];
            let line_limit = starts.get(line + 1).copied().unwrap_or(bytes.len());
            // Content end: strip the trailing '\n' and a preceding '\r'.
            let mut content_end = line_limit;
            if content_end > line_start && bytes[content_end - 1] == b'\n' {
                content_end -= 1;
                if content_end > line_start && bytes[content_end - 1] == b'\r' {
                    content_end -= 1;
                }
            }
            let seg_end = range.end.min(content_end);
            if seg_end > pos {
                let rel = (pos - line_start)..(seg_end - line_start);
                out.entry(line as u32 + 1).or_default().push((rel, style));
            }
            pos = line_limit.max(pos + 1);
        }
    }
    out
}

fn is_blank_style(style: &HighlightStyle) -> bool {
    style.color.is_none()
        && style.background_color.is_none()
        && style.font_weight.is_none()
        && style.font_style.is_none()
}

/// Overlay intraline word ranges (as a background tint) on top of the syntax
/// runs for one line, producing sorted, non-overlapping runs safe for
/// `StyledText::with_highlights`. `syntax` runs are sorted & non-overlapping
/// (from [`bucket_by_line`]); `intraline` ranges are sorted & non-overlapping
/// (from dv-core). All ranges are clamped to `len` so slicing never panics.
///
/// `intra_bg` is the word-tint
/// tier — a second, more saturated alpha layered over the row's own
/// ~12.5% tint. The caller (`workspace.rs`'s `request_diff`) supplies
/// `DvTheme::word_created_bg`/`word_deleted_bg` (`success`/`danger` @
/// 0.28), so this fn itself stays theme-agnostic; it just paints whatever
/// color it's handed onto the ranges dv-core's `intraline` module already
/// picked out (that module's own `>70% changed` suppression heuristic is
/// R3, not this fn's concern — an empty `intraline` slice already means
/// "no highlight" here regardless of why it's empty).
pub fn merge_line_runs(
    len: usize,
    syntax: &[(Range<usize>, HighlightStyle)],
    intraline: &[Range<usize>],
    intra_bg: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if syntax.is_empty() && intraline.is_empty() {
        return Vec::new();
    }

    // Boundary points that any run edge falls on; segments between adjacent
    // boundaries have a single, constant style.
    let mut bounds: Vec<usize> = vec![0, len];
    for (r, _) in syntax {
        bounds.push(r.start.min(len));
        bounds.push(r.end.min(len));
    }
    for r in intraline {
        bounds.push(r.start.min(len));
        bounds.push(r.end.min(len));
    }
    bounds.sort_unstable();
    bounds.dedup();

    // Two monotonically-advancing cursors instead of a scan per segment:
    // both inputs are sorted and non-overlapping and segment starts only
    // increase, so this is O(n) rather than O(n²) — matters on the rare
    // line carrying tens of thousands of runs.
    let mut runs = Vec::new();
    let mut si = 0usize;
    let mut ii = 0usize;
    for pair in bounds.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a >= b {
            continue;
        }
        while si < syntax.len() && syntax[si].0.end.min(len) <= a {
            si += 1;
        }
        let syntax_style = if si < syntax.len() && syntax[si].0.start.min(len) <= a {
            Some(syntax[si].1)
        } else {
            None
        };
        while ii < intraline.len() && intraline[ii].end.min(len) <= a {
            ii += 1;
        }
        let in_intra = ii < intraline.len() && intraline[ii].start.min(len) <= a;

        if syntax_style.is_none() && !in_intra {
            continue; // bare segment — let the base text style render it
        }
        let mut style = syntax_style.unwrap_or_default();
        if in_intra {
            style.background_color = Some(intra_bg);
        }
        runs.push((a..b, style));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colored(color: u8) -> HighlightStyle {
        HighlightStyle {
            color: Some(Hsla {
                h: 0.0,
                s: 0.0,
                l: color as f32 / 255.0,
                a: 1.0,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn bucket_splits_across_lines_and_rebases() {
        // "ab\ncde\n" — line1 "ab" (0..2), line2 "cde" (3..6).
        let text = "ab\ncde\n";
        let styles = vec![(0..6, colored(200))];
        let map = bucket_by_line(text, styles);
        assert_eq!(map.get(&1).unwrap()[0].0, 0..2);
        assert_eq!(map.get(&2).unwrap()[0].0, 0..3); // '\n' excluded
    }

    #[test]
    fn bucket_excludes_crlf_from_content() {
        // "ab\r\n" — content is just "ab"; run must clip before "\r\n".
        let text = "ab\r\n";
        let styles = vec![(0..4, colored(200))];
        let map = bucket_by_line(text, styles);
        assert_eq!(map.get(&1).unwrap()[0].0, 0..2);
    }

    #[test]
    fn merge_overlays_intraline_bg_on_syntax() {
        // text "let x" len 5; syntax colors 0..3 ("let"); intraline 4..5 ("x").
        let bg = Hsla {
            h: 0.5,
            s: 0.5,
            l: 0.5,
            a: 0.3,
        };
        let syntax = vec![(0..3, colored(100))];
        #[allow(clippy::single_range_in_vec_init)]
        let intra: Vec<Range<usize>> = vec![4..5];
        let runs = merge_line_runs(5, &syntax, &intra, bg);
        // Expect: 0..3 colored (no bg), 4..5 bg-only. 3..4 is bare → skipped.
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].0, 0..3);
        assert_eq!(runs[0].1.background_color, None);
        assert_eq!(runs[1].0, 4..5);
        assert_eq!(runs[1].1.background_color, Some(bg));
        assert!(runs[1].1.color.is_none());
    }

    #[test]
    fn merge_intraline_over_syntax_sets_both() {
        // syntax 0..5 colored; intraline 1..3 → segments 0..1, 1..3(bg), 3..5.
        let bg = Hsla {
            h: 0.5,
            s: 0.5,
            l: 0.5,
            a: 0.3,
        };
        let syntax = vec![(0..5, colored(100))];
        #[allow(clippy::single_range_in_vec_init)]
        let intra: Vec<Range<usize>> = vec![1..3];
        let runs = merge_line_runs(5, &syntax, &intra, bg);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].0, 0..1);
        assert_eq!(runs[1].0, 1..3);
        assert_eq!(runs[1].1.background_color, Some(bg));
        assert!(runs[1].1.color.is_some()); // keeps syntax color under the bg
        assert_eq!(runs[2].0, 3..5);
    }

    #[test]
    fn merge_many_runs_two_pointer_stays_correct() {
        // Multiple syntax runs with a gap, plus two intraline ranges — the
        // cursor sweep must resolve each segment to the right style.
        let bg = Hsla {
            h: 0.5,
            s: 0.5,
            l: 0.5,
            a: 0.3,
        };
        // text len 10; syntax: 0..2 (c10), 2..4 (c20), gap 4..6, 6..10 (c30).
        let syntax = vec![
            (0..2, colored(10)),
            (2..4, colored(20)),
            (6..10, colored(30)),
        ];
        let intra = vec![1..3, 7..8];
        let runs = merge_line_runs(10, &syntax, &intra, bg);
        // Every run is within bounds, sorted, non-overlapping.
        let mut prev_end = 0;
        for (r, _) in &runs {
            assert!(r.start >= prev_end, "overlap/unsorted at {r:?}");
            assert!(r.end <= 10);
            prev_end = r.end;
        }
        // Byte 1 (in syntax c10 + intra 1..3) → colored with bg.
        let at1 = runs
            .iter()
            .find(|(r, _)| r.start <= 1 && 1 < r.end)
            .unwrap();
        assert_eq!(at1.1.background_color, Some(bg));
        assert!(at1.1.color.is_some());
        // Byte 5 is in the syntax gap and no intra → no run covers it.
        assert!(!runs.iter().any(|(r, _)| r.start <= 5 && 5 < r.end));
        // Byte 7 (syntax c30 + intra 7..8) → colored with bg.
        let at7 = runs
            .iter()
            .find(|(r, _)| r.start <= 7 && 7 < r.end)
            .unwrap();
        assert_eq!(at7.1.background_color, Some(bg));
    }

    #[test]
    fn merge_clamps_out_of_range() {
        let bg = Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.0,
            a: 0.3,
        };
        // syntax range extends past len 3 — must clamp, never panic.
        let runs = merge_line_runs(3, &[(0..99, colored(100))], &[], bg);
        assert_eq!(runs.last().unwrap().0.end, 3);
    }

    #[test]
    fn empty_inputs_produce_no_runs() {
        let bg = Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.0,
            a: 0.3,
        };
        assert!(merge_line_runs(10, &[], &[], bg).is_empty());
    }
}
