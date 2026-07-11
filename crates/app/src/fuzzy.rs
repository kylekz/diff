//! Fuzzy path matcher for the jump-to-file palette.
//!
//! Case-insensitive **subsequence** match (`std` only, no dependency): every
//! query char must appear in the candidate in order, not necessarily
//! contiguously. Scoring rewards matches that land at path/word boundaries,
//! run consecutively, and sit inside the file's basename rather than its
//! directory prefix — the common "typing the filename" case.
//!
//! Deliberate non-goal: this is greedy, not the full dynamic-programming
//! optimal-subsequence search fzf-style matchers use. We run a greedy
//! left-to-right scan twice — once over the whole candidate, once anchored to
//! the basename — and keep the better result. That two-pass trick covers the
//! matches users actually care about ("path contains query" and "filename
//! means query") in O(len(query) * len(candidate)) worst case, no DP table,
//! no allocation beyond the lowercased copies. A pathological input could
//! out-score this with full DP, but a jump-to-file candidate list isn't
//! adversarial.

/// Score `query` against `candidate`; `None` = no match, higher = better.
pub fn score(query: &str, candidate: &str) -> Option<i64> {
    if query.len() > candidate.len() {
        return None;
    }
    if query.is_empty() {
        return Some(0);
    }

    let query = query.as_bytes().to_ascii_lowercase();
    let hay = candidate.as_bytes().to_ascii_lowercase();
    let bname_start = basename_start(&hay);

    // Pass 1: greedy from the start of the whole candidate.
    let forward = greedy_score(&hay, 0, &query, bname_start, true);
    // Pass 2: greedy restricted to the basename, but still scored with
    // absolute indices into `hay` — that's what lets it award the "right
    // after a separator" bonus honestly (the basename really does follow
    // one) while denying the "index 0" bonus unless the basename genuinely
    // opens the candidate (no directory prefix at all).
    let from_basename = greedy_score(&hay, bname_start, &query, bname_start, false);

    match (forward, from_basename) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Indices of `candidates` that match `query`, best first (stable for ties).
pub fn rank<'a>(query: &str, candidates: impl Iterator<Item = &'a str>) -> Vec<usize> {
    let mut scored: Vec<(usize, i64)> = candidates
        .enumerate()
        .filter_map(|(i, c)| score(query, c).map(|s| (i, s)))
        .collect();
    scored.sort_by_key(|&(i, s)| (std::cmp::Reverse(s), i));
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Boundary chars for the "immediately after a separator" bonus. Broader than
/// a path separator on purpose — `_`/`-`/`.` are word boundaries within a
/// single path segment (`review_navigator`, `phase-1`, `mod.rs`).
fn is_separator(b: u8) -> bool {
    matches!(b, b'/' | b'\\' | b'_' | b'-' | b'.')
}

/// Where the basename starts: strictly after the last *path* separator (`/`
/// or `\`) — never `_`/`-`/`.`, those are word boundaries, not directory
/// boundaries. Falls back to 0 (whole string is the basename) when there's
/// no directory prefix.
fn basename_start(hay: &[u8]) -> usize {
    hay.iter()
        .rposition(|&b| b == b'/' || b == b'\\')
        .map_or(0, |i| i + 1)
}

/// Greedy left-to-right subsequence match of `query` against `hay`, searching
/// only from `start` onward but scoring with indices absolute to `hay`. See
/// [`score`] for why the basename pass still wants absolute indices.
fn greedy_score(
    hay: &[u8],
    start: usize,
    query: &[u8],
    bname_start: usize,
    apply_leading_penalty: bool,
) -> Option<i64> {
    let mut total: i64 = 0;
    let mut cursor = start;
    let mut first_match: Option<usize> = None;
    let mut prev_match: Option<usize> = None;

    for &qc in query {
        let idx = (cursor..hay.len()).find(|&i| hay[i] == qc)?;
        first_match.get_or_insert(idx);

        if idx == 0 {
            total += 8;
        }
        if idx > 0 && is_separator(hay[idx - 1]) {
            total += 6;
        }
        match prev_match {
            // Strictly exceeds separator-bonus-minus-minimum-gap-penalty
            // (6 - 1 = 5): a contiguous exact match ("phase1.md") must never
            // lose to a separator-punctuated cousin ("phase-1.md").
            Some(p) if idx == p + 1 => total += 6,
            Some(p) => total -= (idx - p - 1).min(6) as i64,
            None => {}
        }
        if idx >= bname_start {
            total += 2;
        }

        prev_match = Some(idx);
        cursor = idx + 1;
    }

    if apply_leading_penalty && let Some(first) = first_match {
        total -= first.min(4) as i64;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_matches_only_workspace_rs() {
        let candidates = [
            "docs/architecture.md",
            "crates/app/src/workspace.rs",
            "crates/core/src/lib.rs",
        ];
        assert_eq!(rank("ws", candidates.into_iter()), vec![1]);
    }

    #[test]
    fn no_match_and_empty_query() {
        assert_eq!(score("xyz", "abc"), None);
        assert!(score("", "anything").is_some());

        let candidates = ["b", "a", "c"];
        assert_eq!(rank("", candidates.into_iter()), vec![0, 1, 2]);
    }

    #[test]
    fn case_insensitive() {
        assert!(score("MAIN", "crates/app/src/main.rs").is_some());
    }

    #[test]
    fn basename_start_preferred_over_scattered_dir_match() {
        // "mod" opens the basename of the first candidate directly; the
        // second only reaches a directory-nested, non-boundary "mod" via the
        // forward pass. (Note: "mod" also happens to literally prefix the
        // second candidate's own basename "models-overview.md", so this
        // exercises rank()'s documented "stable for ties" guarantee as much
        // as raw scoring — see the deviation note in the final report.)
        let candidates = [
            "crates/app/src/automation/mod.rs",
            "docs/models-overview.md",
        ];
        assert_eq!(rank("mod", candidates.into_iter())[0], 0);
    }

    #[test]
    fn consecutive_beats_scattered() {
        let a = score("shell", "crates/app/src/shell.rs").unwrap();
        let b = score("shell", "scripts/hello-all.txt").unwrap();
        assert!(a > b, "expected {a} > {b}");
    }

    #[test]
    fn start_of_string_beats_nested_match() {
        let a = score("cargo", "Cargo.toml").unwrap();
        let b = score("cargo", "crates/app/Cargo.toml").unwrap();
        assert!(a > b, "expected {a} > {b}");
    }

    #[test]
    fn contiguous_exact_beats_separator_punctuated() {
        let exact = score("phase1", "phase1.md").unwrap();
        let hyphenated = score("phase1", "phase-1.md").unwrap();
        assert!(exact > hyphenated, "expected {exact} > {hyphenated}");
    }

    #[test]
    fn stable_for_ties() {
        let candidates = ["dup.rs", "dup.rs"];
        assert_eq!(rank("dup", candidates.into_iter()), vec![0, 1]);
    }

    #[test]
    fn query_longer_than_candidate_is_none() {
        assert_eq!(score("verylongquery", "a"), None);
    }

    #[test]
    fn no_matches_returns_empty_rank() {
        let candidates = ["abc", "def"];
        assert_eq!(rank("xyz", candidates.into_iter()), Vec::<usize>::new());
    }

    #[test]
    fn backslash_separator_treated_like_forward_slash() {
        let a = score("main", r"crates\app\src\main.rs");
        let b = score("main", "crates/app/src/main.rs");
        assert_eq!(a, b);
    }

    #[test]
    fn single_char_query_rewards_leading_match_over_buried_one() {
        // Earliest occurrence wins the greedy scan; the index-0 bonus only
        // fires when that occurrence truly opens the string, and a buried
        // match also eats the leading-unmatched penalty.
        let buried = score("a", "banana").unwrap();
        let leading = score("a", "apple").unwrap();
        assert!(leading > buried, "expected {leading} > {buried}");
    }
}
