//! Line-level diff: run imara-diff over already-split raw lines, then merge
//! the resulting change regions into displayable [`Hunk`]s.

use imara_diff::{Algorithm, Diff, InternedInput};

use super::token_source::Tokens;
use super::{DiffLine, DiffOptions, Hunk, LineKind, intraline};

pub(super) fn build_hunks(
    old_lines: &[&str],
    new_lines: &[&str],
    options: &DiffOptions,
) -> Vec<Hunk> {
    let input = InternedInput::new(Tokens(old_lines), Tokens(new_lines));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let regions: Vec<imara_diff::Hunk> = diff.hunks().collect();
    if regions.is_empty() {
        return Vec::new();
    }

    let context = options.context_lines as usize;
    let merge_gap = context.saturating_mul(2);

    let mut hunks = Vec::new();
    let mut start = 0;
    for i in 1..regions.len() {
        let gap = regions[i].before.start as usize - regions[i - 1].before.end as usize;
        if gap > merge_gap {
            hunks.push(assemble(
                &regions, start, i, old_lines, new_lines, context, options,
            ));
            start = i;
        }
    }
    hunks.push(assemble(
        &regions,
        start,
        regions.len(),
        old_lines,
        new_lines,
        context,
        options,
    ));
    hunks
}

/// Assembles one displayed hunk from `regions[start..end]` (a maximal run of
/// change regions close enough to merge). Unchanged runs before/after the
/// group are common to both sides by construction, so their length can be
/// measured on either side; here it's always measured on the old side.
fn assemble(
    regions: &[imara_diff::Hunk],
    start: usize,
    end: usize,
    old_lines: &[&str],
    new_lines: &[&str],
    context: usize,
    options: &DiffOptions,
) -> Hunk {
    let group = &regions[start..end];
    let first = &group[0];
    let last = &group[group.len() - 1];

    let gap_before = if start == 0 {
        first.before.start as usize
    } else {
        first.before.start as usize - regions[start - 1].before.end as usize
    };
    let leading = context.min(gap_before);

    let gap_after = if end == regions.len() {
        old_lines.len() - last.before.end as usize
    } else {
        regions[end].before.start as usize - last.before.end as usize
    };
    let trailing = context.min(gap_after);

    let old_first = first.before.start as usize - leading;
    let new_first = first.after.start as usize - leading;
    let old_last = last.before.end as usize + trailing;
    let new_last = last.after.end as usize + trailing;

    let mut lines = Vec::with_capacity((old_last - old_first) + (new_last - new_first));

    push_context(&mut lines, old_lines, old_first, new_first, leading);

    for (i, region) in group.iter().enumerate() {
        push_change(&mut lines, old_lines, new_lines, region, options);

        if let Some(next) = group.get(i + 1) {
            let old_gap_start = region.before.end as usize;
            let new_gap_start = region.after.end as usize;
            let gap_len = next.before.start as usize - old_gap_start;
            push_context(&mut lines, old_lines, old_gap_start, new_gap_start, gap_len);
        }
    }

    push_context(
        &mut lines,
        old_lines,
        last.before.end as usize,
        last.after.end as usize,
        trailing,
    );

    let old_count = (old_last - old_first) as u32;
    let new_count = (new_last - new_first) as u32;

    // Unified convention: a side showing zero lines anchors to the line
    // *before* the hunk on that side — which is exactly its 0-based first
    // index (0 at the top of the file, so whole-file adds still get 0).
    Hunk {
        old_start: if old_count == 0 {
            old_first as u32
        } else {
            old_first as u32 + 1
        },
        old_count,
        new_start: if new_count == 0 {
            new_first as u32
        } else {
            new_first as u32 + 1
        },
        new_count,
        lines,
    }
}

/// Context lines are identical on both sides, so only `old_lines` is needed
/// for the text; `old_start`/`new_start` (0-indexed) are tracked separately
/// only because the two sides can be at different absolute offsets.
fn push_context(
    lines: &mut Vec<DiffLine>,
    old_lines: &[&str],
    old_start: usize,
    new_start: usize,
    count: usize,
) {
    for k in 0..count {
        lines.push(DiffLine {
            kind: LineKind::Context,
            old_line: Some((old_start + k + 1) as u32),
            new_line: Some((new_start + k + 1) as u32),
            text: strip_ending(old_lines[old_start + k]).to_string(),
            intraline: Vec::new(),
        });
    }
}

fn push_change(
    lines: &mut Vec<DiffLine>,
    old_lines: &[&str],
    new_lines: &[&str],
    region: &imara_diff::Hunk,
    options: &DiffOptions,
) {
    let old_start = region.before.start as usize;
    let new_start = region.after.start as usize;

    let removed: Vec<&str> = old_lines[old_start..region.before.end as usize]
        .iter()
        .map(|l| strip_ending(l))
        .collect();
    let added: Vec<&str> = new_lines[new_start..region.after.end as usize]
        .iter()
        .map(|l| strip_ending(l))
        .collect();

    let mut removed_intraline = vec![Vec::new(); removed.len()];
    let mut added_intraline = vec![Vec::new(); added.len()];

    if options.intraline {
        // Positional pairing only, like GitHub: the i-th removed line pairs
        // with the i-th added line; extra lines on either side stay unpaired.
        let pairs = removed.len().min(added.len());
        for i in 0..pairs {
            let (r, a) = intraline::diff_pair(removed[i], added[i]);
            removed_intraline[i] = r;
            added_intraline[i] = a;
        }
    }

    for (offset, (text, ranges)) in removed.into_iter().zip(removed_intraline).enumerate() {
        lines.push(DiffLine {
            kind: LineKind::Removed,
            old_line: Some((old_start + offset + 1) as u32),
            new_line: None,
            text: text.to_string(),
            intraline: ranges,
        });
    }
    for (offset, (text, ranges)) in added.into_iter().zip(added_intraline).enumerate() {
        lines.push(DiffLine {
            kind: LineKind::Added,
            old_line: None,
            new_line: Some((new_start + offset + 1) as u32),
            text: text.to_string(),
            intraline: ranges,
        });
    }
}

/// The diff itself runs on raw lines (with `\n`, possibly `\r\n`) so that a
/// missing/added trailing newline or a CRLF-only change is detected as a
/// change; the stored/displayed text always has both stripped.
fn strip_ending(raw: &str) -> &str {
    let s = raw.strip_suffix('\n').unwrap_or(raw);
    s.strip_suffix('\r').unwrap_or(s)
}
