//! Format a review's comment threads as one agent-ready prompt block
//! (R3 item 3). Pure formatting: the GUI's "Copy as prompt" pushes the
//! result onto the clipboard; nothing here touches the store, the
//! clipboard, or the network.
//!
//! Shape (with dv's line ranges/authors/status):
//!
//! ```text
//! Review of <source>. Locations are GitHub diff-style: path:Rline for
//! right/new, path:Lline for left/old.
//!
//! src/lib.rs:R7 (kyle)
//! why remove this?
//! Reply 1 (agent)
//! because this path handles nil
//! =====
//! src/main.rs:L12-14 (kyle)
//! second thread
//! ```

use crate::git::DiffSource;

use super::{Comment, CommentStatus, Review, Side};

/// Render `review` as a prompt. `include_resolved: false` keeps only open
/// threads — the "what does the reviewer still want from me" view, matching
/// the CLI's canonical `dv comment list --status open`; `true` includes
/// resolved threads too, each marked `(resolved)` on its anchor line.
pub fn format_review_as_prompt(review: &Review, include_resolved: bool) -> String {
    let threads: Vec<String> = review
        .comments
        .iter()
        .filter(|c| include_resolved || c.status == CommentStatus::Open)
        .map(format_thread)
        .collect();
    let body = if threads.is_empty() {
        "(no open comments)".to_string()
    } else {
        threads.join("\n=====\n")
    };
    format!(
        "Review of {}. Locations are GitHub diff-style: path:Rline for right/new, path:Lline for left/old.\n\n{}",
        source_desc(&review.source),
        body
    )
}

/// One thread: anchor line (`path:R5` or `path:L5-9`, author, resolved
/// marker), root body, then each reply under a `Reply N (author)` header.
fn format_thread(comment: &Comment) -> String {
    let side = match comment.side {
        Side::New => 'R',
        Side::Old => 'L',
    };
    let mut anchor = format!("{}:{}{}", comment.path, side, comment.start_line);
    if comment.end_line > comment.start_line {
        anchor.push_str(&format!("-{}", comment.end_line));
    }
    anchor.push_str(&format!(" ({})", comment.author));
    if comment.status == CommentStatus::Resolved {
        anchor.push_str(" (resolved)");
    }

    let mut out = format!("{anchor}\n{}", comment.body.trim_end());
    for (i, reply) in comment.replies.iter().enumerate() {
        out.push_str(&format!(
            "\nReply {} ({})\n{}",
            i + 1,
            reply.author,
            reply.body.trim_end()
        ));
    }
    out
}

/// Human-readable diff-source line for the prompt header — same wording
/// family as the sidebar's `entry_title`, spelled out here so the prompt
/// stays self-describing without a repo location in hand.
fn source_desc(source: &DiffSource) -> String {
    match source {
        DiffSource::WorkingTree => "the working tree vs HEAD".to_string(),
        DiffSource::Staged => "the staged index vs HEAD".to_string(),
        DiffSource::Commit(rev) => format!("commit {rev}"),
        DiffSource::Range {
            base,
            head,
            merge_base,
        } => {
            let sep = if *merge_base { "..." } else { ".." };
            format!("range {base}{sep}{head}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Side;
    use super::*;

    fn sample_review() -> Review {
        let mut review = Review::new_draft(DiffSource::WorkingTree);
        review
            .add_comment(
                "src/lib.rs",
                Side::New,
                7,
                7,
                None,
                "why remove this?",
                "kyle",
            )
            .unwrap();
        let root_id = review.comments[0].id.clone();
        review
            .reply(&root_id, "because this path handles nil", "agent")
            .unwrap();
        review
            .add_comment(
                "src/main.rs",
                Side::Old,
                12,
                14,
                None,
                "second thread",
                "kyle",
            )
            .unwrap();
        review
    }

    #[test]
    fn formats_threads_with_anchors_replies_and_separators() {
        let review = sample_review();
        let prompt = format_review_as_prompt(&review, false);
        assert_eq!(
            prompt,
            "Review of the working tree vs HEAD. Locations are GitHub diff-style: \
             path:Rline for right/new, path:Lline for left/old.\n\
             \n\
             src/lib.rs:R7 (kyle)\n\
             why remove this?\n\
             Reply 1 (agent)\n\
             because this path handles nil\n\
             =====\n\
             src/main.rs:L12-14 (kyle)\n\
             second thread"
        );
    }

    #[test]
    fn resolved_threads_are_skipped_unless_included() {
        let mut review = sample_review();
        let first_id = review.comments[0].id.clone();
        review
            .set_status(&first_id, CommentStatus::Resolved)
            .unwrap();

        let open_only = format_review_as_prompt(&review, false);
        assert!(!open_only.contains("src/lib.rs:R7"));
        assert!(open_only.contains("src/main.rs:L12-14"));

        let with_resolved = format_review_as_prompt(&review, true);
        assert!(with_resolved.contains("src/lib.rs:R7 (kyle) (resolved)"));
        assert!(with_resolved.contains("src/main.rs:L12-14"));
    }

    #[test]
    fn empty_review_says_so_instead_of_a_bare_header() {
        let review = Review::new_draft(DiffSource::Range {
            base: "main".to_string(),
            head: "feature".to_string(),
            merge_base: true,
        });
        let prompt = format_review_as_prompt(&review, false);
        assert!(prompt.starts_with("Review of range main...feature."));
        assert!(prompt.ends_with("(no open comments)"));
    }
}
