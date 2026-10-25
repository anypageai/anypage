//! Android key events as the engine's key names: "a", "1", "space", "enter",
//! "left", "f5", with modifiers in front: "ctrl+c", "ctrl+shift+z",
//! "shift+left". The names match the desktop host (apps/desktop/src/keys.rs),
//! so a page handles a Bluetooth keyboard on a phone exactly like a PC one.
//!
//! Kotlin passes the key code, the meta state, the character the key types
//! with Shift applied but Ctrl/Alt/Meta removed
//! (`KeyEvent.getUnicodeChar(meta & ~(CTRL|ALT|META))`) and, when right Alt
//! (AltGr) is held without Ctrl or Meta, the character of the layout's Alt
//! level (`getUnicodeChar(meta)`): '@' on a German Q, '€' on E, '{' on a
//! French 4, 'ç' on C in Android's generic layout. A key with such a
//! character types it; without one AltGr stays a chord ("alt+q"). Left Alt is
//! always a modifier, so page shortcuts such as "alt+s" keep working.

/// `KeyEvent.META_SHIFT_ON`
pub const META_SHIFT_ON: i32 = 0x1;
/// `KeyEvent.META_ALT_ON`
pub const META_ALT_ON: i32 = 0x2;
/// `KeyEvent.META_ALT_RIGHT_ON`: AltGr.
pub const META_ALT_RIGHT_ON: i32 = 0x20;
/// `KeyEvent.META_CTRL_ON`
pub const META_CTRL_ON: i32 = 0x1000;
/// `KeyEvent.META_META_ON`
pub const META_META_ON: i32 = 0x10000;
/// `KeyCharacterMap.COMBINING_ACCENT`: the key is a dead key.
const COMBINING_ACCENT: u32 = 0x8000_0000;

const F_KEYS: [&str; 12] = ["f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12"];

/// Keys with a name of their own (Android `KeyEvent.KEYCODE_*`).
fn named(code: i32) -> Option<&'static str> {
    Some(match code {
        62 => "space",
        66 | 160 => "enter", // ENTER, NUMPAD_ENTER
        61 => "tab",
        67 => "backspace", // DEL
        112 => "delete",   // FORWARD_DEL
        111 => "escape",
        124 => "insert",
        21 => "left",
        22 => "right",
        19 => "up",
        20 => "down",
        122 => "home", // MOVE_HOME
        123 => "end",  // MOVE_END
        92 => "pageup",
        93 => "pagedown",
        131..=142 => F_KEYS[(code - 131) as usize],
        _ => return None,
    })
}

/// The character a key stands for when the event carries none (Ctrl held on
/// some keyboards): letters, digits and the number pad.
fn fallback_char(code: i32) -> Option<char> {
    match code {
        29..=54 => char::from_u32('a' as u32 + (code - 29) as u32),
        7..=16 => char::from_u32('0' as u32 + (code - 7) as u32),
        144..=153 => char::from_u32('0' as u32 + (code - 144) as u32),
        _ => None,
    }
}

fn plain_char(plain: i32) -> Option<char> {
    let u = plain as u32;
    if u == 0 || u & COMBINING_ACCENT != 0 {
        return None;
    }
    char::from_u32(u).filter(|c| !c.is_control())
}

/// The engine's name for a key press, or `None` for keys the engine does not
/// route (bare modifiers, Back, volume, media keys), which then keep their
/// normal Android meaning.
pub fn key_name(code: i32, meta: i32, plain: i32) -> Option<String> {
    let (base, is_char) = match named(code) {
        Some(n) => (n.to_string(), false),
        None => {
            let c = plain_char(plain).or_else(|| fallback_char(code))?;
            (c.to_lowercase().collect::<String>(), true)
        }
    };
    let (ctrl, alt, shift, sup) = (meta & META_CTRL_ON != 0, meta & META_ALT_ON != 0, meta & META_SHIFT_ON != 0, meta & META_META_ON != 0);
    let chord = ctrl || alt || sup;
    let mut name = String::new();
    if ctrl {
        name.push_str("ctrl+");
    }
    if alt {
        name.push_str("alt+");
    }
    // Shift is part of the character itself ('A', '!') unless it is a chord
    if shift && (!is_char || chord) {
        name.push_str("shift+");
    }
    if sup {
        name.push_str("meta+");
    }
    name.push_str(&base);
    Some(name)
}

/// Text a key press types, when it is plain typing: no Ctrl/Alt/Meta, not a
/// dead key, not a control character such as Enter's '\n'.
pub fn typed_text(meta: i32, plain: i32) -> Option<String> {
    if meta & (META_CTRL_ON | META_ALT_ON | META_META_ON) != 0 {
        return None;
    }
    plain_char(plain).map(String::from)
}

/// What the render thread does with one key press.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPress {
    /// Sent as `Engine::key` (or when no text field has focus).
    pub name: Option<String>,
    /// Sent as `Engine::text_input` while the page has text focus.
    pub typed: Option<String>,
}

impl KeyPress {
    /// `plain`: the character without Ctrl/Alt/Meta; `alt`: the character of
    /// the Alt level when right Alt (and not Ctrl or Meta) is held, else 0.
    pub fn from_android(code: i32, meta: i32, plain: i32, alt: i32) -> Option<KeyPress> {
        if named(code).is_none() && meta & META_ALT_RIGHT_ON != 0 && meta & (META_CTRL_ON | META_META_ON) == 0 {
            if let Some(c) = plain_char(alt) {
                // AltGr typing: the character is the key, as Shift's is
                return Some(KeyPress { name: Some(c.to_lowercase().collect()), typed: Some(c.into()) });
            }
        }
        let p = KeyPress { name: key_name(code, meta, plain), typed: typed_text(meta, plain) };
        (p.name.is_some() || p.typed.is_some()).then_some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_the_desktop_host() {
        // KEYCODE_A = 29, KEYCODE_Z = 54, KEYCODE_1 = 8
        assert_eq!(key_name(29, META_SHIFT_ON, 'A' as i32).as_deref(), Some("a"));
        assert_eq!(key_name(54, META_CTRL_ON | META_SHIFT_ON, 'Z' as i32).as_deref(), Some("ctrl+shift+z"));
        assert_eq!(key_name(21, META_SHIFT_ON, 0).as_deref(), Some("shift+left"));
        assert_eq!(key_name(62, 0, ' ' as i32).as_deref(), Some("space"));
        assert_eq!(key_name(66, 0, '\n' as i32).as_deref(), Some("enter"));
        assert_eq!(key_name(67, 0, 0).as_deref(), Some("backspace"));
        assert_eq!(key_name(112, META_CTRL_ON, 0).as_deref(), Some("ctrl+delete"));
        assert_eq!(key_name(8, META_SHIFT_ON, '!' as i32).as_deref(), Some("!"));
        assert_eq!(key_name(135, 0, 0).as_deref(), Some("f5"));
        assert_eq!(key_name(31, META_CTRL_ON | META_ALT_ON | META_META_ON, 'c' as i32).as_deref(), Some("ctrl+alt+meta+c"));
        // Ctrl held and no character in the event: the key code still names it
        assert_eq!(key_name(47, META_CTRL_ON, 0).as_deref(), Some("ctrl+s"));
        assert_eq!(key_name(150, META_CTRL_ON, 0).as_deref(), Some("ctrl+6"));
    }

    #[test]
    fn keys_the_engine_leaves_to_android() {
        // BACK, VOLUME_UP, SHIFT_LEFT, CTRL_LEFT, HOME
        for code in [4, 24, 59, 113, 3] {
            assert_eq!(KeyPress::from_android(code, 0, 0, 0), None, "key code {code}");
        }
        // a dead key (combining accent) types nothing and has no name
        assert_eq!(KeyPress::from_android(0, 0, (COMBINING_ACCENT | 0x0301) as i32, 0), None);
    }

    /// `KeyEvent.META_ALT_LEFT_ON`
    const META_ALT_LEFT_ON: i32 = 0x10;

    #[test]
    fn altgr_types_its_character() {
        let altgr = META_ALT_ON | META_ALT_RIGHT_ON;
        // German Q (KEYCODE_Q = 45): AltGr gives '@'
        let k = KeyPress::from_android(45, altgr, 'q' as i32, '@' as i32).unwrap();
        assert_eq!((k.name.as_deref(), k.typed.as_deref()), (Some("@"), Some("@")));
        // German E: '€'; French 4 (KEYCODE_4 = 11): '{'; AltGr+Shift on a Polish A: 'Ą'
        assert_eq!(KeyPress::from_android(33, altgr, 'e' as i32, '€' as i32).unwrap().typed.as_deref(), Some("€"));
        assert_eq!(KeyPress::from_android(11, altgr, '\'' as i32, '{' as i32).unwrap().typed.as_deref(), Some("{"));
        let k = KeyPress::from_android(29, altgr | META_SHIFT_ON, 'A' as i32, 'Ą' as i32).unwrap();
        assert_eq!((k.name.as_deref(), k.typed.as_deref()), (Some("ą"), Some("Ą")));
        // a layout with nothing on the Alt level: AltGr stays a chord
        let k = KeyPress::from_android(45, altgr, 'q' as i32, 0).unwrap();
        assert_eq!((k.name.as_deref(), k.typed), (Some("alt+q"), None));
        // left Alt is a modifier even where the layout has an Alt level
        // (Android's generic layout: Alt+S is 'ß'), so "alt+s" shortcuts work
        let k = KeyPress::from_android(47, META_ALT_ON | META_ALT_LEFT_ON, 's' as i32, 'ß' as i32).unwrap();
        assert_eq!((k.name.as_deref(), k.typed), (Some("alt+s"), None));
        // Ctrl+Alt is a chord whatever the layout says (Kotlin passes 0 then, but be sure)
        let k = KeyPress::from_android(45, altgr | META_CTRL_ON, 'q' as i32, '@' as i32).unwrap();
        assert_eq!((k.name.as_deref(), k.typed), (Some("ctrl+alt+q"), None));
        // named keys keep their names: alt+left is navigation
        let k = KeyPress::from_android(21, altgr, 0, 0).unwrap();
        assert_eq!((k.name.as_deref(), k.typed), (Some("alt+left"), None));
        // a dead key on the AltGr level types nothing by itself
        let k = KeyPress::from_android(45, altgr, 'q' as i32, (COMBINING_ACCENT | 0x0303) as i32).unwrap();
        assert_eq!((k.name.as_deref(), k.typed), (Some("alt+q"), None));
    }

    #[test]
    fn typing() {
        assert_eq!(typed_text(0, '字' as i32).as_deref(), Some("字"));
        assert_eq!(typed_text(META_SHIFT_ON, 'A' as i32).as_deref(), Some("A"));
        assert_eq!(typed_text(0, ' ' as i32).as_deref(), Some(" "));
        assert_eq!(typed_text(0, '\n' as i32), None);
        assert_eq!(typed_text(META_CTRL_ON, 'c' as i32), None);
        assert_eq!(typed_text(0, 0x1F600).as_deref(), Some("😀"));
        let k = KeyPress::from_android(0, 0, '字' as i32, 0).unwrap();
        assert_eq!((k.name.as_deref(), k.typed.as_deref()), (Some("字"), Some("字")));
        let k = KeyPress::from_android(67, 0, 0, 0).unwrap();
        assert_eq!((k.name.as_deref(), k.typed), (Some("backspace"), None));
    }
}
