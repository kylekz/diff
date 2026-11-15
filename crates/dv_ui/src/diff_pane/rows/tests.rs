use dv_core::diff::{Expand, expand_gap, segments};
use dv_core::review::ThreadId;
use pretty_assertions::assert_eq;

use super::*;

/// A text diff from `(marker, text)` lines: `' '` context, `'-'` removed, `'+'` added, numbered
/// as git would.
fn text_diff(spec: &[(char, &str)]) -> TextDiff {
    let (mut old, mut new) = (0, 0);
    let lines = spec
        .iter()
        .map(|&(marker, text)| {
            let kind = match marker {
                '-' => LineKind::Removed,
                '+' => LineKind::Added,
                _ => LineKind::Context,
            };
            let old_no = (kind != LineKind::Added).then(|| {
                old += 1;
                old
            });
            let new_no = (kind != LineKind::Removed).then(|| {
                new += 1;
                new
            });
            DiffLine {
                kind,
                old_no,
                new_no,
                text: text.to_string(),
                words: Vec::new(),
            }
        })
        .collect();
    TextDiff {
        lines,
        ..TextDiff::empty()
    }
}

/// Ten unchanged lines, one line changed, ten unchanged lines.
fn edited() -> FileDiff {
    let mut spec = vec![(' ', "ctx"); 10];
    spec.push(('-', "old"));
    spec.push(('+', "new"));
    spec.extend(vec![(' ', "ctx"); 10]);
    FileDiff::Text(text_diff(&spec))
}

fn rows_of(diff: &FileDiff, mode: ViewMode, inserts: &[Insert]) -> Vec<Row> {
    let segments = diff.segments(CONTEXT_LINES);
    file_rows(&RowsSpec {
        mode,
        collapsed: false,
        content: Content::Loaded(diff),
        segments: &segments,
        inserts,
        heading: &[],
        footer: true,
        whole: false,
    })
}

fn header() -> Row {
    Row::Header {
        collapsed: false,
        labels: true,
    }
}

/// The header of a file that shows a notice: no side labels.
fn bare_header() -> Row {
    Row::Header {
        collapsed: false,
        labels: false,
    }
}

#[test]
fn unified_rows_show_the_change_with_its_context_and_gaps_either_side() {
    let diff = edited();
    let rows = rows_of(&diff, ViewMode::Unified, &[]);
    let mut expected = vec![
        header(),
        Row::Gap(Gap {
            segment: 0,
            ordinal: 0,
            lines: 0..7,
            hunk: Some(HunkRange {
                old_start: 8,
                old_count: 7,
                new_start: 8,
                new_count: 7,
            }),
            above: false,
            below: true,
        }),
    ];
    expected.extend((7..15).map(Row::Line));
    expected.push(Row::Gap(Gap {
        segment: 2,
        ordinal: 1,
        lines: 15..22,
        hunk: None,
        above: true,
        below: false,
    }));
    expected.push(Row::Footer);
    assert_eq!(rows, expected);
    let Row::Gap(gap) = &rows[1] else {
        unreachable!()
    };
    assert_eq!(
        gap.hunk.map(|hunk| hunk.to_string()).as_deref(),
        Some("@@ -8,7 +8,7 @@")
    );
}

#[test]
fn split_rows_pair_the_removed_line_with_the_added_one() {
    let diff = edited();
    let rows = rows_of(&diff, ViewMode::Split, &[]);
    let code: Vec<&Row> = rows.iter().filter(|row| row.is_code()).collect();
    let pair = |old, new| Row::Pair { old, new };
    assert_eq!(
        code,
        [
            &pair(Some(7), Some(7)),
            &pair(Some(8), Some(8)),
            &pair(Some(9), Some(9)),
            &pair(Some(10), Some(11)),
            &pair(Some(12), Some(12)),
            &pair(Some(13), Some(13)),
            &pair(Some(14), Some(14)),
        ]
    );
    // An addition without a removal faces an empty cell.
    let added = FileDiff::Text(text_diff(&[(' ', "a"), ('+', "b"), ('+', "c"), (' ', "d")]));
    let rows = rows_of(&added, ViewMode::Split, &[]);
    assert_eq!(
        rows[1..5],
        [
            pair(Some(0), Some(0)),
            pair(None, Some(1)),
            pair(None, Some(2)),
            pair(Some(3), Some(3)),
        ]
    );
}

#[test]
fn expanding_a_gap_reveals_its_lines_from_the_loaded_text() {
    let mut spec = vec![(' ', "ctx"); 40];
    spec.push(('+', "new"));
    let diff = FileDiff::Text(text_diff(&spec));
    let segs = diff.segments(CONTEXT_LINES);
    let build = |segments: &[Segment]| {
        file_rows(&RowsSpec {
            mode: ViewMode::Unified,
            collapsed: false,
            content: Content::Loaded(&diff),
            segments,
            inserts: &[],
            heading: &[],
            footer: false,
            whole: false,
        })
    };
    let gaps = |rows: &[Row]| -> Vec<Range<usize>> {
        rows.iter()
            .filter_map(|row| match row {
                Row::Gap(gap) => Some(gap.lines.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(gaps(&build(&segs)), vec![0..37]);
    // One step from below reveals the twenty lines next to the change.
    let one = expand_gap(&segs, 0, Expand::FromBelow);
    let rows = build(&one);
    assert_eq!(gaps(&rows), vec![0..17]);
    assert_eq!(rows.iter().filter(|row| row.is_code()).count(), 24);
    // The rest at once.
    let all = expand_gap(&one, 0, Expand::All);
    assert!(gaps(&build(&all)).is_empty());
}

#[test]
fn a_collapsed_file_is_its_header_alone() {
    let diff = edited();
    let segments = diff.segments(CONTEXT_LINES);
    let rows = file_rows(&RowsSpec {
        mode: ViewMode::Split,
        collapsed: true,
        content: Content::Loaded(&diff),
        segments: &segments,
        inserts: &[],
        heading: &[],
        footer: true,
        whole: false,
    });
    assert_eq!(
        rows,
        [Row::Header {
            collapsed: true,
            labels: false
        }]
    );
    assert_eq!(rows[0].height(), heights::HEADER_BAR);
    assert_eq!(header().height(), heights::HEADER);
}

#[test]
fn files_without_lines_show_a_notice() {
    let oid = Oid::parse("1111111111111111111111111111111111111111").unwrap();
    let cases = [
        (
            FileDiff::Binary {
                old_len: Some(3),
                new_len: Some(4),
            },
            Notice::Binary {
                old_len: Some(3),
                new_len: Some(4),
            },
        ),
        (
            FileDiff::TooLarge {
                old_len: None,
                new_len: Some(9),
            },
            Notice::TooLarge {
                old_len: None,
                new_len: Some(9),
            },
        ),
        (
            FileDiff::Submodule {
                old_oid: Some(oid.clone()),
                new_oid: None,
                dirty: true,
            },
            Notice::Submodule {
                old_oid: Some(oid),
                new_oid: None,
                dirty: true,
            },
        ),
        (FileDiff::Unmerged, Notice::Unmerged),
        (FileDiff::Text(TextDiff::empty()), Notice::NoContentChange),
    ];
    for (diff, notice) in cases {
        assert_eq!(
            rows_of(&diff, ViewMode::Split, &[]),
            [bare_header(), Row::Notice(notice.clone()), Row::Footer],
            "{notice:?}"
        );
    }
    for (content, notice) in [
        (Content::Loading, Notice::Loading),
        (Content::Failed, Notice::Failed),
    ] {
        let rows = file_rows(&RowsSpec {
            mode: ViewMode::Unified,
            collapsed: false,
            content,
            segments: &[],
            inserts: &[],
            heading: &[],
            footer: true,
            whole: false,
        });
        assert_eq!(rows, [bare_header(), Row::Notice(notice)]);
    }
}

#[test]
fn inserts_follow_the_row_of_their_line_on_their_side_or_the_gap_hiding_it() {
    let diff = edited();
    let thread = |n: u8| ThreadId::parse(&format!("t-01j9{n:0>22}")).unwrap();
    let inserts = [
        // The removed line, on the old side.
        Insert {
            line: 10,
            side: AnchorSide::Old,
            item: InsertItem::Thread(thread(1).into()),
        },
        // The added line, on the new side.
        Insert {
            line: 11,
            side: AnchorSide::New,
            item: InsertItem::Composer,
        },
        // A line the first gap hides.
        Insert {
            line: 2,
            side: AnchorSide::New,
            item: InsertItem::Thread(thread(2).into()),
        },
    ];
    let unified = rows_of(&diff, ViewMode::Unified, &inserts);
    let at = |rows: &[Row], item: &InsertItem| {
        rows.iter()
            .position(|row| matches!(row, Row::Insert(insert) if &insert.item == item))
            .unwrap()
    };
    let one = InsertItem::Thread(thread(1).into());
    let two = InsertItem::Thread(thread(2).into());
    assert_eq!(unified[at(&unified, &one) - 1], Row::Line(10));
    assert_eq!(
        unified[at(&unified, &InsertItem::Composer) - 1],
        Row::Line(11)
    );
    assert!(matches!(unified[at(&unified, &two) - 1], Row::Gap(_)));

    // In split view both follow the pair that shows them: the old side's first.
    let split = rows_of(&diff, ViewMode::Split, &inserts);
    let pair = Row::Pair {
        old: Some(10),
        new: Some(11),
    };
    assert_eq!(split[at(&split, &one) - 1], pair);
    assert_eq!(at(&split, &InsertItem::Composer), at(&split, &one) + 1);
    // Inserts sit inside a change block without splitting it.
    assert_eq!(change_starts(&split, &text(&diff).lines).len(), 1);
}

#[test]
fn outdated_threads_head_the_file_under_its_header_unless_it_is_collapsed() {
    let diff = edited();
    let segments = diff.segments(CONTEXT_LINES);
    let thread = |n: u8| ThreadId::parse(&format!("t-01j9{n:0>22}")).unwrap();
    let outdated = [
        InsertItem::Thread(thread(1).into()),
        InsertItem::Thread(thread(2).into()),
    ];
    let spec = |collapsed| RowsSpec {
        mode: ViewMode::Unified,
        collapsed,
        content: Content::Loaded(&diff),
        segments: &segments,
        inserts: &[],
        heading: &outdated,
        footer: true,
        whole: false,
    };
    let rows = file_rows(&spec(false));
    assert_eq!(rows[0], header());
    assert_eq!(rows[1], Row::Heading(InsertItem::Thread(thread(1).into())));
    assert_eq!(rows[2], Row::Heading(InsertItem::Thread(thread(2).into())));
    assert!(matches!(rows[3], Row::Gap(_)));
    // A card is measured, keyed like an inline card of the same thread, and names its thread.
    assert!(!rows[1].has_fixed_height());
    assert_eq!(
        rows[1].key(),
        RowKey::Insert(InsertItem::Thread(thread(1).into()))
    );
    assert_eq!(rows[1].thread(), Some(&thread(1).into()));
    assert_eq!(rows[3].thread(), None);
    // A viewed file shows its header alone, cards included.
    assert_eq!(
        file_rows(&spec(true)),
        [Row::Header {
            collapsed: true,
            labels: false
        }]
    );
}

fn text(diff: &FileDiff) -> &TextDiff {
    diff.as_text().unwrap()
}

#[test]
fn the_cursor_steps_through_code_rows_and_keeps_its_column_in_split_view() {
    let diff = FileDiff::Text(text_diff(&[
        (' ', "a"),
        ('-', "b"),
        ('+', "B"),
        ('+', "C"),
        (' ', "d"),
    ]));
    let lines = &text(&diff).lines;
    let unified = rows_of(&diff, ViewMode::Unified, &[]);
    // Unified: every line, each on its natural side.
    let (row, pos) = end_cell(&unified, lines, ViewMode::Unified, Dir::Down, None).unwrap();
    assert_eq!(
        (row, pos),
        (
            1,
            LinePos {
                line: 0,
                side: AnchorSide::New
            }
        )
    );
    let (row, pos) = step(&unified, lines, ViewMode::Unified, row, Dir::Down, None).unwrap();
    assert_eq!(
        pos,
        LinePos {
            line: 1,
            side: AnchorSide::Old
        }
    );
    // A selection on the new side skips the removed line.
    let (_, pos) = step(
        &unified,
        lines,
        ViewMode::Unified,
        1,
        Dir::Down,
        Some(AnchorSide::New),
    )
    .unwrap();
    assert_eq!(pos.line, 2);
    assert_eq!(
        step(&unified, lines, ViewMode::Unified, row, Dir::Up, None).map(|(_, pos)| pos.line),
        Some(0)
    );

    // Split: rows are a, b|B, -|C, d. The old column skips the empty cell.
    let split = rows_of(&diff, ViewMode::Split, &[]);
    let old = Some(AnchorSide::Old);
    let (row, pos) = step(&split, lines, ViewMode::Split, 2, Dir::Down, old).unwrap();
    assert_eq!(
        (row, pos),
        (
            4,
            LinePos {
                line: 4,
                side: AnchorSide::Old
            }
        )
    );
    let (row, _) = step(
        &split,
        lines,
        ViewMode::Split,
        2,
        Dir::Down,
        Some(AnchorSide::New),
    )
    .unwrap();
    assert_eq!(row, 3);
    assert_eq!(
        find_row(
            &split,
            lines,
            ViewMode::Split,
            LinePos {
                line: 3,
                side: AnchorSide::New
            }
        ),
        Some(3)
    );
    assert_eq!(
        find_row(
            &split,
            lines,
            ViewMode::Split,
            LinePos {
                line: 3,
                side: AnchorSide::Old
            }
        ),
        None
    );
    // The last cell, and a file with no old side falls back to the new one.
    assert_eq!(
        end_cell(&split, lines, ViewMode::Split, Dir::Up, old).map(|(row, _)| row),
        Some(4)
    );
    let added = FileDiff::Text(text_diff(&[('+', "x")]));
    let added_rows = rows_of(&added, ViewMode::Split, &[]);
    assert_eq!(
        end_cell(
            &added_rows,
            &text(&added).lines,
            ViewMode::Split,
            Dir::Down,
            old
        )
        .map(|(_, pos)| pos.side),
        Some(AnchorSide::New)
    );
}

#[test]
fn change_blocks_start_after_context_or_a_gap() {
    let diff = FileDiff::Text(text_diff(&[
        ('-', "a"),
        (' ', "b"),
        ('+', "c"),
        ('+', "d"),
        (' ', "e"),
        ('-', "f"),
        ('+', "F"),
    ]));
    let lines = &text(&diff).lines;
    let unified = rows_of(&diff, ViewMode::Unified, &[]);
    assert_eq!(change_starts(&unified, lines), [1, 3, 6]);
    let split = rows_of(&diff, ViewMode::Split, &[]);
    let starts = change_starts(&split, lines);
    assert_eq!(starts, [1, 3, 6]);
    // The cursor lands on a pair's new line, and on the removed line of a removal.
    assert_eq!(
        change_pos(&split[6], lines, ViewMode::Split),
        Some(LinePos {
            line: 6,
            side: AnchorSide::New
        })
    );
    assert_eq!(
        change_pos(&split[1], lines, ViewMode::Split),
        Some(LinePos {
            line: 0,
            side: AnchorSide::Old
        })
    );
}

#[test]
fn the_nearest_gap_and_what_a_selection_anchors() {
    let diff = edited();
    let rows = rows_of(&diff, ViewMode::Unified, &[]);
    // Gaps at rows 1 and 10.
    assert_eq!(nearest_gap(&rows, 3), Some(1));
    assert_eq!(nearest_gap(&rows, 8), Some(10));
    assert_eq!(nearest_gap(&rows[..2], 0), Some(1));
    let lines = &text(&diff).lines;
    // Lines 9..=12 hold old 10, 11, 12 and new 10, 11, 12.
    assert_eq!(
        selected_numbers(lines, AnchorSide::New, 12, 9),
        Some((10, 12))
    );
    assert_eq!(
        selected_numbers(lines, AnchorSide::Old, 10, 10),
        Some((11, 11))
    );
    assert_eq!(selected_numbers(lines, AnchorSide::Old, 11, 11), None);
}

#[test]
fn row_keys_name_what_a_row_stands_for_in_either_mode() {
    let diff = edited();
    let unified = rows_of(&diff, ViewMode::Unified, &[]);
    let split = rows_of(&diff, ViewMode::Split, &[]);
    let keys = |rows: &[Row]| rows.iter().map(Row::key).collect::<Vec<_>>();
    assert!(keys(&unified).contains(&RowKey::Line(11)));
    assert!(keys(&split).contains(&RowKey::Line(11)));
    assert_eq!(keys(&unified)[1], RowKey::Gap(0));
    assert_eq!(Row::Footer.key(), RowKey::Footer);
    assert!(segments(&text(&diff).lines, CONTEXT_LINES).len() == 3);
}
