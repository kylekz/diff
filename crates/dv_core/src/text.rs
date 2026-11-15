//! Characters in text dv didn't write (a path, a name, a comment, a URL: docs/PLAN.md §10.3,
//! §9.6) that would make it read as something else where dv prints it as text.

/// Whether `c` moves the cursor, redraws, reorders or hides what is shown where text is
/// printed: a C0 or C1 control character (a line break, ESC, DEL; the tab among them), a
/// Unicode line or paragraph separator, a bidirectional embedding, override or isolate
/// (U+202A–U+202E, U+2066–U+2069), which can show a line in another order than it is
/// written, or a tag character (U+E0000–U+E007F), which shows as nothing. The marks that
/// scripts need to be written at all (LRM and RLM, the joiners, the variation selectors)
/// are not among them. The flags of England, Scotland and Wales are a black flag followed
/// by tag characters, the only emoji built from them: they print as the flag and its tags
/// spelled out, since a tag that spells a flag can't be told from one that hides text.
pub fn is_unprintable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

#[cfg(test)]
mod tests {
    use super::is_unprintable;

    #[test]
    fn what_moves_reorders_or_hides_text_is_unprintable() {
        for c in [
            '\n',
            '\r',
            '\t',
            '\0',
            '\x1b',
            '\x7f',
            '\u{85}',
            '\u{9b}',
            '\u{2028}',
            '\u{2029}',
            '\u{202a}',
            '\u{202e}',
            '\u{2066}',
            '\u{2069}',
            '\u{e0000}',
            '\u{e0041}',
            '\u{e007f}',
        ] {
            assert!(is_unprintable(c), "{c:?}");
        }
        for c in [
            ' ',
            'a',
            'é',
            '日',
            '\u{200e}',
            '\u{200f}',
            '\u{200d}',
            '\u{200c}',
            '\u{fe0f}',
            '\u{2027}',
            '\u{202f}',
            '\u{2065}',
            '\u{206a}',
            '\u{e0080}',
            '\u{fffd}',
        ] {
            assert!(!is_unprintable(c), "{c:?}");
        }
    }
}
