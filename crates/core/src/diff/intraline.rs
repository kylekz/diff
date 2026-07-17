//! Word-level (intraline) diff for one paired removed/added line.
//!
//! Tokenizes both (already newline/CR-stripped) line texts into
//! word/whitespace/punctuation runs, diffs the token sequences with
//! imara-diff, and converts the changed token spans back to byte ranges.

use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};

use super::token_source::Tokens;

/// Above this product of token counts, skip intraline highlighting for the
/// pair rather than pay for an O(n*m) diff on e.g. a huge minified line.
const MAX_TOKEN_PRODUCT: usize = 10_000;

/// If more than this fraction of either line's bytes changed, word-level
/// highlighting is noise — the line reads as rewritten, and speckling it
/// with tint fragments obscures that (R3 item 1). The whole-line
/// row tint still marks it changed; only the word-level tier goes quiet.
const MAX_CHANGED_FRACTION: f32 = 0.7;

/// Returns (removed byte ranges, added byte ranges), both empty if the pair
/// is identical, too large to diff cheaply, or otherwise has no word-level
/// difference.
pub(super) fn diff_pair(removed: &str, added: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let r_tokens = tokenize(removed);
    let a_tokens = tokenize(added);

    if r_tokens.len().saturating_mul(a_tokens.len()) > MAX_TOKEN_PRODUCT {
        return (Vec::new(), Vec::new());
    }

    let r_strs: Vec<&str> = r_tokens.iter().map(|r| &removed[r.clone()]).collect();
    let a_strs: Vec<&str> = a_tokens.iter().map(|r| &added[r.clone()]).collect();

    let input = InternedInput::new(Tokens(&r_strs), Tokens(&a_strs));
    let diff = Diff::compute(Algorithm::Histogram, &input);

    let mut r_ranges = Vec::new();
    let mut a_ranges = Vec::new();
    for hunk in diff.hunks() {
        if !hunk.before.is_empty() {
            let start = r_tokens[hunk.before.start as usize].start;
            let end = r_tokens[hunk.before.end as usize - 1].end;
            push_merged(&mut r_ranges, start..end);
        }
        if !hunk.after.is_empty() {
            let start = a_tokens[hunk.after.start as usize].start;
            let end = a_tokens[hunk.after.end as usize - 1].end;
            push_merged(&mut a_ranges, start..end);
        }
    }

    // Mostly-rewritten pair: suppress BOTH sides when either side crossed
    // the fraction (a one-sided suppression would leave an asymmetric,
    // more-confusing highlight). `trim_end` keeps trailing whitespace
    // from diluting the denominator. Known quirk (R3 review, P3): a pair
    // differing ONLY in trailing whitespace has changed-range bytes past
    // the trimmed length (fraction > 1.0) and always suppresses — the row
    // tint still marks the line changed, but the word tint won't point at
    // the invisible whitespace. Accepted as-is; clamp the numerator to
    // the trimmed length if it ever bites.
    let changed = |ranges: &[Range<usize>], len: usize| {
        len > 0
            && ranges.iter().map(|r| r.len()).sum::<usize>() as f32 / len as f32
                > MAX_CHANGED_FRACTION
    };
    if changed(&r_ranges, removed.trim_end().len()) || changed(&a_ranges, added.trim_end().len()) {
        return (Vec::new(), Vec::new());
    }
    (r_ranges, a_ranges)
}

fn push_merged(ranges: &mut Vec<Range<usize>>, new: Range<usize>) {
    if let Some(last) = ranges.last_mut()
        && new.start <= last.end
    {
        last.end = last.end.max(new.end);
        return;
    }
    ranges.push(new);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn classify(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// Splits `s` into byte ranges: runs of word chars, runs of whitespace, and
/// single-char tokens for everything else (punctuation never runs together,
/// so e.g. `((` tokenizes as two separate tokens).
fn tokenize(s: &str) -> Vec<Range<usize>> {
    let mut tokens = Vec::new();
    let mut iter = s.char_indices().peekable();
    while let Some((start, c)) = iter.next() {
        let class = classify(c);
        let mut end = start + c.len_utf8();
        if class != Class::Other {
            while let Some(&(_, next)) = iter.peek() {
                if classify(next) != class {
                    break;
                }
                end += next.len_utf8();
                iter.next();
            }
        }
        tokens.push(start..end);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_groups_words_and_spaces_splits_punctuation() {
        let tokens = tokenize("let count = 5;");
        let texts: Vec<&str> = tokens
            .iter()
            .map(|r| &"let count = 5;"[r.clone()])
            .collect();
        assert_eq!(texts, ["let", " ", "count", " ", "=", " ", "5", ";"]);
    }

    #[test]
    fn tokenize_punctuation_never_merges() {
        let tokens = tokenize("((");
        assert_eq!(tokens, [0..1, 1..2]);
    }

    #[test]
    fn tokenize_empty_string_has_no_tokens() {
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn identical_text_has_no_intraline_ranges() {
        let (r, a) = diff_pair("same", "same");
        assert!(r.is_empty());
        assert!(a.is_empty());
    }

    #[test]
    fn huge_token_product_skips_intraline() {
        // 200 * 200 = 40_000 > MAX_TOKEN_PRODUCT, so the pair is skipped
        // even though the strings clearly differ.
        let a = "x ".repeat(200);
        let b = "y ".repeat(200);
        let (r, added) = diff_pair(&a, &b);
        assert!(r.is_empty());
        assert!(added.is_empty());
    }

    // --- MAX_CHANGED_FRACTION (R3 item 1) ---------------------------------

    #[test]
    fn small_edit_keeps_intraline() {
        let (r, a) = diff_pair("let count = 5;", "let count = 6;");
        assert_eq!(r, vec![12..13], "only the changed digit is tinted");
        assert_eq!(a, vec![12..13]);
    }

    #[test]
    fn mostly_rewritten_line_suppresses_intraline() {
        // Only the trailing ";" survives between the two — far past 70%
        // changed on both sides. Word-tinting nearly the whole line is
        // noise; the row tint alone should carry it.
        let (r, a) = diff_pair(
            "let old_name = compute_thing(alpha);",
            "self.registry.insert(key, value);",
        );
        assert!(r.is_empty(), "suppressed, got {r:?}");
        assert!(a.is_empty(), "suppressed, got {a:?}");
    }

    #[test]
    fn one_side_mostly_rewritten_suppresses_both_sides() {
        // The removed side is one short token, fully replaced (100% > 70%);
        // the added side is long with plenty of unchanged... nothing — the
        // sides share nothing here, but the point is the EITHER-side rule:
        // a single crossing suppresses both, never just one (asymmetric
        // highlights read worse than none).
        let (r, a) = diff_pair("x", "a much longer replacement line entirely");
        assert!(r.is_empty());
        assert!(a.is_empty());
    }

    #[test]
    fn trailing_whitespace_does_not_dilute_the_fraction() {
        // Without trim_end, 30 trailing spaces would grow the denominator
        // and sneak a fully-rewritten visible line under the threshold.
        let removed = format!("abc{}", " ".repeat(30));
        let added = format!("xyz{}", " ".repeat(30));
        let (r, a) = diff_pair(&removed, &added);
        assert!(r.is_empty());
        assert!(a.is_empty());
    }

    #[test]
    fn moderate_edit_below_threshold_keeps_intraline() {
        // Roughly half the line changes — under 70% on both sides, so the
        // word tints stay.
        let (r, a) = diff_pair("prefix common alpha beta", "prefix common gamma delta");
        assert!(!r.is_empty());
        assert!(!a.is_empty());
    }
}
