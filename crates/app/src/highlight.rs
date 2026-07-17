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

/// WCAG relative luminance of an (opaque) color — `to_rgb()` drops alpha, so
/// callers pre-composite any translucent color onto its backing surface
/// first (see [`merge_line_runs`], which blends the intraline tint over the
/// row's base background before calling this).
fn relative_luminance(color: Hsla) -> f32 {
    let rgb = color.to_rgb();
    let channel = |c: f32| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
}

/// WCAG contrast ratio between two colors, order-independent — 1.0 is no
/// contrast at all, 21.0 is black-on-white.
fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Below this ratio, a syntax token's own foreground reads as muddy against
/// an intraline highlight background — Dracula's and Aura's comment color
/// measure ~1.5 against the green intraline-add tint (docs/backlog.md's
/// "Dracula: comment-colored text over the green intraline-add highlight
/// is muddy" entry) — and [`merge_line_runs`] swaps in a theme-provided
/// fallback foreground instead. Deliberately short of WCAG AA's 4.5 (normal
/// text): that bar would repaint most syntax colors on every intraline
/// span, not just the genuinely muddy ones. 3.0 is AA's own "large text"
/// floor — high enough to catch clearly-bad pairs, low enough to leave
/// comfortably-contrasted syntax colors alone.
const MIN_INTRA_CONTRAST: f32 = 3.0;

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

/// Whether [`highlight_file`] would produce any runs at all for this
/// text/path — the cheap pre-check `workspace.rs`'s two-stage diff pipeline
/// uses to decide if a background tree-sitter pass is worth scheduling.
/// Mirrors `highlight_file`'s own early-outs exactly (unknown language,
/// empty, over the size cap), so "false for both sides" means the
/// unhighlighted stage-1 rows are already the final render.
pub fn wants_highlight(text: &str, path: &str) -> bool {
    language_for_path(path).is_some() && !text.is_empty() && text.len() <= MAX_HIGHLIGHT_BYTES
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
///
/// `base_bg` is the row's surface color `intra_bg` paints over (an
/// approximation — it skips the row's own ~12.5% tint layered between the
/// two, so it slightly under-states the true accumulated tint, but that's
/// the conservative direction for a contrast check). `intra_fg_fallback` is
/// swapped in for a syntax token's own foreground when that color measures
/// under [`MIN_INTRA_CONTRAST`] against the blended `base_bg`/`intra_bg` —
/// the Dracula/Aura comment-on-green-intraline muddiness this fn's own doc
/// comment on `intra_bg` doesn't otherwise account for (docs/backlog.md).
pub fn merge_line_runs(
    len: usize,
    syntax: &[(Range<usize>, HighlightStyle)],
    intraline: &[Range<usize>],
    intra_bg: Hsla,
    base_bg: Hsla,
    intra_fg_fallback: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if syntax.is_empty() && intraline.is_empty() {
        return Vec::new();
    }
    let effective_intra_bg = base_bg.blend(intra_bg);

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
            if let Some(fg) = style.color
                && contrast_ratio(fg, effective_intra_bg) < MIN_INTRA_CONTRAST
            {
                style.color = Some(intra_fg_fallback);
            }
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

    /// Black — the surface backing every plain `merge_line_runs` test call
    /// below, none of which care about the contrast-override behavior
    /// (they only assert `background_color`/`color.is_some()`, never a
    /// specific `color` value) — see the dedicated `contrast_*`/
    /// `merge_swaps_*` tests further down for that.
    const OPAQUE_BLACK: Hsla = Hsla {
        h: 0.0,
        s: 0.0,
        l: 0.0,
        a: 1.0,
    };
    const OPAQUE_WHITE: Hsla = Hsla {
        h: 0.0,
        s: 0.0,
        l: 1.0,
        a: 1.0,
    };

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
        let runs = merge_line_runs(5, &syntax, &intra, bg, OPAQUE_BLACK, OPAQUE_WHITE);
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
        let runs = merge_line_runs(5, &syntax, &intra, bg, OPAQUE_BLACK, OPAQUE_WHITE);
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
        let runs = merge_line_runs(10, &syntax, &intra, bg, OPAQUE_BLACK, OPAQUE_WHITE);
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
        let runs = merge_line_runs(
            3,
            &[(0..99, colored(100))],
            &[],
            bg,
            OPAQUE_BLACK,
            OPAQUE_WHITE,
        );
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
        assert!(merge_line_runs(10, &[], &[], bg, OPAQUE_BLACK, OPAQUE_WHITE).is_empty());
    }

    #[test]
    fn contrast_ratio_black_on_white_is_maximal() {
        let ratio = contrast_ratio(OPAQUE_BLACK, OPAQUE_WHITE);
        assert!((ratio - 21.0).abs() < 0.05, "expected ~21.0, got {ratio}");
    }

    #[test]
    fn contrast_ratio_is_order_independent() {
        assert_eq!(
            contrast_ratio(OPAQUE_BLACK, OPAQUE_WHITE),
            contrast_ratio(OPAQUE_WHITE, OPAQUE_BLACK)
        );
    }

    #[test]
    fn contrast_ratio_same_color_is_one() {
        let ratio = contrast_ratio(OPAQUE_WHITE, OPAQUE_WHITE);
        assert!((ratio - 1.0).abs() < 1e-4);
    }

    #[test]
    fn contrast_ratio_matches_dracula_comment_on_green_intraline() {
        // Real numbers from the reported bug (docs/backlog.md): Dracula's
        // #6272a4 comment color measures ~1.45 against its background
        // (#282a36) blended with the green intraline tint (success @
        // 0.28) — well under MIN_INTRA_CONTRAST, confirming this is the
        // exact pair that needs the override.
        use gpui_component::Colorize as _;
        let comment = Hsla::parse_hex("#6272a4").unwrap();
        let base_bg = Hsla::parse_hex("#282a36").unwrap();
        let green = Hsla::parse_hex("#50fa7b").unwrap().opacity(0.28);
        let effective_bg = base_bg.blend(green);
        let ratio = contrast_ratio(comment, effective_bg);
        assert!(ratio < MIN_INTRA_CONTRAST, "expected muddy, got {ratio}");
    }

    #[test]
    fn merge_swaps_low_contrast_syntax_color_for_fallback_over_intra_bg() {
        // A mid-gray syntax color (l=0.5) sits low-contrast against a
        // mid-gray-ish translucent intra background over a mid-gray base —
        // the override must kick in and use the fallback instead.
        let mid_gray = HighlightStyle {
            color: Some(Hsla {
                h: 0.0,
                s: 0.0,
                l: 0.5,
                a: 1.0,
            }),
            ..Default::default()
        };
        let base_bg = Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.5,
            a: 1.0,
        };
        let intra_bg = Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.5,
            a: 0.9,
        };
        let fallback = OPAQUE_WHITE;
        let syntax = vec![(0..3, mid_gray)];
        #[allow(clippy::single_range_in_vec_init)]
        let intra: Vec<Range<usize>> = vec![0..3];
        let runs = merge_line_runs(3, &syntax, &intra, intra_bg, base_bg, fallback);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1.color, Some(fallback));
        assert_eq!(runs[0].1.background_color, Some(intra_bg));
    }

    #[test]
    fn merge_keeps_high_contrast_syntax_color_over_intra_bg() {
        // White text over a dark base + intra tint stays comfortably above
        // MIN_INTRA_CONTRAST — the override must NOT fire, so the original
        // syntax color survives untouched.
        let white_text = colored(255);
        let base_bg = OPAQUE_BLACK;
        let intra_bg = Hsla {
            h: 0.3,
            s: 0.6,
            l: 0.4,
            a: 0.28,
        };
        let fallback = Hsla {
            h: 0.9,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        };
        let syntax = vec![(0..3, white_text)];
        #[allow(clippy::single_range_in_vec_init)]
        let intra: Vec<Range<usize>> = vec![0..3];
        let runs = merge_line_runs(3, &syntax, &intra, intra_bg, base_bg, fallback);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1.color, white_text.color);
    }
}
