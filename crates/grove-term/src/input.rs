//! Translating GPUI keystrokes into libghostty key events.

use gpui_kit::Keystroke;
use libghostty_vt::key::{Key, Mods};

/// A keystroke in the terms libghostty's key encoder wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyInput<'a> {
    pub key: Key,
    /// The character the key produces with no modifiers (US layout), or
    /// `'\0'` for keys without one (arrows, function keys…).
    pub unshifted: char,
    pub mods: Mods,
    /// Modifiers already applied to `text` (e.g. Shift in `A`).
    pub consumed: Mods,
    /// Printable text the key types. Never contains control characters.
    pub text: Option<&'a str>,
}

pub(crate) fn map_keystroke(keystroke: &Keystroke, option_as_alt: bool) -> KeyInput<'_> {
    let m = keystroke.modifiers;
    let mut mods = Mods::empty();
    mods.set(Mods::SHIFT, m.shift);
    mods.set(Mods::ALT, m.alt);
    mods.set(Mods::CTRL, m.control);
    mods.set(Mods::SUPER, m.platform);

    let (key, unshifted, implied_shift) = physical_key(&keystroke.key);
    if implied_shift {
        // GPUI reports shift-1 as "!" without Shift.
        mods |= Mods::SHIFT;
    }

    let text = if option_as_alt && m.alt && !m.control && !m.platform {
        // macOS gives the Option-modified character (e.g. "ç"); as Alt we
        // want the plain one and let the encoder add the ESC prefix.
        alt_text(&keystroke.key, m.shift)
    } else {
        keystroke.key_char.as_deref()
    }
    .filter(|t| is_printable(t));

    let mut consumed = Mods::empty();
    if text.is_some() {
        consumed.set(Mods::SHIFT, mods.contains(Mods::SHIFT));
        consumed.set(Mods::ALT, m.alt && !option_as_alt);
    }

    KeyInput {
        key,
        unshifted,
        mods,
        consumed,
        text,
    }
}

/// macOS text-editing shortcuts that shells understand as readline keys,
/// as in Ghostty's defaults: Option-←/→ move by word, Cmd-←/→ go to the
/// start/end of the line, Cmd-Backspace deletes to the start of the line.
pub(crate) fn text_editing_shortcut(keystroke: &Keystroke) -> Option<&'static [u8]> {
    let m = keystroke.modifiers;
    if m.shift || m.control || m.function || (m.alt == m.platform) {
        return None;
    }
    Some(match (keystroke.key.as_str(), m.alt) {
        ("left", true) => b"\x1bb",
        ("right", true) => b"\x1bf",
        ("left", false) => b"\x01",
        ("right", false) => b"\x05",
        ("backspace", false) => b"\x15",
        _ => return None,
    })
}

/// The character a key types without Option.
fn alt_text(key: &str, shift: bool) -> Option<&str> {
    const UPPER: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let c = single_char(key)?;
    if shift && c.is_ascii_lowercase() {
        let i = usize::from(c as u8 - b'a');
        return Some(&UPPER[i..=i]);
    }
    Some(key)
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// Text the encoder may attach: no C0/DEL controls and no macOS function-key
/// private-use characters.
fn is_printable(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| !c.is_control() && !('\u{f700}'..='\u{f8ff}').contains(&c))
}

/// Maps GPUI's key name to a physical key, its unshifted character, and
/// whether reaching it needs Shift (US layout).
fn physical_key(name: &str) -> (Key, char, bool) {
    let named = match name {
        "enter" => Some(Key::Enter),
        "tab" => Some(Key::Tab),
        "space" => return (Key::Space, ' ', false),
        "backspace" => Some(Key::Backspace),
        "escape" => Some(Key::Escape),
        "delete" => Some(Key::Delete),
        "insert" => Some(Key::Insert),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "up" => Some(Key::ArrowUp),
        "down" => Some(Key::ArrowDown),
        "left" => Some(Key::ArrowLeft),
        "right" => Some(Key::ArrowRight),
        _ => function_key(name),
    };
    if let Some(key) = named {
        return (key, '\0', false);
    }

    let Some(c) = single_char(name) else {
        return (Key::Unidentified, '\0', false);
    };
    if let Some((key, unshifted)) = unshifted_key(c) {
        return (key, unshifted, false);
    }
    if let Some(base) = shifted_symbol_base(c)
        && let Some((key, unshifted)) = unshifted_key(base)
    {
        return (key, unshifted, true);
    }
    if c.is_ascii_uppercase()
        && let Some((key, unshifted)) = unshifted_key(c.to_ascii_lowercase())
    {
        return (key, unshifted, true);
    }
    (Key::Unidentified, c, false)
}

fn function_key(name: &str) -> Option<Key> {
    const KEYS: [Key; 25] = [
        Key::F1,
        Key::F2,
        Key::F3,
        Key::F4,
        Key::F5,
        Key::F6,
        Key::F7,
        Key::F8,
        Key::F9,
        Key::F10,
        Key::F11,
        Key::F12,
        Key::F13,
        Key::F14,
        Key::F15,
        Key::F16,
        Key::F17,
        Key::F18,
        Key::F19,
        Key::F20,
        Key::F21,
        Key::F22,
        Key::F23,
        Key::F24,
        Key::F25,
    ];
    let n: usize = name.strip_prefix('f')?.parse().ok()?;
    KEYS.get(n.checked_sub(1)?).copied()
}

fn unshifted_key(c: char) -> Option<(Key, char)> {
    const LETTERS: [Key; 26] = [
        Key::A,
        Key::B,
        Key::C,
        Key::D,
        Key::E,
        Key::F,
        Key::G,
        Key::H,
        Key::I,
        Key::J,
        Key::K,
        Key::L,
        Key::M,
        Key::N,
        Key::O,
        Key::P,
        Key::Q,
        Key::R,
        Key::S,
        Key::T,
        Key::U,
        Key::V,
        Key::W,
        Key::X,
        Key::Y,
        Key::Z,
    ];
    const DIGITS: [Key; 10] = [
        Key::Digit0,
        Key::Digit1,
        Key::Digit2,
        Key::Digit3,
        Key::Digit4,
        Key::Digit5,
        Key::Digit6,
        Key::Digit7,
        Key::Digit8,
        Key::Digit9,
    ];
    let key = match c {
        'a'..='z' => LETTERS[usize::from(c as u8 - b'a')],
        '0'..='9' => DIGITS[usize::from(c as u8 - b'0')],
        '-' => Key::Minus,
        '=' => Key::Equal,
        '[' => Key::BracketLeft,
        ']' => Key::BracketRight,
        '\\' => Key::Backslash,
        ';' => Key::Semicolon,
        '\'' => Key::Quote,
        ',' => Key::Comma,
        '.' => Key::Period,
        '/' => Key::Slash,
        '`' => Key::Backquote,
        _ => return None,
    };
    Some((key, c))
}

fn shifted_symbol_base(c: char) -> Option<char> {
    Some(match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use gpui_kit::Modifiers;

    use super::*;

    fn keystroke(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    fn none() -> Modifiers {
        Modifiers::default()
    }

    #[test]
    fn plain_letter() {
        let ks = keystroke("a", Some("a"), none());
        let input = map_keystroke(&ks, false);
        assert_eq!(input.key, Key::A);
        assert_eq!(input.unshifted, 'a');
        assert_eq!(input.mods, Mods::empty());
        assert_eq!(input.text, Some("a"));
    }

    #[test]
    fn shifted_letter_consumes_shift() {
        let ks = keystroke(
            "a",
            Some("A"),
            Modifiers {
                shift: true,
                ..none()
            },
        );
        let input = map_keystroke(&ks, false);
        assert_eq!(input.key, Key::A);
        assert_eq!(input.mods, Mods::SHIFT);
        assert_eq!(input.consumed, Mods::SHIFT);
        assert_eq!(input.text, Some("A"));
    }

    #[test]
    fn shifted_symbol_gets_its_shift_back() {
        let ks = keystroke("!", Some("!"), none());
        let input = map_keystroke(&ks, false);
        assert_eq!(input.key, Key::Digit1);
        assert_eq!(input.unshifted, '1');
        assert_eq!(input.mods, Mods::SHIFT);
        assert_eq!(input.consumed, Mods::SHIFT);
        assert_eq!(input.text, Some("!"));
    }

    #[test]
    fn control_keys_have_no_text() {
        let ks = keystroke(
            "c",
            None,
            Modifiers {
                control: true,
                ..none()
            },
        );
        let input = map_keystroke(&ks, false);
        assert_eq!(input.key, Key::C);
        assert_eq!(input.mods, Mods::CTRL);
        assert_eq!(input.text, None);

        let enter = keystroke("enter", Some("\n"), none());
        let enter = map_keystroke(&enter, false);
        assert_eq!(enter.key, Key::Enter);
        assert_eq!(enter.text, None);
        let tab = keystroke("tab", Some("\t"), none());
        let tab = map_keystroke(&tab, false);
        assert_eq!(tab.key, Key::Tab);
        assert_eq!(tab.text, None);
    }

    #[test]
    fn named_and_function_keys() {
        assert_eq!(physical_key("up"), (Key::ArrowUp, '\0', false));
        assert_eq!(physical_key("pagedown"), (Key::PageDown, '\0', false));
        assert_eq!(physical_key("f1"), (Key::F1, '\0', false));
        assert_eq!(physical_key("f25"), (Key::F25, '\0', false));
        assert_eq!(physical_key("f26"), (Key::Unidentified, '\0', false));
        assert_eq!(physical_key("space"), (Key::Space, ' ', false));
        assert_eq!(physical_key("ö"), (Key::Unidentified, 'ö', false));
        assert_eq!(physical_key("~"), (Key::Backquote, '`', true));
    }

    #[test]
    fn option_types_characters_unless_alt() {
        let alt = Modifiers {
            alt: true,
            ..none()
        };
        let ks = keystroke("c", Some("ç"), alt);

        let input = map_keystroke(&ks, false);
        assert_eq!(input.text, Some("ç"));
        assert_eq!(input.mods, Mods::ALT);
        assert_eq!(input.consumed, Mods::ALT);

        let input = map_keystroke(&ks, true);
        assert_eq!(input.text, Some("c"));
        assert_eq!(input.consumed, Mods::empty());

        let shifted = keystroke("c", Some("Ç"), Modifiers { shift: true, ..alt });
        let input = map_keystroke(&shifted, true);
        assert_eq!(input.text, Some("C"));
        assert_eq!(input.consumed, Mods::SHIFT);
    }

    #[test]
    fn text_editing_shortcuts() {
        let alt = Modifiers {
            alt: true,
            ..none()
        };
        let cmd = Modifiers {
            platform: true,
            ..none()
        };
        let shortcut =
            |key: &str, modifiers| text_editing_shortcut(&keystroke(key, None, modifiers));
        assert_eq!(shortcut("left", alt), Some(&b"\x1bb"[..]));
        assert_eq!(shortcut("right", alt), Some(&b"\x1bf"[..]));
        assert_eq!(shortcut("left", cmd), Some(&b"\x01"[..]));
        assert_eq!(shortcut("right", cmd), Some(&b"\x05"[..]));
        assert_eq!(shortcut("backspace", cmd), Some(&b"\x15"[..]));
        assert_eq!(shortcut("left", none()), None);
        assert_eq!(shortcut("backspace", alt), None);
        assert_eq!(shortcut("w", cmd), None);
        assert_eq!(shortcut("left", Modifiers { shift: true, ..alt }), None);
    }

    #[test]
    fn filters_function_key_private_use_text() {
        let ks = keystroke("f5", Some("\u{f708}"), none());
        assert_eq!(map_keystroke(&ks, false).text, None);
    }
}
