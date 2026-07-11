//! Thin `imara_diff::TokenSource` wrapper for a pre-split slice of `&str`
//! tokens. Both the line-level diff (lines) and the intraline word diff
//! (word/space/punctuation runs) just need to hand imara-diff a `&[&str]`,
//! so one wrapper serves both.

use imara_diff::TokenSource;

pub(super) struct Tokens<'a>(pub &'a [&'a str]);

impl<'a> TokenSource for Tokens<'a> {
    type Token = &'a str;
    type Tokenizer = std::iter::Copied<std::slice::Iter<'a, &'a str>>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter().copied()
    }

    fn estimate_tokens(&self) -> u32 {
        self.0.len() as u32
    }
}
