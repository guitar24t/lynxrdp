//! Input injection with the XTEST extension.
//!
//! Keys arrive as X11 keysyms. Each keysym is looked up in the server's
//! keyboard mapping. If it is only reachable with a modifier the client did
//! not press (for example the client has a different layout), the modifier
//! is pressed temporarily. Keysyms absent from the mapping are bound to a
//! spare keycode on the fly, the way `xdotool` and VNC servers do.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use lynxrdp_proto::keysym;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, AutoRepeatMode, ChangeKeyboardControlAux, ConnectionExt as _};
use x11rb::protocol::xtest;

use super::XDisplay;

const KEY_PRESS: u8 = xproto::KEY_PRESS_EVENT;
const KEY_RELEASE: u8 = xproto::KEY_RELEASE_EVENT;
const BUTTON_PRESS: u8 = xproto::BUTTON_PRESS_EVENT;
const BUTTON_RELEASE: u8 = xproto::BUTTON_RELEASE_EVENT;
const MOTION: u8 = xproto::MOTION_NOTIFY_EVENT;

/// Where a keysym lives in the keyboard mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyLocation {
    keycode: u8,
    /// Column in the keysym table: 0 = plain, 1 = shift, 2 = level3, 3 = level3+shift.
    level: u8,
}

/// Parsed keyboard mapping.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    min_keycode: u8,
    max_keycode: u8,
    per_keycode: u8,
    /// Keysym table, `per_keycode` entries per keycode starting at `min_keycode`.
    syms: Vec<u32>,
    /// keysym -> best locations (lowest level first).
    lookup: HashMap<u32, Vec<KeyLocation>>,
}

impl Keymap {
    /// Build from a `GetKeyboardMapping` reply.
    pub fn from_reply(min_keycode: u8, max_keycode: u8, per_keycode: u8, syms: Vec<u32>) -> Self {
        let mut lookup: HashMap<u32, Vec<KeyLocation>> = HashMap::new();
        let count = usize::from(max_keycode) - usize::from(min_keycode) + 1;
        for kc_idx in 0..count {
            let keycode = min_keycode.wrapping_add(kc_idx as u8);
            let row =
                &syms[kc_idx * usize::from(per_keycode)..(kc_idx + 1) * usize::from(per_keycode)];
            // Columns 0..3 follow the XKB convention (group 1 levels 1-2, then
            // level 3 for the AltGr group in the "x11 core" view). Column order
            // in the core protocol view is: [g1l1, g1l2, g2l1, g2l2, ...].
            for (col, &ks) in row.iter().enumerate().take(4) {
                if ks == 0 {
                    continue;
                }
                let level = col as u8;
                lookup
                    .entry(ks)
                    .or_default()
                    .push(KeyLocation { keycode, level });
            }
            // Un-shifted alphabetic keys: X lists only the lowercase in column
            // 0 and uppercase in column 1 when both are given, but some maps
            // list a single lowercase keysym and expect implicit case
            // conversion. Handle that by registering the uppercase too.
            if let (Some(&plain), true) = (row.first(), row.get(1).map(|&s| s == 0).unwrap_or(true))
            {
                if let Some(c) = keysym::char_from_keysym(plain) {
                    if c.is_lowercase() {
                        let up: String = c.to_uppercase().collect();
                        if let Some(uc) = up.chars().next() {
                            if up.chars().count() == 1 {
                                lookup
                                    .entry(keysym::keysym_from_char(uc))
                                    .or_default()
                                    .push(KeyLocation { keycode, level: 1 });
                            }
                        }
                    }
                }
            }
        }
        for locs in lookup.values_mut() {
            locs.sort_by_key(|l| (l.level, l.keycode));
        }
        Self {
            min_keycode,
            max_keycode,
            per_keycode,
            syms,
            lookup,
        }
    }

    /// Location of a keysym, preferring one that needs no modifier.
    fn find(&self, ks: u32) -> Option<KeyLocation> {
        self.lookup.get(&ks).and_then(|v| v.first().copied())
    }

    /// Keycode for a keysym that must be pressable without modifiers
    /// (used for modifiers themselves).
    fn plain_keycode(&self, ks: u32) -> Option<u8> {
        self.lookup
            .get(&ks)?
            .iter()
            .find(|l| l.level == 0)
            .map(|l| l.keycode)
    }

    /// Keycodes with no keysyms at all, highest first (best spare candidates).
    fn spare_keycodes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let count = usize::from(self.max_keycode) - usize::from(self.min_keycode) + 1;
        for kc_idx in (0..count).rev() {
            let row = &self.syms[kc_idx * usize::from(self.per_keycode)
                ..(kc_idx + 1) * usize::from(self.per_keycode)];
            if row.iter().all(|&s| s == 0) {
                out.push(self.min_keycode.wrapping_add(kc_idx as u8));
            }
        }
        out
    }
}

/// The server's global auto-repeat setting, or `None` if it would not say.
///
/// A server that will not answer this is one we must not "restore" a guess to,
/// which is why the failure is an absence rather than a default.
fn read_auto_repeat(display: &XDisplay) -> Option<AutoRepeatMode> {
    let reply = match display.conn().get_keyboard_control() {
        Ok(cookie) => cookie.reply(),
        Err(e) => {
            log::warn!("cannot ask for the keyboard auto-repeat setting: {e}");
            return None;
        }
    };
    match reply {
        Ok(r) => Some(r.global_auto_repeat),
        Err(e) => {
            log::warn!("cannot read the keyboard auto-repeat setting: {e}");
            None
        }
    }
}

/// Injects keyboard and pointer events.
pub struct InputInjector {
    display: Arc<XDisplay>,
    keymap: Keymap,
    shift_keycode: Option<u8>,
    level3_keycode: Option<u8>,
    /// keysym -> keycode used for the press, so the release matches.
    pressed: HashMap<u32, u8>,
    /// Modifier keysyms the client currently holds.
    held_modifiers: HashSet<u32>,
    pressed_buttons: HashSet<u8>,
    /// Spare keycodes available for dynamic binding.
    spares: Vec<u8>,
    /// keysym -> spare keycode currently bound to it.
    dynamic: HashMap<u32, u8>,
    /// Round robin over `dynamic` when spares run out.
    dynamic_order: Vec<u32>,
    last_pointer: (i16, i16),
    /// The server's global auto-repeat setting as it was when we attached, so
    /// that a session we suppressed it for gets back exactly what it had.
    /// Read once, here, rather than assumed to be on: a user who turned key
    /// repeat off would otherwise find us turning it back on for them.
    original_auto_repeat: Option<AutoRepeatMode>,
}

impl InputInjector {
    /// Create an injector and load the keyboard mapping.
    pub fn new(display: Arc<XDisplay>) -> Result<Self> {
        anyhow::ensure!(display.ext.xtest, "XTEST extension is required for input");
        let original_auto_repeat = read_auto_repeat(&display);
        let mut s = Self {
            display,
            keymap: Keymap::default(),
            shift_keycode: None,
            level3_keycode: None,
            pressed: HashMap::new(),
            held_modifiers: HashSet::new(),
            pressed_buttons: HashSet::new(),
            spares: Vec::new(),
            dynamic: HashMap::new(),
            dynamic_order: Vec::new(),
            last_pointer: (0, 0),
            original_auto_repeat,
        };
        s.reload_keymap()?;
        Ok(s)
    }

    /// Turn the X server's own key auto-repeat off while a client is connected.
    ///
    /// A held key otherwise has two repeat generators that know nothing about
    /// each other. The client's operating system repeats the key and forwards
    /// every repeat to us as another `KeyEvent`, and -- this is the part nobody
    /// has confirmed against Xvfb -- `XTestFakeInput` leaves the keycode
    /// logically held, which entitles the X server to repeat it as well. The
    /// visible results are a repeat rate that is neither end's configured one
    /// on arrows and Backspace, and a key that runs away entirely whenever a
    /// `KeyRelease` is delayed past X's 660 ms threshold by a stalled tunnel.
    /// x11vnc ships `-norepeat` and turns server-side repeat off by default for
    /// exactly this reasoning.
    ///
    /// Applied per connection rather than once at startup because a desktop's
    /// settings daemon (gnome-settings-daemon, xfsettingsd, kded) applies the
    /// user's keyboard preferences some seconds into login, asynchronously, and
    /// would simply overwrite a value set before it ran.
    ///
    /// Harmless if the guess about Xvfb is wrong: with no server-side repeat to
    /// suppress this changes a setting nothing is reading, and
    /// [`InputInjector::restore_auto_repeat`] puts it back on the way out.
    pub fn suppress_auto_repeat(&self) {
        self.set_auto_repeat(AutoRepeatMode::OFF);
    }

    /// Put the auto-repeat setting back to whatever it was when we attached.
    pub fn restore_auto_repeat(&self) {
        if let Some(mode) = self.original_auto_repeat {
            self.set_auto_repeat(mode);
        }
    }

    fn set_auto_repeat(&self, mode: AutoRepeatMode) {
        let aux = ChangeKeyboardControlAux::new().auto_repeat_mode(mode);
        // Logged, never propagated. This is a nicety about repeat rates; it has
        // no business being able to end a user's desktop session, which is what
        // an error out of here would eventually become.
        let done = self
            .display
            .conn()
            .change_keyboard_control(&aux)
            .map_err(anyhow::Error::from)
            .and_then(|c| c.check().map_err(anyhow::Error::from));
        match done {
            Ok(()) => log::debug!("keyboard auto-repeat set to {mode:?}"),
            Err(e) => log::warn!("cannot change the keyboard auto-repeat setting: {e:#}"),
        }
    }

    /// Re-read the keyboard mapping (after a `MappingNotify`).
    pub fn reload_keymap(&mut self) -> Result<()> {
        let conn = self.display.conn();
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let count = max - min + 1;
        let reply = conn
            .get_keyboard_mapping(min, count)?
            .reply()
            .context("keyboard mapping")?;
        self.keymap = Keymap::from_reply(min, max, reply.keysyms_per_keycode, reply.keysyms);
        self.shift_keycode = self.keymap.plain_keycode(keysym::SHIFT_L);
        self.level3_keycode = self
            .keymap
            .plain_keycode(keysym::ISO_LEVEL3_SHIFT)
            .or_else(|| self.keymap.plain_keycode(keysym::ALT_R));
        // Keep dynamic bindings that are still present in the new map.
        self.dynamic
            .retain(|ks, kc| self.keymap.find(*ks).map(|l| l.keycode) == Some(*kc));
        self.dynamic_order
            .retain(|ks| self.dynamic.contains_key(ks));
        let bound: HashSet<u8> = self.dynamic.values().copied().collect();
        self.spares = self
            .keymap
            .spare_keycodes()
            .into_iter()
            .filter(|k| !bound.contains(k))
            .collect();
        log::debug!(
            "keymap loaded: keycodes {min}-{max}, {} spare, shift={:?} level3={:?}",
            self.spares.len(),
            self.shift_keycode,
            self.level3_keycode
        );
        Ok(())
    }

    fn fake(&self, type_: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        xtest::fake_input(
            self.display.conn(),
            type_,
            detail,
            x11rb::CURRENT_TIME,
            self.display.root(),
            x,
            y,
            0,
        )?;
        Ok(())
    }

    /// Bind `ks` to a spare keycode (or recycle the oldest dynamic binding).
    fn bind_dynamic(&mut self, ks: u32) -> Result<Option<u8>> {
        let keycode = if let Some(kc) = self.spares.pop() {
            kc
        } else if let Some(old) = self.dynamic_order.first().copied() {
            // Do not recycle a keycode that is currently pressed.
            if self.pressed.contains_key(&old) {
                return Ok(None);
            }
            self.dynamic_order.remove(0);
            self.dynamic.remove(&old).unwrap_or(0)
        } else {
            return Ok(None);
        };
        let conn = self.display.conn();
        let per = self.keymap.per_keycode.max(1);
        let mut row = vec![0u32; usize::from(per)];
        row[0] = ks;
        if per > 1 {
            row[1] = ks;
        }
        conn.change_keyboard_mapping(1, keycode, per, &row)?
            .check()
            .context("change keyboard mapping")?;
        // Update the local table so `find` sees the binding.
        let idx = (usize::from(keycode) - usize::from(self.keymap.min_keycode)) * usize::from(per);
        self.keymap.syms[idx..idx + usize::from(per)].copy_from_slice(&row);
        self.keymap
            .lookup
            .entry(ks)
            .or_default()
            .insert(0, KeyLocation { keycode, level: 0 });
        self.dynamic.insert(ks, keycode);
        self.dynamic_order.push(ks);
        // The server must process the mapping change before the fake key.
        self.display.sync()?;
        log::debug!(
            "bound keysym {} to spare keycode {keycode}",
            keysym::name(ks)
        );
        Ok(Some(keycode))
    }

    /// Inject a key press or release.
    pub fn key(&mut self, ks: u32, down: bool) -> Result<()> {
        if !down {
            let Some(kc) = self.pressed.remove(&ks) else {
                // Release without press (e.g. pressed before connect): best effort.
                if let Some(loc) = self.keymap.find(ks) {
                    self.fake(KEY_RELEASE, loc.keycode, 0, 0)?;
                    self.display.flush()?;
                }
                self.held_modifiers.remove(&ks);
                return Ok(());
            };
            self.fake(KEY_RELEASE, kc, 0, 0)?;
            self.held_modifiers.remove(&ks);
            return self.display.flush();
        }

        let loc = match self.keymap.find(ks) {
            Some(l) => l,
            None => match self.bind_dynamic(ks)? {
                Some(kc) => KeyLocation {
                    keycode: kc,
                    level: 0,
                },
                None => {
                    log::warn!("no keycode available for keysym {}", keysym::name(ks));
                    return Ok(());
                }
            },
        };
        if keysym::is_modifier(ks) {
            self.held_modifiers.insert(ks);
        }
        let shift_held = self.held_modifiers.contains(&keysym::SHIFT_L)
            || self.held_modifiers.contains(&keysym::SHIFT_R);
        let level3_held = self.held_modifiers.contains(&keysym::ISO_LEVEL3_SHIFT)
            || self.held_modifiers.contains(&keysym::ALT_R);
        let need_shift = loc.level & 1 == 1;
        let need_level3 = loc.level & 2 == 2;

        // Temporarily adjust modifiers when the location needs a different
        // state than the client holds.
        let mut temp_press = Vec::new();
        let mut temp_release = Vec::new();
        if need_shift && !shift_held {
            if let Some(kc) = self.shift_keycode {
                temp_press.push(kc);
            }
        } else if !need_shift && shift_held && !keysym::is_modifier(ks) && loc.level == 0 {
            // Client holds shift but the keysym is the unshifted one (e.g. the
            // client sent an explicit lowercase char while shift is down, which
            // happens with dead keys / composed input). Release shift briefly.
            for m in [keysym::SHIFT_L, keysym::SHIFT_R] {
                if let Some(&kc) = self.pressed.get(&m) {
                    temp_release.push(kc);
                }
            }
        }
        if need_level3 && !level3_held {
            if let Some(kc) = self.level3_keycode {
                temp_press.push(kc);
            }
        }
        for kc in &temp_release {
            self.fake(KEY_RELEASE, *kc, 0, 0)?;
        }
        for kc in &temp_press {
            self.fake(KEY_PRESS, *kc, 0, 0)?;
        }
        self.fake(KEY_PRESS, loc.keycode, 0, 0)?;
        self.pressed.insert(ks, loc.keycode);
        // Temporary modifiers are undone right after the press; the release of
        // the key itself comes later from the client and needs no modifier.
        for kc in temp_press.iter().rev() {
            self.fake(KEY_RELEASE, *kc, 0, 0)?;
        }
        for kc in temp_release.iter().rev() {
            self.fake(KEY_PRESS, *kc, 0, 0)?;
        }
        self.display.flush()
    }

    /// Move the pointer to absolute root coordinates.
    pub fn pointer_move(&mut self, x: i16, y: i16) -> Result<()> {
        self.last_pointer = (x, y);
        self.fake(MOTION, 0, x, y)?;
        self.display.flush()
    }

    /// Press or release a pointer button.
    pub fn button(&mut self, button: u8, down: bool) -> Result<()> {
        if button == 0 {
            return Ok(());
        }
        if down {
            self.pressed_buttons.insert(button);
            self.fake(BUTTON_PRESS, button, 0, 0)?;
        } else {
            self.pressed_buttons.remove(&button);
            self.fake(BUTTON_RELEASE, button, 0, 0)?;
        }
        self.display.flush()
    }

    /// Scroll by whole detents (X buttons 4/5 vertical, 6/7 horizontal).
    pub fn scroll(&mut self, dx: i16, dy: i16) -> Result<()> {
        let clicks = |n: i16| n.unsigned_abs().min(50);
        let vbtn = if dy < 0 { 4 } else { 5 };
        for _ in 0..clicks(dy) {
            self.fake(BUTTON_PRESS, vbtn, 0, 0)?;
            self.fake(BUTTON_RELEASE, vbtn, 0, 0)?;
        }
        let hbtn = if dx < 0 { 6 } else { 7 };
        for _ in 0..clicks(dx) {
            self.fake(BUTTON_PRESS, hbtn, 0, 0)?;
            self.fake(BUTTON_RELEASE, hbtn, 0, 0)?;
        }
        self.display.flush()
    }

    /// Release every key and button the client left pressed (on disconnect).
    pub fn release_all(&mut self) -> Result<()> {
        let keys: Vec<u8> = self.pressed.drain().map(|(_, kc)| kc).collect();
        for kc in keys {
            self.fake(KEY_RELEASE, kc, 0, 0)?;
        }
        self.held_modifiers.clear();
        let buttons: Vec<u8> = self.pressed_buttons.drain().collect();
        for b in buttons {
            self.fake(BUTTON_RELEASE, b, 0, 0)?;
        }
        self.display.flush()
    }

    /// Last pointer position injected.
    pub fn last_pointer(&self) -> (i16, i16) {
        self.last_pointer
    }

    /// Number of keys currently held.
    pub fn pressed_count(&self) -> usize {
        self.pressed.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> Keymap {
        // keycodes 8..=12, 4 syms each.
        let syms = vec![
            0x61,
            0x41,
            0,
            0, // 8: a A
            0x31,
            0x21,
            0,
            0, // 9: 1 !
            keysym::SHIFT_L,
            0,
            0,
            0, // 10
            0,
            0,
            0,
            0, // 11 spare
            0x65,
            0,
            0x20ac,
            0, // 12: e (no explicit E), level3 euro
        ];
        Keymap::from_reply(8, 12, 4, syms)
    }

    #[test]
    fn keymap_lookup() {
        let m = map();
        assert_eq!(
            m.find(0x61),
            Some(KeyLocation {
                keycode: 8,
                level: 0
            })
        );
        assert_eq!(
            m.find(0x41),
            Some(KeyLocation {
                keycode: 8,
                level: 1
            })
        );
        assert_eq!(
            m.find(0x21),
            Some(KeyLocation {
                keycode: 9,
                level: 1
            })
        );
        assert_eq!(
            m.find(0x20ac),
            Some(KeyLocation {
                keycode: 12,
                level: 2
            })
        );
        // implicit uppercase for e
        assert_eq!(
            m.find(0x45),
            Some(KeyLocation {
                keycode: 12,
                level: 1
            })
        );
        assert_eq!(m.plain_keycode(keysym::SHIFT_L), Some(10));
        assert_eq!(m.plain_keycode(0x41), None);
        assert_eq!(m.find(0xff0d), None);
        assert_eq!(m.spare_keycodes(), vec![11]);
    }
}
