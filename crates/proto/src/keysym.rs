//! X11 keysym constants and helpers.
//!
//! The protocol carries keyboard input as X11 keysyms because the server
//! injects them with the XTEST extension. Characters are converted with
//! [`keysym_from_char`], which follows the X11 convention: Latin-1
//! characters map to themselves and everything else to
//! `0x0100_0000 | codepoint`.

/// Space.
pub const SPACE: u32 = 0x0020;
/// BackSpace.
pub const BACKSPACE: u32 = 0xff08;
/// Tab.
pub const TAB: u32 = 0xff09;
/// Shift+Tab as reported by X.
pub const ISO_LEFT_TAB: u32 = 0xfe20;
/// Return / Enter.
pub const RETURN: u32 = 0xff0d;
/// Pause.
pub const PAUSE: u32 = 0xff13;
/// Scroll Lock.
pub const SCROLL_LOCK: u32 = 0xff14;
/// Escape.
pub const ESCAPE: u32 = 0xff1b;
/// Delete.
pub const DELETE: u32 = 0xffff;
/// Home.
pub const HOME: u32 = 0xff50;
/// Left arrow.
pub const LEFT: u32 = 0xff51;
/// Up arrow.
pub const UP: u32 = 0xff52;
/// Right arrow.
pub const RIGHT: u32 = 0xff53;
/// Down arrow.
pub const DOWN: u32 = 0xff54;
/// Page Up.
pub const PAGE_UP: u32 = 0xff55;
/// Page Down.
pub const PAGE_DOWN: u32 = 0xff56;
/// End.
pub const END: u32 = 0xff57;
/// Print Screen.
pub const PRINT: u32 = 0xff61;
/// Insert.
pub const INSERT: u32 = 0xff63;
/// Menu / context menu key.
pub const MENU: u32 = 0xff67;
/// Num Lock.
pub const NUM_LOCK: u32 = 0xff7f;
/// Keypad Enter.
pub const KP_ENTER: u32 = 0xff8d;
/// Keypad Home.
pub const KP_HOME: u32 = 0xff95;
/// Keypad Left.
pub const KP_LEFT: u32 = 0xff96;
/// Keypad Up.
pub const KP_UP: u32 = 0xff97;
/// Keypad Right.
pub const KP_RIGHT: u32 = 0xff98;
/// Keypad Down.
pub const KP_DOWN: u32 = 0xff99;
/// Keypad Page Up.
pub const KP_PAGE_UP: u32 = 0xff9a;
/// Keypad Page Down.
pub const KP_PAGE_DOWN: u32 = 0xff9b;
/// Keypad End.
pub const KP_END: u32 = 0xff9c;
/// Keypad Insert.
pub const KP_INSERT: u32 = 0xff9e;
/// Keypad Delete.
pub const KP_DELETE: u32 = 0xff9f;
/// Keypad `*`.
pub const KP_MULTIPLY: u32 = 0xffaa;
/// Keypad `+`.
pub const KP_ADD: u32 = 0xffab;
/// Keypad `-`.
pub const KP_SUBTRACT: u32 = 0xffad;
/// Keypad `.`.
pub const KP_DECIMAL: u32 = 0xffae;
/// Keypad `/`.
pub const KP_DIVIDE: u32 = 0xffaf;
/// Keypad `0`; `KP_0 + n` for digit `n`.
pub const KP_0: u32 = 0xffb0;
/// F1; `F1 + n - 1` for `Fn` up to F35.
pub const F1: u32 = 0xffbe;
/// Left Shift.
pub const SHIFT_L: u32 = 0xffe1;
/// Right Shift.
pub const SHIFT_R: u32 = 0xffe2;
/// Left Control.
pub const CONTROL_L: u32 = 0xffe3;
/// Right Control.
pub const CONTROL_R: u32 = 0xffe4;
/// Caps Lock.
pub const CAPS_LOCK: u32 = 0xffe5;
/// Left Alt.
pub const ALT_L: u32 = 0xffe9;
/// Right Alt.
pub const ALT_R: u32 = 0xffea;
/// Left Super (Windows / Command).
pub const SUPER_L: u32 = 0xffeb;
/// Right Super.
pub const SUPER_R: u32 = 0xffec;
/// Right Alt as AltGr.
pub const ISO_LEVEL3_SHIFT: u32 = 0xfe03;
/// Multimedia: volume mute.
pub const AUDIO_MUTE: u32 = 0x1008ff12;
/// Multimedia: volume down.
pub const AUDIO_LOWER_VOLUME: u32 = 0x1008ff11;
/// Multimedia: volume up.
pub const AUDIO_RAISE_VOLUME: u32 = 0x1008ff13;
/// Multimedia: play/pause.
pub const AUDIO_PLAY: u32 = 0x1008ff14;

/// Keysym for a Unicode character.
pub fn keysym_from_char(c: char) -> u32 {
    let cp = c as u32;
    match cp {
        0x20..=0x7e | 0xa0..=0xff => cp,
        // Control characters are not keysyms; map the common ones.
        0x08 => BACKSPACE,
        0x09 => TAB,
        0x0d | 0x0a => RETURN,
        0x1b => ESCAPE,
        0x7f => DELETE,
        _ => 0x0100_0000 | cp,
    }
}

/// Unicode character for a keysym, if it represents one.
pub fn char_from_keysym(ks: u32) -> Option<char> {
    match ks {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(ks),
        0x0100_0000..=0x0110_FFFF => char::from_u32(ks & 0x00FF_FFFF),
        _ => None,
    }
}

/// Keysym for function key `n` (1-based). Returns `None` for `n == 0` or `n > 35`.
pub fn function_key(n: u32) -> Option<u32> {
    if (1..=35).contains(&n) {
        Some(F1 + n - 1)
    } else {
        None
    }
}

/// Whether the keysym is a modifier key.
pub fn is_modifier(ks: u32) -> bool {
    matches!(
        ks,
        SHIFT_L
            | SHIFT_R
            | CONTROL_L
            | CONTROL_R
            | CAPS_LOCK
            | ALT_L
            | ALT_R
            | SUPER_L
            | SUPER_R
            | ISO_LEVEL3_SHIFT
            | NUM_LOCK
    )
}

/// Human readable name for debugging/logging.
pub fn name(ks: u32) -> String {
    match ks {
        BACKSPACE => "BackSpace".into(),
        TAB => "Tab".into(),
        RETURN => "Return".into(),
        ESCAPE => "Escape".into(),
        DELETE => "Delete".into(),
        HOME => "Home".into(),
        LEFT => "Left".into(),
        UP => "Up".into(),
        RIGHT => "Right".into(),
        DOWN => "Down".into(),
        PAGE_UP => "Page_Up".into(),
        PAGE_DOWN => "Page_Down".into(),
        END => "End".into(),
        INSERT => "Insert".into(),
        SHIFT_L => "Shift_L".into(),
        SHIFT_R => "Shift_R".into(),
        CONTROL_L => "Control_L".into(),
        CONTROL_R => "Control_R".into(),
        ALT_L => "Alt_L".into(),
        ALT_R => "Alt_R".into(),
        SUPER_L => "Super_L".into(),
        SUPER_R => "Super_R".into(),
        CAPS_LOCK => "Caps_Lock".into(),
        SPACE => "space".into(),
        _ => {
            if let Some(c) = char_from_keysym(ks) {
                format!("'{c}'")
            } else if (F1..F1 + 35).contains(&ks) {
                format!("F{}", ks - F1 + 1)
            } else {
                format!("0x{ks:x}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_latin1_map_to_themselves() {
        assert_eq!(keysym_from_char('a'), 0x61);
        assert_eq!(keysym_from_char(' '), SPACE);
        assert_eq!(keysym_from_char('é'), 0xe9);
        assert_eq!(char_from_keysym(0x61), Some('a'));
        assert_eq!(char_from_keysym(0xe9), Some('é'));
    }

    #[test]
    fn unicode_uses_x11_convention() {
        assert_eq!(keysym_from_char('€'), 0x0100_20ac);
        assert_eq!(char_from_keysym(0x0100_20ac), Some('€'));
        assert_eq!(keysym_from_char('😀'), 0x0101_f600);
        assert_eq!(char_from_keysym(0x0101_f600), Some('😀'));
    }

    #[test]
    fn control_chars() {
        assert_eq!(keysym_from_char('\n'), RETURN);
        assert_eq!(keysym_from_char('\t'), TAB);
        assert_eq!(keysym_from_char('\u{8}'), BACKSPACE);
        assert_eq!(char_from_keysym(RETURN), None);
    }

    #[test]
    fn function_keys() {
        assert_eq!(function_key(1), Some(F1));
        assert_eq!(function_key(12), Some(0xffc9));
        assert_eq!(function_key(0), None);
        assert_eq!(function_key(36), None);
        assert_eq!(name(0xffc9), "F12");
    }

    #[test]
    fn modifiers_and_names() {
        assert!(is_modifier(SHIFT_L));
        assert!(!is_modifier(RETURN));
        assert_eq!(name(RETURN), "Return");
        assert_eq!(name(0x61), "'a'");
        assert_eq!(name(0x1008ff12), "0x1008ff12");
    }
}
