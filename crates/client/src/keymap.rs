//! Translation of winit keyboard events into X11 keysyms.

use lynxrdp_proto::keysym as ks;
use winit::keyboard::{Key, KeyLocation, NamedKey};

/// Map a winit logical key (plus its location) to an X11 keysym.
pub fn keysym_for(key: &Key, location: KeyLocation) -> Option<u32> {
    match key {
        Key::Character(s) => {
            let mut chars = s.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                // Multi-character strings (dead key compositions) are typed
                // by the app as separate events; take the first here.
                log::debug!("multi-character key {s:?}; using first char");
            }
            Some(ks::keysym_from_char(c))
        }
        Key::Named(n) => named(*n, location),
        Key::Unidentified(_) | Key::Dead(_) => None,
    }
}

fn named(n: NamedKey, location: KeyLocation) -> Option<u32> {
    let right = location == KeyLocation::Right;
    let numpad = location == KeyLocation::Numpad;
    Some(match n {
        NamedKey::Alt => {
            if right {
                ks::ALT_R
            } else {
                ks::ALT_L
            }
        }
        NamedKey::AltGraph => ks::ISO_LEVEL3_SHIFT,
        NamedKey::CapsLock => ks::CAPS_LOCK,
        NamedKey::Control => {
            if right {
                ks::CONTROL_R
            } else {
                ks::CONTROL_L
            }
        }
        NamedKey::Shift => {
            if right {
                ks::SHIFT_R
            } else {
                ks::SHIFT_L
            }
        }
        NamedKey::Super | NamedKey::Meta | NamedKey::Hyper => {
            if right {
                ks::SUPER_R
            } else {
                ks::SUPER_L
            }
        }
        NamedKey::NumLock => ks::NUM_LOCK,
        NamedKey::ScrollLock => ks::SCROLL_LOCK,
        NamedKey::Enter => {
            if numpad {
                ks::KP_ENTER
            } else {
                ks::RETURN
            }
        }
        NamedKey::Tab => ks::TAB,
        NamedKey::Space => ks::SPACE,
        NamedKey::ArrowDown => {
            if numpad {
                ks::KP_DOWN
            } else {
                ks::DOWN
            }
        }
        NamedKey::ArrowLeft => {
            if numpad {
                ks::KP_LEFT
            } else {
                ks::LEFT
            }
        }
        NamedKey::ArrowRight => {
            if numpad {
                ks::KP_RIGHT
            } else {
                ks::RIGHT
            }
        }
        NamedKey::ArrowUp => {
            if numpad {
                ks::KP_UP
            } else {
                ks::UP
            }
        }
        NamedKey::End => {
            if numpad {
                ks::KP_END
            } else {
                ks::END
            }
        }
        NamedKey::Home => {
            if numpad {
                ks::KP_HOME
            } else {
                ks::HOME
            }
        }
        NamedKey::PageDown => {
            if numpad {
                ks::KP_PAGE_DOWN
            } else {
                ks::PAGE_DOWN
            }
        }
        NamedKey::PageUp => {
            if numpad {
                ks::KP_PAGE_UP
            } else {
                ks::PAGE_UP
            }
        }
        NamedKey::Backspace => ks::BACKSPACE,
        NamedKey::Clear => 0xff0b,
        NamedKey::Delete => {
            if numpad {
                ks::KP_DELETE
            } else {
                ks::DELETE
            }
        }
        NamedKey::Insert => {
            if numpad {
                ks::KP_INSERT
            } else {
                ks::INSERT
            }
        }
        NamedKey::Escape => ks::ESCAPE,
        NamedKey::Pause => ks::PAUSE,
        NamedKey::PrintScreen => ks::PRINT,
        NamedKey::ContextMenu => ks::MENU,
        NamedKey::F1 => ks::F1,
        NamedKey::F2 => ks::F1 + 1,
        NamedKey::F3 => ks::F1 + 2,
        NamedKey::F4 => ks::F1 + 3,
        NamedKey::F5 => ks::F1 + 4,
        NamedKey::F6 => ks::F1 + 5,
        NamedKey::F7 => ks::F1 + 6,
        NamedKey::F8 => ks::F1 + 7,
        NamedKey::F9 => ks::F1 + 8,
        NamedKey::F10 => ks::F1 + 9,
        NamedKey::F11 => ks::F1 + 10,
        NamedKey::F12 => ks::F1 + 11,
        NamedKey::F13 => ks::F1 + 12,
        NamedKey::F14 => ks::F1 + 13,
        NamedKey::F15 => ks::F1 + 14,
        NamedKey::F16 => ks::F1 + 15,
        NamedKey::F17 => ks::F1 + 16,
        NamedKey::F18 => ks::F1 + 17,
        NamedKey::F19 => ks::F1 + 18,
        NamedKey::F20 => ks::F1 + 19,
        NamedKey::AudioVolumeDown => ks::AUDIO_LOWER_VOLUME,
        NamedKey::AudioVolumeUp => ks::AUDIO_RAISE_VOLUME,
        NamedKey::AudioVolumeMute => ks::AUDIO_MUTE,
        NamedKey::MediaPlayPause => ks::AUDIO_PLAY,
        _ => return None,
    })
}

/// Keysym for a numpad character key, honouring the numpad location so
/// that applications can distinguish `KP_1` from `1`.
pub fn numpad_keysym(c: char) -> Option<u32> {
    Some(match c {
        '0'..='9' => ks::KP_0 + (c as u32 - '0' as u32),
        '.' | ',' => ks::KP_DECIMAL,
        '+' => ks::KP_ADD,
        '-' => ks::KP_SUBTRACT,
        '*' => ks::KP_MULTIPLY,
        '/' => ks::KP_DIVIDE,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::SmolStr;

    #[test]
    fn characters_map_to_keysyms() {
        assert_eq!(
            keysym_for(&Key::Character(SmolStr::new("a")), KeyLocation::Standard),
            Some(0x61)
        );
        assert_eq!(
            keysym_for(&Key::Character(SmolStr::new("€")), KeyLocation::Standard),
            Some(0x100_20ac)
        );
        assert_eq!(
            keysym_for(&Key::Named(NamedKey::Enter), KeyLocation::Standard),
            Some(ks::RETURN)
        );
        assert_eq!(
            keysym_for(&Key::Named(NamedKey::Enter), KeyLocation::Numpad),
            Some(ks::KP_ENTER)
        );
        assert_eq!(
            keysym_for(&Key::Named(NamedKey::Shift), KeyLocation::Right),
            Some(ks::SHIFT_R)
        );
        assert_eq!(
            keysym_for(&Key::Named(NamedKey::Shift), KeyLocation::Left),
            Some(ks::SHIFT_L)
        );
        assert_eq!(
            keysym_for(&Key::Named(NamedKey::F12), KeyLocation::Standard),
            Some(0xffc9)
        );
        assert_eq!(
            keysym_for(&Key::Named(NamedKey::Fn), KeyLocation::Standard),
            None
        );
        assert_eq!(keysym_for(&Key::Dead(None), KeyLocation::Standard), None);
        assert_eq!(numpad_keysym('7'), Some(ks::KP_0 + 7));
        assert_eq!(numpad_keysym('x'), None);
    }
}
