//! Keystroke and modifier parsing for the `key`, `keys`, `type` and pointer
//! commands.

use anyhow::{Context as _, Result, bail};
use gpui::{App, Keystroke, Modifiers, OperatingSystem};

/**
 * The OS that `secondary-` resolves for: gpui-pre 0.3.11 parses a keystroke for a named OS (cmd on
 * the Mac, ctrl elsewhere), and scenarios run on the machine the app runs on.
 */
const HOST_OS: OperatingSystem = if cfg!(target_os = "macos") {
    OperatingSystem::Mac
} else if cfg!(target_os = "linux") {
    OperatingSystem::Linux
} else {
    OperatingSystem::Windows
};

/// Parse one keystroke in gpui's keymap syntax (`"ctrl-shift-k"`, `"enter"`,
/// `"A"` which means shift-a).
pub fn parse_keystroke(source: &str) -> Result<Keystroke> {
    let source = source.trim();
    if source.is_empty() {
        bail!("empty keystroke");
    }
    Keystroke::parse_with_os(source, HOST_OS)
        .map_err(|err| anyhow::anyhow!("bad keystroke {source:?}: {err}"))
}

/// `keystroke` as the OS's own key event would carry it. On Windows a shifted
/// digit or punctuation key arrives as its shifted character without shift
/// (`shift-[` is `{` on a US layout), and gpui maps key bindings the same way
/// through the keyboard mapper, so the keystroke goes through that mapper with
/// the current layout too; gpui then gives it its `key_char`. The Mac's key
/// events keep the unshifted key and shift, as parsed.
pub fn as_typed(keystroke: Keystroke, cx: &App) -> Keystroke {
    if HOST_OS == OperatingSystem::Windows {
        cx.keyboard_mapper()
            .map_key_equivalent(keystroke, false)
            .inner()
            .clone()
    } else {
        keystroke
    }
}

/// Parse a whitespace-separated keystroke sequence such as
/// `"ctrl-k down enter"`. Every keystroke is validated before any is sent,
/// so a typo late in the sequence can't leave the app half-driven.
pub fn parse_keystroke_sequence(source: &str) -> Result<Vec<Keystroke>> {
    let keystrokes = source
        .split_whitespace()
        .enumerate()
        .map(|(index, part)| {
            parse_keystroke(part).with_context(|| format!("keystroke {} of {source:?}", index + 1))
        })
        .collect::<Result<Vec<_>>>()?;
    if keystrokes.is_empty() {
        bail!("empty keystroke sequence");
    }
    Ok(keystrokes)
}

/// The keystrokes a user would produce typing `text`. Printable characters
/// carry their `key_char` so an unbound key falls through to the focused
/// input handler exactly as real typing does; `\n` and `\t` become `enter`
/// and `tab`, and `\r` is dropped so CRLF text types one newline.
pub fn text_keystrokes(text: &str) -> Vec<Keystroke> {
    text.chars()
        .filter(|&c| c != '\r')
        .map(|c| match c {
            '\n' => named("enter", Some("\n")),
            '\t' => named("tab", Some("\t")),
            ' ' => named("space", Some(" ")),
            c if c.is_ascii_uppercase() => Keystroke {
                modifiers: Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                key: c.to_ascii_lowercase().to_string(),
                key_char: Some(c.to_string()),
            },
            c => Keystroke {
                modifiers: Modifiers::default(),
                key: c.to_string(),
                key_char: Some(c.to_string()),
            },
        })
        .collect()
}

fn named(key: &str, key_char: Option<&str>) -> Keystroke {
    Keystroke {
        modifiers: Modifiers::default(),
        key: key.into(),
        key_char: key_char.map(Into::into),
    }
}

/// Parse a dash- or plus-separated modifier spec (`"ctrl"`, `"ctrl-shift"`,
/// `"cmd+alt"`). Names follow what the modifier means rather than which key
/// produces it on this OS: `ctrl`/`control`, `alt`/`option`, `shift`,
/// `cmd`/`platform`/`super`/`win`, `fn`/`function`, and `secondary` (as in
/// gpui's keymap: `cmd` on macOS, `ctrl` elsewhere). Unknown names are an
/// error, never a silent no-op.
pub fn parse_modifiers(spec: &str) -> Result<Modifiers> {
    let mut modifiers = Modifiers::default();
    for token in spec.split(['-', '+']) {
        match token.trim().to_ascii_lowercase().as_str() {
            "" => {}
            "secondary" if cfg!(target_os = "macos") => modifiers.platform = true,
            "secondary" => modifiers.control = true,
            "ctrl" | "control" => modifiers.control = true,
            "alt" | "option" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            "cmd" | "platform" | "super" | "win" => modifiers.platform = true,
            "fn" | "function" => modifiers.function = true,
            other => bail!("unknown modifier: {other}"),
        }
    }
    Ok(modifiers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn sequence_splits_on_whitespace() {
        let keys = parse_keystroke_sequence("ctrl-k  down\tenter").unwrap();
        let unparsed: Vec<String> = keys.iter().map(|key| key.unparse(HOST_OS)).collect();
        assert_eq!(unparsed, ["ctrl-k", "down", "enter"]);
        assert!(keys[0].modifiers.control);
        assert_eq!(keys[0].key, "k");
    }

    #[test]
    fn sequence_supports_chords_and_uppercase() {
        let keys = parse_keystroke_sequence("ctrl-k ctrl-r shift-tab A").unwrap();
        assert_eq!(keys.len(), 4);
        assert!(keys[2].modifiers.shift);
        assert_eq!(keys[2].key, "tab");
        assert!(keys[3].modifiers.shift, "an uppercase letter implies shift");
        assert_eq!(keys[3].key, "a");
    }

    #[test]
    fn sequence_rejects_empty_and_invalid_input() {
        assert!(parse_keystroke_sequence("   ").is_err());
        let err = parse_keystroke_sequence("down ctrl-k-x enter").unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("keystroke 2"), "{message}");
    }

    #[test]
    fn single_keystroke_parses() {
        assert_eq!(parse_keystroke("ctrl-,").unwrap().key, ",");
        assert_eq!(parse_keystroke(" escape ").unwrap().key, "escape");
        assert!(parse_keystroke("").is_err());
    }

    #[test]
    fn text_becomes_character_keystrokes() {
        let keys = text_keystrokes("Hi, you!\r\n\t");
        let summary: Vec<(String, Option<String>, bool)> = keys
            .iter()
            .map(|k| (k.key.clone(), k.key_char.clone(), k.modifiers.shift))
            .collect();
        let expected = [
            ("h", Some("H"), true),
            ("i", Some("i"), false),
            (",", Some(","), false),
            ("space", Some(" "), false),
            ("y", Some("y"), false),
            ("o", Some("o"), false),
            ("u", Some("u"), false),
            ("!", Some("!"), false),
            ("enter", Some("\n"), false),
            ("tab", Some("\t"), false),
        ]
        .map(|(key, key_char, shift)| (key.to_string(), key_char.map(String::from), shift));
        assert_eq!(summary, expected);
    }

    #[test]
    fn text_keeps_non_ascii_characters() {
        let keys = text_keystrokes("é字");
        assert_eq!(keys[0].key_char.as_deref(), Some("é"));
        assert_eq!(keys[1].key, "字");
        assert!(!keys[1].modifiers.shift);
    }

    #[test]
    fn modifiers_parse_aliases_and_combinations() {
        let m = parse_modifiers("ctrl-shift").unwrap();
        assert!(m.control && m.shift && !m.alt && !m.platform);
        assert!(parse_modifiers("cmd").unwrap().platform);
        assert!(parse_modifiers("win").unwrap().platform);
        assert!(parse_modifiers("Option+fn").unwrap().alt);
        assert!(parse_modifiers("option+fn").unwrap().function);
        assert_eq!(parse_modifiers("").unwrap(), Modifiers::default());
        // The keymap's platform modifier, as `secondary-k` is in a keystroke.
        assert_eq!(
            parse_modifiers("secondary").unwrap(),
            parse_keystroke("secondary-k").unwrap().modifiers
        );
        let secondary = parse_modifiers("secondary").unwrap();
        assert!(if HOST_OS == OperatingSystem::Mac {
            secondary.platform
        } else {
            secondary.control
        });
        let err = parse_modifiers("meta").unwrap_err().to_string();
        assert!(err.contains("unknown modifier"), "{err}");
    }
}
