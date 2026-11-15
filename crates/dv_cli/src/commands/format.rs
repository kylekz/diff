//! The text output's lines (docs/PLAN.md §10.3): short, stable, no colour, no trailing
//! whitespace. Fields are separated by two spaces, so a line splits cleanly on runs of
//! whitespace except within a label or a comment's summary, which come last or between fixed
//! words. Whatever a person or an agent wrote (a comment, a reply, a note, a round's summary) is
//! indented by [`BODY_INDENT`], so it never reaches column 0, where headers and code rows stand.
//! A path, a name or a row of code that holds a line break can't either: `output::text` spells
//! out every control character in a line it prints (`crate::escape`).

use dv_core::diff::LineKind;
use dv_core::github::hunk_rows;
use dv_core::review::AnchorSide;
use dv_core::review::excerpt::{code_row, hidden_span};

use crate::dto::{
    GithubCountsDto, GithubDto, PrDto, ReviewDto, RoundDto, ThreadCountsDto, ThreadDto, TrailDto,
};
use crate::escape;

/// What every line someone wrote starts with.
pub(crate) const BODY_INDENT: &str = "    ";

/// `r-…  working tree vs main  changes_requested  round 2  1 open, 0 addressed, 2 resolved`;
/// a PR review's round is its pull request's, as the app shows it
/// (`dv_core::review::displayed_round`), and one whose pull request has threads of GitHub's
/// own ends with them, counted apart (PLAN §9.6): `…  on GitHub: 5 open, 1 resolved`.
pub(crate) fn review_line(review: &ReviewDto) -> String {
    let mut line = format!(
        "{}  {}  {}  round {}  {}",
        review.id,
        review.target.label,
        review.status,
        dv_core::review::displayed_round(review.round, review.pr.as_ref().map(|pr| pr.pr_round)),
        counts(&review.threads)
    );
    let github = review.pr.as_ref().and_then(|pr| pr.threads);
    if let Some(github) = github.filter(|github| github.open + github.resolved > 0) {
        line.push_str(&format!("  on GitHub: {}", github_counts(&github)));
    }
    line
}

/// `5 open, 1 resolved`: GitHub's threads on a pull request.
pub(crate) fn github_counts(counts: &GithubCountsDto) -> String {
    format!("{} open, {} resolved", counts.open, counts.resolved)
}

/// A PR review's pull request: `pull request: https://github.com/acme/api/pull/482  open
/// head 5746f3c  PR round 2` (the repository and number while its URL isn't known, and no
/// state before it was read).
pub(crate) fn pr_line(pr: &PrDto) -> String {
    let mut fields = vec![match &pr.url {
        Some(url) => url.clone(),
        None => format!("{}/{}#{}", pr.owner, pr.repo, pr.number),
    }];
    fields.extend(pr.state.map(str::to_string));
    fields.push(format!("head {}", &pr.head_oid[..pr.head_oid.len().min(7)]));
    fields.push(format!("PR round {}", pr.pr_round));
    format!("pull request: {}", fields.join("  "))
}

/// `1 open, 0 addressed, 2 resolved`, then `, 3 annotations` when there are any (PLAN §19).
pub(crate) fn counts(counts: &ThreadCountsDto) -> String {
    let mut text = format!(
        "{} open, {} addressed, {} resolved",
        counts.open, counts.addressed, counts.resolved
    );
    match counts.annotation {
        0 => {}
        1 => text.push_str(", 1 annotation"),
        n => text.push_str(&format!(", {n} annotations")),
    }
    text
}

/// A thread's header line: `t-…  open  src/a.ts:12`.
pub(crate) fn thread_line(thread: &ThreadDto) -> String {
    format!("{}  {}  {}", thread.id, thread.state, location(thread))
}

/// A thread as `dv comment list` and `dv comment show` print it (PLAN §10.3): its header
/// ([`thread_line`]); the excerpt of the diff around its code now, then, when that code changed
/// since the comment, `as commented in round N:` and the lines as written; or, when its code
/// can't be found, only those lines between the ones that stood around them, then where that
/// code went when its round's snapshot tells ([`trail_lines`]). Then the comment
/// under `<kind> <name>, round <n>:`, each reply under `<kind> <name>:` and the last state
/// change's note under `<state> note:`, each body indented. A comment on the whole file has
/// no code: one line says so ([`file_level_line`]).
///
/// One of GitHub's threads (PLAN §9.10) reads `as commented on GitHub` where a review's own
/// names its round; one without a place in the diff shows the end of its diff hunk as GitHub
/// shows it ([`hunk_lines`]) in place of its code, under `(outdated there)` when GitHub calls
/// it outdated and `(can't be placed here)` when dv can't tell which code it is on, and that line
/// alone when GitHub gives no hunk; its comments stand under `github <login>:` (`github
/// <login> (pending):` for the viewer's own, not submitted yet); and its block ends with `on
/// GitHub: <its URL>`.
pub(crate) fn thread_block(thread: &ThreadDto) -> Vec<String> {
    let commented = match &thread.github {
        Some(_) => "as commented on GitHub".to_string(),
        None => format!("as commented in round {}", thread.anchor.original.round),
    };
    let mut lines = vec![thread_line(thread)];
    match (&thread.excerpt, &thread.github) {
        _ if thread.file_level => lines.push(file_level_line(thread)),
        (Some(excerpt), _) => {
            lines.extend(excerpt.split('\n').map(str::to_string));
            if thread.current_code.as_deref() != Some(thread.code.as_str()) {
                lines.push(format!("{commented}:"));
                lines.extend(commented_code(&thread.code));
            }
        }
        // Read from the diff hunk alone: the code as GitHub shows it.
        (None, Some(github)) if thread.hunk_only => {
            let why = if github.outdated {
                "outdated there"
            } else {
                "can't be placed here"
            };
            let rows = hunk_lines(thread, github);
            let ends = if rows.is_empty() { "" } else { ":" };
            lines.push(format!("{commented} ({why}){ends}"));
            lines.extend(rows);
        }
        (None, _) => {
            lines.push(format!("{commented} (not found in the file now):"));
            let (before, after) = match &thread.code_context {
                Some(context) => (context.before.as_slice(), context.after.as_slice()),
                None => (&[][..], &[][..]),
            };
            lines.extend(before.iter().map(|line| unnumbered(' ', line)));
            lines.extend(commented_code(&thread.code));
            lines.extend(after.iter().map(|line| unnumbered(' ', line)));
            if let Some(trail) = &thread.trail {
                lines.extend(trail_lines(trail));
            }
        }
    }
    let pending = |pending: bool| if pending { " (pending)" } else { "" };
    lines.push(match &thread.github {
        Some(github) => format!(
            "{} {}{}:",
            thread.author.kind,
            thread.author.name,
            pending(github.pending)
        ),
        None => format!(
            "{} {}, round {}:",
            thread.author.kind, thread.author.name, thread.anchor.original.round
        ),
    });
    lines.extend(indented(&thread.body));
    for reply in &thread.replies {
        lines.push(format!(
            "{} {}{}:",
            reply.author.kind,
            reply.author.name,
            pending(reply.pending)
        ));
        lines.extend(indented(&reply.body));
    }
    if let Some(note) = &thread.note {
        lines.push(format!("{} note:", thread.state));
        lines.extend(indented(note));
    }
    if let Some(url) = thread
        .github
        .as_ref()
        .and_then(|github| github.url.as_ref())
    {
        lines.push(format!("on GitHub: {url}"));
    }
    lines
}

/// The end of a GitHub thread's diff hunk, as GitHub shows it above the thread
/// (`dv_core::github::hunk_rows`): each row unnumbered, `>` on a commented line, then `+`,
/// `-` or a space as the hunk marks it; nothing for a thread GitHub gives no hunk.
fn hunk_lines(thread: &ThreadDto, github: &GithubDto) -> Vec<String> {
    let original = &thread.anchor.original;
    let side = match thread.side {
        "old" => AnchorSide::Old,
        _ => AnchorSide::New,
    };
    let count = original.end_line.saturating_sub(original.start_line) + 1;
    hunk_rows(&github.diff_hunk, side, count)
        .iter()
        .map(|row| {
            let mark = if row.commented { '>' } else { ' ' };
            let sign = match row.kind {
                LineKind::Added => '+',
                LineKind::Removed => '-',
                LineKind::Context => ' ',
            };
            code_row(&format!("{mark} {sign} |"), &row.text)
        })
        .collect()
}

/// What a comment on the whole file is about (PLAN §10.3): `on the whole file`; once its file
/// is gone from the review's changes, `on the whole file, which isn't among the review's
/// changes now`; once the file lacks its side, `on the whole file, which is deleted now` (`…
/// as it was, which is new now` on the old side).
fn file_level_line(thread: &ThreadDto) -> String {
    let line = if !thread.outdated && !thread.in_diff {
        "on the whole file, which the review doesn't change"
    } else if !thread.outdated {
        "on the whole file"
    } else if !thread.listed {
        "on the whole file, which isn't among the review's changes now"
    } else if thread.side == "old" {
        "on the whole file as it was, which is new now"
    } else {
        "on the whole file, which is deleted now"
    };
    line.to_string()
}

/// Where an outdated thread's code went (PLAN §10.3): `code changed since round N; these lines
/// replaced it (path:40-45):` and the excerpt around them, its `>` rows the replacing lines; or
/// `code changed since round N; it was removed: it stood before line 12 of path` (`at the end
/// of path` past its last line).
fn trail_lines(trail: &TrailDto) -> Vec<String> {
    let round = trail.round;
    match (trail.start_line, trail.end_line, &trail.excerpt) {
        (Some(start), Some(end), Some(excerpt)) => {
            let mut out = vec![format!(
                "code changed since round {round}; these lines replaced it ({}:{}):",
                trail.path,
                lines(start, end)
            )];
            out.extend(excerpt.split('\n').map(str::to_string));
            out
        }
        (Some(start), Some(end), None) => vec![format!(
            "code changed since round {round}; lines {} of {} replaced it",
            lines(start, end),
            trail.path
        )],
        _ => vec![match trail.before_line {
            Some(_) if trail.at_end => format!(
                "code changed since round {round}; it was removed: it stood at the end of {}",
                trail.path
            ),
            Some(before) => format!(
                "code changed since round {round}; it was removed: it stood before line {before} of {}",
                trail.path
            ),
            None => format!("code changed since round {round}; it was removed"),
        }],
    }
}

/// A line of code without a number: `> | text`, `  | text`; an empty one ends at `|`.
fn unnumbered(mark: char, line: &str) -> String {
    code_row(&format!("{mark} |"), line)
}

/// `code`, the lines a thread was written on, unnumbered and marked `>`; over
/// `dv_core::review::excerpt::RANGE_MAX` lines, its first and last `RANGE_KEEP` with `  ⋮ N
/// lines` between, the excerpt's rule for a long commented span (PLAN §10.3).
fn commented_code(code: &str) -> Vec<String> {
    let lines: Vec<&str> = code.split('\n').collect();
    let hidden = hidden_span(lines.len()).unwrap_or(lines.len()..lines.len());
    let mut rows: Vec<String> = lines[..hidden.start]
        .iter()
        .map(|line| unnumbered('>', line))
        .collect();
    if !hidden.is_empty() {
        rows.push(format!("  ⋮ {} lines", hidden.len()));
    }
    rows.extend(lines[hidden.end..].iter().map(|line| unnumbered('>', line)));
    rows
}

/// A closed round: `round 1 closed: request_changes`, then the reviewer's summary in full,
/// indented.
pub(crate) fn round_summary(round: &RoundDto) -> Vec<String> {
    let mut lines = vec![format!(
        "round {} closed: {}",
        round.n,
        round.verdict.unwrap_or("no verdict")
    )];
    if let Some(body) = &round.body {
        lines.extend(indented(body));
    }
    lines
}

/// A closed round on one line: `round 1 closed: request_changes: <the summary's first line>`.
pub(crate) fn round_line(round: &RoundDto) -> String {
    let summary = round
        .body
        .as_deref()
        .map(|body| format!(": {}", first_line(body)))
        .unwrap_or_default();
    format!(
        "round {} closed: {}{summary}",
        round.n,
        round.verdict.unwrap_or("no verdict")
    )
}

/// A thread's place: `src/a.ts:12`, `src/a.ts:12-14`, `src/a.ts:14-16 (moved from 12-14)`, or
/// `src/a.ts (outdated, was 12-14)`, `src/util.ts:12 (not in the diff)` for a thread on a file
/// the review doesn't change, with where its code went when its round tells
/// (`(outdated, was 12-14, replaced by 40-45)`, `(outdated, was 12-14, removed before 40)`,
/// `(outdated, was 12-14, removed at the end)`). A thread on the old side says so first,
/// `src/a.ts:12 (old)`, `src/a.ts:14 (old, moved from 12)`, so its lines aren't taken for the
/// file's lines as they are now; the GUI names it the same way (DESIGN §4.1). A comment on the
/// whole file is `src/a.ts (file)`, `(file, old)` on the old side, `(file, not in the diff)` on
/// a file the review doesn't change and `(file, outdated)` once its file is gone or deleted.
/// One of GitHub's threads without a place in the diff is `src/a.ts (outdated on GitHub, was
/// 12)` when GitHub calls it outdated, and `src/a.ts (can't be placed here, was 12)` when dv can't
/// tell which code it is on.
pub(crate) fn location(thread: &ThreadDto) -> String {
    if thread.file_level {
        let mut notes = vec!["file"];
        if thread.side == "old" {
            notes.push("old");
        }
        if thread.outdated {
            notes.push("outdated");
        } else if !thread.in_diff {
            notes.push("not in the diff");
        }
        return format!("{} ({})", thread.side_path, notes.join(", "));
    }
    let original = &thread.anchor.original;
    let was = lines(original.start_line, original.end_line);
    let mut notes: Vec<String> = Vec::new();
    if thread.side == "old" {
        notes.push("old".to_string());
    }
    let place = match &thread.anchor.current {
        Some(current) => {
            if (current.start_line, current.end_line) != (original.start_line, original.end_line) {
                notes.push(format!("moved from {was}"));
            }
            if !thread.in_diff {
                notes.push("not in the diff".to_string());
            }
            format!(
                "{}:{}",
                current.path,
                lines(current.start_line, current.end_line)
            )
        }
        None => {
            notes.push(match &thread.github {
                Some(github) if github.outdated => format!("outdated on GitHub, was {was}"),
                Some(_) if thread.hunk_only => format!("can't be placed here, was {was}"),
                _ => format!("outdated, was {was}"),
            });
            if let Some(trail) = &thread.trail {
                notes.push(
                    match (trail.start_line, trail.end_line, trail.before_line) {
                        (Some(start), Some(end), _) => format!("replaced by {}", lines(start, end)),
                        (_, _, Some(_)) if trail.at_end => "removed at the end".to_string(),
                        (_, _, Some(before)) => format!("removed before {before}"),
                        _ => "removed".to_string(),
                    },
                );
            }
            thread.path.clone()
        }
    };
    if notes.is_empty() {
        place
    } else {
        format!("{place} ({})", notes.join(", "))
    }
}

/// `12` or `12-14`.
pub(crate) fn lines(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

/// The first line of `text` that has anything on it, trimmed and cut at 100 characters.
pub(crate) fn first_line(text: &str) -> String {
    const MAX: usize = 100;
    let line = escape::breaks(text)
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    if line.chars().count() > MAX {
        let cut: String = line.chars().take(MAX - 1).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

/// `text`'s lines, each indented by [`BODY_INDENT`] and without whitespace at its end; a blank
/// line stays empty. (A Markdown hard break, two spaces at a line's end, reads as a line break
/// here anyway.) A line ends at `\n`, `\r\n` or a lone `\r` (`escape::breaks`), so nothing in
/// a body reaches column 0; what else would move the cursor is spelled out when the line is
/// printed (`output::text`).
pub(crate) fn indented(text: &str) -> Vec<String> {
    escape::breaks(text)
        .map(|line| {
            let line = line.trim_end();
            if line.is_empty() {
                String::new()
            } else {
                format!("{BODY_INDENT}{line}")
            }
        })
        .collect()
}
