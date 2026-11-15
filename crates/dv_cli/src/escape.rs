//! Text dv didn't write, made safe to print as text (docs/PLAN.md §10.3): a file's path, a
//! login or an agent's name, a model's name, a comment's body, a row of code or of GitHub's
//! diff hunk, a URL. A newline in a path, a lone `\r` or a terminal escape in a comment would
//! otherwise start a line at column 0 that reads as dv's own, or redraw the terminal; a
//! bidirectional override would show a line in another order than it is written, and a tag
//! character hide text in it.
//!
//! The text output escapes every line it prints here ([`line`], through `output::text`), so no
//! command can forget to, and so does what goes to stderr; a body is first split at its line
//! breaks ([`breaks`]) so each of its lines gets the body's indent. `--json` never passes
//! through: JSON escapes for itself. The app's "Copy as prompt" writes its lines the same way.
//!
//! A backslash is not escaped: a Windows path stays as it is, and a name that holds a
//! backslash and an `n` reads the same as one that holds a line break. What matters is that
//! neither starts a line.

use std::borrow::Cow;
use std::fmt::Write as _;

use dv_core::text::is_unprintable;

/// `text` as one printed line: every character that moves, redraws, reorders or hides text
/// (`dv_core::text::is_unprintable`) but the tab, spelled out (`\n`, `\r`, `\x1b`, `\u{85}`,
/// `\u{2028}`, `\u{202e}`), so the line stays one line and draws nothing but its
/// characters.
pub fn line(text: &str) -> Cow<'_, str> {
    let unsafe_char = |c: char| c != '\t' && is_unprintable(c);
    if !text.contains(unsafe_char) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            c if !unsafe_char(c) => out.push(c),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            // Writing to a `String` can't fail.
            c if c.is_ascii() => {
                let _ = write!(out, "\\x{:02x}", u32::from(c));
            }
            c => {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
        }
    }
    Cow::Owned(out)
}

/// `text`'s lines as a body has them: `\n`, `\r\n` and a lone `\r` each end one, since a
/// terminal moves to column 0 at any of them; no empty line after a break that ends the text.
pub fn breaks(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = (!text.is_empty()).then_some(text);
    std::iter::from_fn(move || {
        let text = rest?;
        let Some(at) = text.find(['\n', '\r']) else {
            rest = None;
            return Some(text);
        };
        let after = &text[at..];
        let after = after.strip_prefix("\r\n").unwrap_or(&after[1..]);
        rest = (!after.is_empty()).then_some(after);
        Some(&text[..at])
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_line_spells_out_what_would_break_or_redraw_it() {
        assert_eq!(line("src/a.rs"), "src/a.rs");
        assert_eq!(line("a\tb"), "a\tb");
        assert_eq!(line("a\nt-1  open  b"), "a\\nt-1  open  b");
        assert_eq!(line("a\r\x1b[2Jb\x7f\0"), "a\\r\\x1b[2Jb\\x7f\\x00");
        assert_eq!(
            line("a\u{85}b\u{9b}c\u{2028}d\u{2029}"),
            "a\\u{85}b\\u{9b}c\\u{2028}d\\u{2029}"
        );
        assert_eq!(line("naïve — 日本語"), "naïve — 日本語");
    }

    #[test]
    fn what_reorders_or_hides_text_is_spelled_out_and_what_scripts_need_is_not() {
        assert_eq!(
            line("a\u{202e}b\u{2066}c\u{2069}\u{e0041}\u{e007f}"),
            "a\\u{202e}b\\u{2066}c\\u{2069}\\u{e0041}\\u{e007f}"
        );
        let kept = "\u{200e}\u{200f}a\u{200d}b\u{200c}c\u{fe0f}";
        assert_eq!(line(kept), kept);
        // A backslash stands as it is.
        assert_eq!(line("C:\\new\\repo"), "C:\\new\\repo");
    }

    #[test]
    fn a_body_breaks_wherever_a_terminal_returns_to_column_zero() {
        let split = |text| breaks(text).collect::<Vec<_>>();
        assert_eq!(split(""), Vec::<&str>::new());
        assert_eq!(split("one"), ["one"]);
        assert_eq!(split("one\n"), ["one"]);
        assert_eq!(
            split("one\ntwo\r\nthree\rfour"),
            ["one", "two", "three", "four"]
        );
        assert_eq!(split("one\n\ntwo\r\r\n"), ["one", "", "two", ""]);
        assert_eq!(split("\u{2028}"), ["\u{2028}"]);
    }
}
