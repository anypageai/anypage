//! Key names the engine and pages understand: "a", "1", "space", "enter",
//! "left", "f5", with modifiers in front: "ctrl+c", "ctrl+shift+z", "shift+left".

use winit::keyboard::{Key, ModifiersState, NamedKey};

fn named(k: &NamedKey) -> Option<&'static str> {
    Some(match k {
        NamedKey::Space => "space",
        NamedKey::Enter => "enter",
        NamedKey::Tab => "tab",
        NamedKey::Backspace => "backspace",
        NamedKey::Delete => "delete",
        NamedKey::Escape => "escape",
        NamedKey::Insert => "insert",
        NamedKey::ArrowLeft => "left",
        NamedKey::ArrowRight => "right",
        NamedKey::ArrowUp => "up",
        NamedKey::ArrowDown => "down",
        NamedKey::Home => "home",
        NamedKey::End => "end",
        NamedKey::PageUp => "pageup",
        NamedKey::PageDown => "pagedown",
        NamedKey::F1 => "f1",
        NamedKey::F2 => "f2",
        NamedKey::F3 => "f3",
        NamedKey::F4 => "f4",
        NamedKey::F5 => "f5",
        NamedKey::F6 => "f6",
        NamedKey::F7 => "f7",
        NamedKey::F8 => "f8",
        NamedKey::F9 => "f9",
        NamedKey::F10 => "f10",
        NamedKey::F11 => "f11",
        NamedKey::F12 => "f12",
        _ => return None,
    })
}

/// The engine's name for a key press, or `None` for keys it does not route
/// (bare modifiers, media keys).
pub fn key_name(key: &Key, mods: ModifiersState) -> Option<String> {
    let (base, is_char) = match key {
        Key::Named(n) => (named(n)?.to_string(), false),
        Key::Character(c) => (c.to_lowercase(), true),
        _ => return None,
    };
    let chord = mods.control_key() || mods.alt_key() || mods.super_key();
    let mut name = String::new();
    if mods.control_key() {
        name.push_str("ctrl+");
    }
    if mods.alt_key() {
        name.push_str("alt+");
    }
    // Shift is part of the character itself ('A', '!') unless it is a chord
    if mods.shift_key() && (!is_char || chord) {
        name.push_str("shift+");
    }
    if mods.super_key() {
        name.push_str("meta+");
    }
    name.push_str(&base);
    Some(name)
}

/// Text a key press types, when it is plain typing (no ctrl/alt/meta and no
/// control characters such as Enter's "\r").
pub fn typed_text(text: Option<&str>, mods: ModifiersState) -> Option<String> {
    let t = text?;
    if mods.control_key() || mods.alt_key() || mods.super_key() || t.is_empty() || t.chars().any(char::is_control) {
        return None;
    }
    Some(t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let none = ModifiersState::empty();
        assert_eq!(key_name(&Key::Character("A".into()), ModifiersState::SHIFT).as_deref(), Some("a"));
        assert_eq!(key_name(&Key::Character("z".into()), ModifiersState::CONTROL | ModifiersState::SHIFT).as_deref(), Some("ctrl+shift+z"));
        assert_eq!(key_name(&Key::Named(NamedKey::ArrowLeft), ModifiersState::SHIFT).as_deref(), Some("shift+left"));
        assert_eq!(key_name(&Key::Named(NamedKey::Space), none).as_deref(), Some("space"));
        assert_eq!(typed_text(Some("字"), none).as_deref(), Some("字"));
        assert_eq!(typed_text(Some("\r"), none), None);
        assert_eq!(typed_text(Some("c"), ModifiersState::CONTROL), None);
    }
}
