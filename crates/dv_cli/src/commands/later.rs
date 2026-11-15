//! Commands this version recognises but doesn't run (docs/PLAN.md §16): each is refused with
//! exit 2 and what to do instead.

use crate::error::CliError;

/// `dv review submit` on a PR review, between phase 8's two cuts (PLAN §9.12): the review
/// takes the reviewer's comments, and posting them is the write side.
pub(crate) fn pr_submit() -> CliError {
    CliError::unsupported("Submitting to GitHub arrives in a later version")
}
