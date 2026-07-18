//! Merge-conflict probing for the review navigator's conflict indicator
//! (docs/backlog.md "Merge-conflict indicator in the review navigator").
//!
//! Read-only: nothing here ever touches the working tree or the index.
//! Two independent probes feed [`ConflictProbe`], both wired up in
//! [`crate::git::GitRepo::conflict_probe`]:
//!
//! - **Range reviews** (`DiffSource::Range`): `git merge-tree --write-tree
//!   --name-only -z <base> <head>` (git >= 2.38) asks git to simulate the
//!   merge in memory. Exit 0 means clean; exit 1 with a tree-oid first
//!   field means conflicted, and the following NUL-terminated fields up to
//!   the first empty one are the conflicted paths — raw and unquoted
//!   thanks to `-z` (without it git C-quotes spaces/unicode, breaking path
//!   matching — capstone P3-4; verified against real git 2.53 output, see
//!   this module's tests for the exact byte shapes). Anything else — an
//!   old git rejecting a flag (a usage-error exit outside {0, 1}), or
//!   exit 1 with no leading oid (git's "not something we can merge" for
//!   an unresolvable rev) — is [`ConflictProbe::Unsupported`]: callers
//!   render no indicator at all rather than guessing.
//! - **WorkingTree/Staged reviews**: `git ls-files -u -z` — a non-empty
//!   result means the index already has unmerged (stage 1/2/3) entries,
//!   i.e. the user is mid-merge/rebase with unresolved conflicts left in
//!   place. Independent of `merge-tree`: this reads existing repo state,
//!   it doesn't simulate anything.

/// Which files a [`ConflictProbe::Determined`] found conflicted. Empty
/// means clean. Paths are repo-root-relative, forward-slash, sorted and
/// deduped — never persisted (this is always a freshly computed,
/// in-memory value; see docs/backlog.md's "ephemeral in the index/
/// workspace" scoping note).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ConflictInfo {
    pub files: Vec<String>,
}

impl ConflictInfo {
    pub fn is_conflicted(&self) -> bool {
        !self.files.is_empty()
    }

    fn from_unsorted(mut files: Vec<String>) -> Self {
        files.sort();
        files.dedup();
        Self { files }
    }
}

/// Outcome of a conflict probe. Deliberately NOT a plain `Option` or
/// `Result` — "couldn't tell" (old git, an unresolvable rev, a spawn
/// failure) is a real third state, distinct from both "clean" and
/// "conflicted", and callers must render it identically to "clean" (no
/// indicator), never as an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictProbe {
    Determined(ConflictInfo),
    Unsupported,
}

impl ConflictProbe {
    /// `Some(info)` when determined (whether or not `info.is_conflicted()`
    /// is true), `None` when [`ConflictProbe::Unsupported`].
    pub fn info(&self) -> Option<&ConflictInfo> {
        match self {
            ConflictProbe::Determined(info) => Some(info),
            ConflictProbe::Unsupported => None,
        }
    }

    pub fn is_conflicted(&self) -> bool {
        self.info().is_some_and(ConflictInfo::is_conflicted)
    }
}

/// Parse `git merge-tree --write-tree --name-only -z <base> <head>`'s
/// result. `-z` matters for correctness, not just framing (capstone P3-4):
/// without it git C-quotes any path with spaces/quotes/non-ASCII bytes
/// (`"\303\274nicode file.txt"` — verified against real git 2.53), so the
/// stored conflicted paths would never match dv's own unquoted
/// repo-relative paths. With `-z` every field is NUL-terminated and paths
/// arrive as raw, unquoted bytes: the layout is the tree oid, then each
/// conflicted filename, then an EMPTY field (the `\0\0` section
/// separator) followed by the NUL-framed informational messages, which
/// this parser stops at (also verified byte-for-byte against git 2.53).
///
/// `success` true (exit 0) is always clean — merge-tree's exit-0 output is
/// just the merged tree's oid, nothing else to read. A non-zero exit is
/// conflict data ONLY when it's exactly 1 (git's documented "merge
/// succeeded, with conflicts" code) AND stdout's first field looks like a
/// tree oid (hex, non-empty) — anything else on exit 1 is git's "not
/// something we can merge" error path (bad rev, e.g.), which prints
/// nothing to stdout. Any OTHER exit code (2 = merge couldn't happen at
/// all; a usage-error exit from an old git rejecting `--write-tree`/
/// `--name-only`/`-z`) is `Unsupported`.
pub(crate) fn parse_merge_tree_probe(
    success: bool,
    exit_code: Option<i32>,
    stdout: &[u8],
) -> ConflictProbe {
    if success {
        return ConflictProbe::Determined(ConflictInfo::default());
    }
    if exit_code != Some(1) {
        return ConflictProbe::Unsupported;
    }
    let text = String::from_utf8_lossy(stdout);
    let mut fields = text.split('\0');
    let Some(first) = fields.next() else {
        return ConflictProbe::Unsupported;
    };
    // Real `-z` output NUL-terminates the oid with no newline (verified);
    // tolerate one defensively rather than misread the whole shape.
    let first = first.trim_end_matches('\n');
    if first.is_empty() || !first.bytes().all(|b| b.is_ascii_hexdigit()) {
        return ConflictProbe::Unsupported;
    }
    let files = fields
        .take_while(|field| !field.is_empty())
        .map(str::to_string)
        .collect();
    ConflictProbe::Determined(ConflictInfo::from_unsorted(files))
}

/// Parse `git ls-files -u -z`'s result: `<mode> <sha> <stage>\t<path>\0`
/// repeated once per (stage, path) pair — one *file* can appear up to
/// three times (stages 1/2/3), so paths are deduped. A run failure (any
/// non-zero exit — `ls-files` doesn't fail for "clean index", only for
/// something being genuinely wrong, e.g. not a git repo, already ruled
/// out by the time this runs) is `Unsupported`, never misread as "clean".
pub(crate) fn parse_ls_files_u_probe(success: bool, stdout: &[u8]) -> ConflictProbe {
    if !success {
        return ConflictProbe::Unsupported;
    }
    let text = String::from_utf8_lossy(stdout);
    let files = text
        .split('\0')
        .filter(|token| !token.is_empty())
        .filter_map(|token| token.split_once('\t').map(|(_, path)| path.to_string()))
        .collect();
    ConflictProbe::Determined(ConflictInfo::from_unsorted(files))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ConflictInfo / ConflictProbe accessors --------------------------

    #[test]
    fn conflict_info_dedupes_and_sorts() {
        let info = ConflictInfo::from_unsorted(vec![
            "b.txt".to_string(),
            "a.txt".to_string(),
            "b.txt".to_string(),
        ]);
        assert_eq!(info.files, vec!["a.txt", "b.txt"]);
        assert!(info.is_conflicted());
    }

    #[test]
    fn empty_conflict_info_is_not_conflicted() {
        assert!(!ConflictInfo::default().is_conflicted());
    }

    #[test]
    fn probe_info_and_is_conflicted() {
        let clean = ConflictProbe::Determined(ConflictInfo::default());
        assert!(!clean.is_conflicted());
        assert!(clean.info().is_some());

        let dirty =
            ConflictProbe::Determined(ConflictInfo::from_unsorted(vec!["f.txt".to_string()]));
        assert!(dirty.is_conflicted());

        assert!(!ConflictProbe::Unsupported.is_conflicted());
        assert!(ConflictProbe::Unsupported.info().is_none());
    }

    // --- parse_merge_tree_probe -------------------------------------------

    #[test]
    fn merge_tree_clean_exit_zero_is_determined_empty() {
        // Real git 2.53 `-z` output on a clean merge: just the tree oid.
        let stdout = b"8e58d60c892ceec26013d14cd2ff7bf91ef23ae7\0";
        let probe = parse_merge_tree_probe(true, Some(0), stdout);
        assert_eq!(probe, ConflictProbe::Determined(ConflictInfo::default()));
    }

    #[test]
    fn merge_tree_single_file_conflict() {
        // Real git 2.53 `-z` output for one conflicted file
        // (--name-only -z): oid, filenames, then an EMPTY field (`\0\0`)
        // before the NUL-framed informational section.
        let stdout = b"27c1c1481bb758152909d2e8b223af292ca642cb\0\
f.txt\0\
\0\
1\0f.txt\0Auto-merging\0Auto-merging f.txt\n\0\
1\0f.txt\0CONFLICT (contents)\0CONFLICT (content): Merge conflict in f.txt\n\0";
        let probe = parse_merge_tree_probe(false, Some(1), stdout);
        assert_eq!(
            probe,
            ConflictProbe::Determined(ConflictInfo {
                files: vec!["f.txt".to_string()]
            })
        );
    }

    #[test]
    fn merge_tree_multi_file_conflict_with_unicode_and_spaces() {
        // Byte-for-byte the shape real git 2.53 emitted for a conflict in
        // `f.txt` + `ünïcode file.txt` (capstone P3-4's motivating case:
        // withOUT `-z` that second path arrives C-quoted as
        // `"\303\274n\303\257code file.txt"` and could never match dv's own
        // unquoted paths).
        let stdout = "f0965dd54b0b801b641c7ac12bf8d0e3a7b7f8b0\0\
f.txt\0\
\u{fc}n\u{ef}code file.txt\0\
\0\
1\0f.txt\0Auto-merging\0Auto-merging f.txt\n\0\
1\0f.txt\0CONFLICT (contents)\0CONFLICT (content): Merge conflict in f.txt\n\0\
1\0\u{fc}n\u{ef}code file.txt\0Auto-merging\0Auto-merging \u{fc}n\u{ef}code file.txt\n\0\
1\0\u{fc}n\u{ef}code file.txt\0CONFLICT (contents)\0CONFLICT (content): Merge conflict in \u{fc}n\u{ef}code file.txt\n\0";
        let probe = parse_merge_tree_probe(false, Some(1), stdout.as_bytes());
        assert_eq!(
            probe,
            ConflictProbe::Determined(ConflictInfo {
                files: vec![
                    "f.txt".to_string(),
                    "\u{fc}n\u{ef}code file.txt".to_string()
                ]
            })
        );
    }

    #[test]
    fn merge_tree_bad_rev_exit_one_with_empty_stdout_is_unsupported() {
        // Real git 2.53 behavior for an unresolvable rev: exit 1, but the
        // error text ("not something we can merge") goes to stderr, not
        // stdout — no leading oid to key off, so this must NOT be
        // misread as a conflict.
        let probe = parse_merge_tree_probe(false, Some(1), b"");
        assert_eq!(probe, ConflictProbe::Unsupported);
    }

    #[test]
    fn merge_tree_unknown_flag_usage_error_is_unsupported() {
        // Real git behavior for an old git rejecting --write-tree/
        // --name-only: a usage-error exit (129 observed), not 0 or 1.
        let probe = parse_merge_tree_probe(false, Some(129), b"");
        assert_eq!(probe, ConflictProbe::Unsupported);
    }

    #[test]
    fn merge_tree_exit_two_merge_could_not_happen_is_unsupported() {
        let probe = parse_merge_tree_probe(false, Some(2), b"");
        assert_eq!(probe, ConflictProbe::Unsupported);
    }

    #[test]
    fn merge_tree_signal_terminated_none_exit_code_is_unsupported() {
        let probe = parse_merge_tree_probe(false, None, b"");
        assert_eq!(probe, ConflictProbe::Unsupported);
    }

    #[test]
    fn merge_tree_garbage_stdout_on_exit_one_is_unsupported() {
        // Defensive: exit 1 but the first line isn't hex at all (shouldn't
        // happen against real git, but a corrupt/mocked output must never
        // be misparsed into a false conflict).
        let probe = parse_merge_tree_probe(false, Some(1), b"not an oid\nfile.txt\n");
        assert_eq!(probe, ConflictProbe::Unsupported);
    }

    // --- parse_ls_files_u_probe -------------------------------------------

    #[test]
    fn ls_files_u_empty_output_is_clean() {
        let probe = parse_ls_files_u_probe(true, b"");
        assert_eq!(probe, ConflictProbe::Determined(ConflictInfo::default()));
    }

    #[test]
    fn ls_files_u_dedupes_multi_stage_entries() {
        // Real `git ls-files -u -z` output shape: one line per stage, up
        // to three stages for a two-sided content conflict, one for an
        // add/add conflict missing the common-ancestor stage.
        let stdout = b"100644 83db48f84ec878fbfb30b46d16630e944e34f205 1\tf.txt\0\
100644 bb724db63e8e2867e1a090e4df6b92f628af3206 2\tf.txt\0\
100644 745659694b768078ecd38f4380c333de3711d470 3\tf.txt\0\
100644 43d5a8ed6ef6c00ff775008633f95787d088285d 2\tg.txt\0\
100644 ba629238ca89489f2b350e196ca445e09d8bb834 3\tg.txt\0";
        let probe = parse_ls_files_u_probe(true, stdout);
        assert_eq!(
            probe,
            ConflictProbe::Determined(ConflictInfo {
                files: vec!["f.txt".to_string(), "g.txt".to_string()]
            })
        );
    }

    #[test]
    fn ls_files_u_command_failure_is_unsupported() {
        let probe = parse_ls_files_u_probe(false, b"");
        assert_eq!(probe, ConflictProbe::Unsupported);
    }
}
