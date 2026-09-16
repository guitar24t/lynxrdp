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
use x11rb::protocol::xproto::{
    self, AutoRepeatMode, ChangeKeyboardControlAux, ConnectionExt as _, KeyButMask,
};
use x11rb::protocol::xtest;

use super::XDisplay;

const KEY_PRESS: u8 = xproto::KEY_PRESS_EVENT;
const KEY_RELEASE: u8 = xproto::KEY_RELEASE_EVENT;
const BUTTON_PRESS: u8 = xproto::BUTTON_PRESS_EVENT;
const BUTTON_RELEASE: u8 = xproto::BUTTON_RELEASE_EVENT;
const MOTION: u8 = xproto::MOTION_NOTIFY_EVENT;

/// The lock key, if any, that swaps the two Shift levels of a location.
///
/// xkbcomp types a key from its keysyms alone: a lower/upper case pair is
/// ALPHABETIC and a pair with a keypad keysym is KEYPAD. On both, the lock
/// (Caps Lock, Num Lock) selects the second level by itself and Shift held
/// together with the lock selects the first again. The core map carries no
/// types, but the same rule read off the same keysyms reproduces them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lock {
    None,
    Caps,
    Num,
}

/// Where a keysym lives in the keyboard mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyLocation {
    keycode: u8,
    /// Bit 0: needs Shift; bit 1: needs Level3 (AltGr).
    level: u8,
    /// The lock that, while on, inverts what bit 0 of `level` asks for.
    lock: Lock,
}

/// Parsed keyboard mapping.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    min_keycode: u8,
    max_keycode: u8,
    per_keycode: u8,
    /// Keysym table, `per_keycode` entries per keycode starting at `min_keycode`.
    syms: Vec<u32>,
    /// Canonical keysym -> best locations (lowest level first).
    lookup: HashMap<u32, Vec<KeyLocation>>,
}

/// The one character of a case mapping, or `None` when it expands ('ß' to "SS").
fn single_char(mut it: impl Iterator<Item = char>) -> Option<char> {
    match (it.next(), it.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// Keysym for the uppercase counterpart of a lowercase letter keysym, in the
/// spelling [`Keymap::canonical`] uses.
fn uppercase_keysym(ks: u32) -> Option<u32> {
    let c = keysym::char_from_keysym(ks)?;
    if !c.is_lowercase() {
        return None;
    }
    single_char(c.to_uppercase())
        .filter(|&u| u != c)
        .map(keysym::keysym_from_char)
}

/// Keysym for the lowercase counterpart of an uppercase letter keysym.
fn lowercase_keysym(ks: u32) -> Option<u32> {
    let c = keysym::char_from_keysym(ks)?;
    if !c.is_uppercase() {
        return None;
    }
    single_char(c.to_lowercase())
        .filter(|&l| l != c)
        .map(keysym::keysym_from_char)
}

/// `KP_Space..=KP_Equal`, the range xkbcomp tests to type a key KEYPAD.
fn is_keypad(ks: u32) -> bool {
    (0xff80..=0xffbd).contains(&ks)
}

/// The lock xkbcomp's automatic key types would give a two-level pair.
fn pair_lock(first: u32, second: u32) -> Lock {
    if is_keypad(first) || is_keypad(second) {
        Lock::Num
    } else if second != 0 && uppercase_keysym(first) == Some(Keymap::canonical(second)) {
        Lock::Caps
    } else {
        Lock::None
    }
}

impl Keymap {
    /// The one spelling of a keysym the tables are keyed by.
    ///
    /// A character arrives in whichever encoding its sender had: the client
    /// sends the Unicode form for anything outside Latin-1, while the
    /// session's layout stores the legacy keysym for the same letter
    /// (`aogonek`, `Cyrillic_ef`). Both sides of every lookup pass through
    /// here so that the two spellings meet.
    fn canonical(ks: u32) -> u32 {
        keysym::char_from_keysym(ks)
            .map(keysym::keysym_from_char)
            .unwrap_or(ks)
    }

    /// Build from a `GetKeyboardMapping` reply.
    pub fn from_reply(min_keycode: u8, max_keycode: u8, per_keycode: u8, syms: Vec<u32>) -> Self {
        let per = usize::from(per_keycode);
        let count = usize::from(max_keycode) - usize::from(min_keycode) + 1;
        let row = |kc_idx: usize| syms.get(kc_idx * per..(kc_idx + 1) * per).unwrap_or(&[]);
        let col = |r: &[u32], i: usize| r.get(i).copied().unwrap_or(0);
        // The core export of an XKB map puts the first two levels of group 1
        // in columns 0-1 and of group 2 in columns 2-3, then whatever levels
        // remain of each group in turn. A key with a single group has that
        // group copied into columns 2-3. So a map is either one layout
        // throughout, with level 3 of every key at column 4, or carries a
        // second layout, in which case column 4 belongs to whichever group
        // has more than two levels and the core map alone cannot say which;
        // those characters are left to dynamic binding rather than typed on
        // the wrong key. (Read back from Xvfb: `pl` exports
        // `a A a A aogonek Aogonek`, `us,ru` exports
        // `a A Cyrillic_ef Cyrillic_EF`, and `us,de` exports
        // `q Q q Q at Greek_OMEGA` with the `at` on the German side.)
        let two_groups = (0..count).any(|i| {
            let r = row(i);
            let g2 = (col(r, 2), col(r, 3));
            g2 != (0, 0) && g2 != (col(r, 0), col(r, 1))
        });
        // Level 3 is only reachable through a key that switches to it. On a
        // map without ISO_Level3_Shift, Alt_R is a plain Mod1 Alt that opens
        // menus rather than reaching level 3, so such a map's level-3
        // characters are bound to spares instead.
        let has_level3 = (0..count).any(|i| col(row(i), 0) == keysym::ISO_LEVEL3_SHIFT);
        let mut lookup: HashMap<u32, Vec<KeyLocation>> = HashMap::new();
        for kc_idx in 0..count {
            let keycode = min_keycode.wrapping_add(kc_idx as u8);
            let r = row(kc_idx);
            let plain = col(r, 0);
            let mut shifted = col(r, 1);
            // A row holding a lone lowercase letter means [lower, upper] to
            // the core protocol. An XKB server materialises the pair before
            // exporting, so the rule only applies where nothing else in the
            // row could say otherwise: a one-level key that deliberately
            // ignores Shift comes out as `x NoSymbol x` and must not gain an
            // uppercase it cannot type.
            if shifted == 0 && r.iter().skip(1).all(|&s| s == 0) {
                if let Some(up) = uppercase_keysym(plain) {
                    shifted = up;
                }
            }
            let (third, fourth) = if two_groups || !has_level3 {
                (0, 0)
            } else {
                (col(r, 4), col(r, 5))
            };
            for (pair, base) in [((plain, shifted), 0u8), ((third, fourth), 2u8)] {
                let lock = pair_lock(pair.0, pair.1);
                for (i, ks) in [pair.0, pair.1].into_iter().enumerate() {
                    if ks == 0 {
                        continue;
                    }
                    lookup
                        .entry(Self::canonical(ks))
                        .or_default()
                        .push(KeyLocation {
                            keycode,
                            level: base + i as u8,
                            lock,
                        });
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
        self.lookup
            .get(&Self::canonical(ks))
            .and_then(|v| v.first().copied())
    }

    /// Keycode for a keysym that must be pressable without modifiers
    /// (used for modifiers themselves).
    fn plain_keycode(&self, ks: u32) -> Option<u8> {
        self.lookup
            .get(&Self::canonical(ks))?
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

/// The held key a release spelled differently from its press belongs to:
/// the entry of `pressed` to release for `ks`, when no entry is `ks` itself.
///
/// A client that lets go of Shift or AltGr before the letter reports the
/// release under the unshifted spelling ('a' for a press of 'A', 'q' for a
/// press of '@' on a German client). Matched by keysym alone, the keycode
/// then stays down for the rest of the connection, and the X server drops
/// every later press of it. So the pressed key is found by the keycode the
/// release resolves to, and failing that by case counterpart, which is the
/// one relation a spare keycode preserves ('Я' and 'я' are bound apart).
fn held_counterpart(keymap: &Keymap, pressed: &HashMap<u32, u8>, ks: u32) -> Option<u32> {
    let by_keycode = keymap.find(ks).and_then(|loc| {
        pressed
            .iter()
            .find(|(_, kc)| **kc == loc.keycode)
            .map(|(s, _)| *s)
    });
    by_keycode.or_else(|| {
        [uppercase_keysym(ks), lowercase_keysym(ks)]
            .into_iter()
            .flatten()
            .find(|c| pressed.contains_key(c))
    })
}

/// The temporary modifier changes a press needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ModifierPlan {
    press_shift: bool,
    release_shift: bool,
    press_level3: bool,
}

/// Decide what to do with Shift and Level3 around a press so that `loc`
/// produces its keysym, given what the client holds and what the server has
/// locked.
///
/// The lock inverts the Shift decision on the keys it applies to. With Caps
/// Lock on, a client types "hello" and sends 'H', 'E', ... (its own OS has
/// already applied the lock): pressing Shift for the uppercase would give
/// lowercase, because Shift+Lock on an alphabetic key is the first level,
/// so the press wanted is the plain one. The same holds for Num Lock and
/// the keypad: `KP_7` is the plain press with the lock on and a shifted
/// press with it off.
fn plan_modifiers(
    loc: KeyLocation,
    is_modifier: bool,
    shift_held: bool,
    level3_held: bool,
    caps_on: bool,
    num_on: bool,
) -> ModifierPlan {
    let lock_on = match loc.lock {
        Lock::None => false,
        Lock::Caps => caps_on,
        Lock::Num => num_on,
    };
    let want_shift = (loc.level & 1 == 1) != lock_on;
    let want_level3 = loc.level & 2 == 2;
    ModifierPlan {
        press_shift: want_shift && !shift_held,
        // The client holds Shift but wants the unshifted keysym: an explicit
        // lowercase character while Shift is down happens with dead keys and
        // composed input, and an AltGr character with Shift down would come
        // out as level 4. A modifier press is never wrapped this way -- the
        // client's held Shift is its own to manage.
        release_shift: !want_shift && shift_held && !is_modifier,
        press_level3: want_level3 && !level3_held,
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
    /// Real modifier bit Num Lock is bound to, or 0 when it is not one.
    num_lock_mask: u16,
    /// Canonical keysym -> keycode used for the press, so the release matches.
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
    /// Whether auto-repeat is currently suppressed for a connected client.
    suppressed: bool,
    /// The server's global auto-repeat setting as it was when we last
    /// suppressed it, so that a session we suppressed it for gets back
    /// exactly what it had. Read rather than assumed to be on: a user who
    /// turned key repeat off would otherwise find us turning it back on.
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
            num_lock_mask: 0,
            pressed: HashMap::new(),
            held_modifiers: HashSet::new(),
            pressed_buttons: HashSet::new(),
            spares: Vec::new(),
            dynamic: HashMap::new(),
            dynamic_order: Vec::new(),
            last_pointer: (0, 0),
            suppressed: false,
            original_auto_repeat,
        };
        s.reload_keymap()?;
        Ok(s)
    }

    /// Turn the X server's own key auto-repeat off while a client is connected.
    ///
    /// A held key otherwise has two repeat generators that know nothing about
    /// each other. The client's operating system repeats the key and forwards
    /// every repeat to us as another `KeyEvent`, and `XTestFakeInput` leaves
    /// the keycode logically held, which entitles the X server to repeat it
    /// as well. The visible results are a repeat rate that is neither end's
    /// configured one on arrows and Backspace, and a key that runs away
    /// entirely whenever a `KeyRelease` is delayed past X's 660 ms threshold
    /// by a stalled tunnel. x11vnc ships `-norepeat` and turns server-side
    /// repeat off by default for exactly this reasoning.
    ///
    /// Applied per connection rather than once at startup because a desktop's
    /// settings daemon (gnome-settings-daemon, xfsettingsd, kded) applies the
    /// user's keyboard preferences some seconds into login, asynchronously,
    /// and would simply overwrite a value set before it ran. The first
    /// connection of a session is accepted inside that same window, which is
    /// why [`InputInjector::reassert_auto_repeat_suppression`] exists.
    ///
    /// The setting is read again here, not taken from the constructor: by
    /// the time a client connects the settings daemon may have applied the
    /// user's preference, and [`InputInjector::restore_auto_repeat`] must
    /// hand back that, not the X server's startup default.
    pub fn suppress_auto_repeat(&mut self) {
        // A second suppression without a restore between (a client replaced
        // in place) would otherwise read our own OFF as the user's setting.
        if !self.suppressed {
            if let Some(mode) = read_auto_repeat(&self.display) {
                self.original_auto_repeat = Some(mode);
            }
            self.suppressed = true;
        }
        self.set_auto_repeat(AutoRepeatMode::OFF);
    }

    /// Put auto-repeat back off if something turned it on since
    /// [`InputInjector::suppress_auto_repeat`]. For the engine's housekeeping
    /// tick while a client is connected; does nothing otherwise.
    ///
    /// The OFF written when the session's first client is accepted lands a
    /// few seconds before the desktop's settings daemon applies the user's
    /// keyboard preferences over it, which hands that whole first connection
    /// the runaway-repeat case suppression exists for. A value found on here
    /// is that daemon's doing and therefore the user's preference, so it is
    /// also what restore should return.
    pub fn reassert_auto_repeat_suppression(&mut self) {
        if !self.suppressed {
            return;
        }
        let Some(mode) = read_auto_repeat(&self.display) else {
            return;
        };
        if mode == AutoRepeatMode::OFF {
            return;
        }
        log::debug!("keyboard auto-repeat was turned back on; suppressing it again");
        self.original_auto_repeat = Some(mode);
        self.set_auto_repeat(AutoRepeatMode::OFF);
    }

    /// Put the auto-repeat setting back to whatever it was when suppressed.
    pub fn restore_auto_repeat(&mut self) {
        self.suppressed = false;
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
        // No Alt_R fallback: `Keymap::from_reply` leaves level 3 out of a map
        // without ISO_Level3_Shift, so nothing asks for this key there, and
        // pressing a Mod1 Alt in its place would open menus instead.
        self.level3_keycode = self.keymap.plain_keycode(keysym::ISO_LEVEL3_SHIFT);
        self.num_lock_mask = self.num_lock_mask();
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
            "keymap loaded: keycodes {min}-{max}, {} spare, shift={:?} level3={:?} numlock=0x{:x}",
            self.spares.len(),
            self.shift_keycode,
            self.level3_keycode,
            self.num_lock_mask
        );
        Ok(())
    }

    /// The real modifier bit Num Lock is bound to, or 0 when it is not one.
    ///
    /// The Lock bit is fixed by the core protocol, but NumLock is a virtual
    /// modifier that XKB maps to whichever real modifier the map says --
    /// Mod2 on stock maps and not necessarily elsewhere -- so the answer
    /// comes from the modifier mapping rather than from convention.
    fn num_lock_mask(&self) -> u16 {
        let Some(kc) = self.keymap.plain_keycode(keysym::NUM_LOCK) else {
            return 0;
        };
        let reply = self
            .display
            .conn()
            .get_modifier_mapping()
            .map_err(anyhow::Error::from)
            .and_then(|c| c.reply().map_err(anyhow::Error::from));
        let reply = match reply {
            Ok(r) => r,
            Err(e) => {
                log::warn!("cannot read the modifier mapping: {e:#}");
                return 0;
            }
        };
        let per = usize::from(reply.keycodes_per_modifier());
        (0..8usize)
            .find(|&i| {
                reply
                    .keycodes
                    .get(i * per..(i + 1) * per)
                    .is_some_and(|k| k.contains(&kc))
            })
            .map_or(0, |i| 1 << i)
    }

    /// Whether Caps Lock and Num Lock are locked on the server right now.
    ///
    /// Asked for rather than tracked: the client is not the only party that
    /// toggles them -- the desktop restores Num Lock at login, and an
    /// on-screen keyboard or `xdotool` can flip either -- and a stale answer
    /// types the wrong case. One round trip per letter is cheap against a
    /// loopback X server. A failed query reads as both off, which is what the
    /// code assumed before it learned to look.
    fn locks(&self) -> (bool, bool) {
        let mask = self
            .display
            .conn()
            .query_pointer(self.display.root())
            .map_err(anyhow::Error::from)
            .and_then(|c| c.reply().map_err(anyhow::Error::from))
            .map(|r| u16::from(r.mask));
        match mask {
            Ok(m) => (
                m & u16::from(KeyButMask::LOCK) != 0,
                self.num_lock_mask != 0 && m & self.num_lock_mask != 0,
            ),
            Err(e) => {
                log::debug!("cannot read the lock state, assuming both off: {e:#}");
                (false, false)
            }
        }
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

    /// Bind `ks` (canonical) to a spare keycode, or recycle the oldest
    /// dynamic binding that is not held.
    fn bind_dynamic(&mut self, ks: u32) -> Result<Option<u8>> {
        let keycode = if let Some(kc) = self.spares.pop() {
            kc
        } else {
            // A keycode the client still holds cannot change meaning under
            // it, but that is no reason to drop the key when a younger
            // binding is free.
            let Some(pos) = self
                .dynamic_order
                .iter()
                .position(|old| !self.pressed.contains_key(old))
            else {
                return Ok(None);
            };
            let old = self.dynamic_order.remove(pos);
            let Some(kc) = self.dynamic.remove(&old) else {
                return Ok(None);
            };
            // Forget the evicted keysym's location now rather than at the
            // next MappingNotify: key messages already queued behind this one
            // would otherwise find the recycled keycode and type the new
            // character in place of the old one.
            let emptied = self.keymap.lookup.get_mut(&old).is_some_and(|locs| {
                locs.retain(|l| l.keycode != kc);
                locs.is_empty()
            });
            if emptied {
                self.keymap.lookup.remove(&old);
            }
            kc
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
        self.keymap.lookup.entry(ks).or_default().insert(
            0,
            KeyLocation {
                keycode,
                level: 0,
                lock: Lock::None,
            },
        );
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
        let ks = Keymap::canonical(ks);
        if !down {
            return self.release(ks);
        }
        if keysym::is_modifier(ks) && self.pressed.contains_key(&ks) {
            // A repeat of a held modifier changes nothing, and releasing it
            // around the re-press would drop the modifier for an instant.
            return Ok(());
        }
        // The X server discards a press of a keycode that is already down
        // (checked against Xvfb, repeat on or off), and the client repeats
        // keys itself because server-side repeat is off while it is
        // connected. Releasing a held key before pressing it again is what
        // turns the client's repeat into one here, and it also heals a key
        // whose release never matched its press.
        if let Some(kc) = self.pressed.remove(&ks) {
            self.fake(KEY_RELEASE, kc, 0, 0)?;
        }

        let loc = match self.keymap.find(ks) {
            Some(l) => l,
            None => match self.bind_dynamic(ks)? {
                Some(kc) => KeyLocation {
                    keycode: kc,
                    level: 0,
                    lock: Lock::None,
                },
                None => {
                    log::warn!("no keycode available for keysym {}", keysym::name(ks));
                    return Ok(());
                }
            },
        };
        // The same keycode held under another spelling ('a' down when 'A'
        // arrives because Shift went down meanwhile) is the same physical key
        // to the server, which would discard the press just the same.
        let other = self
            .pressed
            .iter()
            .find(|(_, kc)| **kc == loc.keycode)
            .map(|(s, _)| *s);
        if let Some(other) = other {
            self.pressed.remove(&other);
            self.fake(KEY_RELEASE, loc.keycode, 0, 0)?;
        }
        if keysym::is_modifier(ks) {
            self.held_modifiers.insert(ks);
        }
        let shift_held = self.held_modifiers.contains(&keysym::SHIFT_L)
            || self.held_modifiers.contains(&keysym::SHIFT_R);
        let level3_held = self.held_modifiers.contains(&keysym::ISO_LEVEL3_SHIFT)
            || self.held_modifiers.contains(&keysym::ALT_R);
        let (caps_on, num_on) = if loc.lock == Lock::None {
            (false, false)
        } else {
            self.locks()
        };
        let plan = plan_modifiers(
            loc,
            keysym::is_modifier(ks),
            shift_held,
            level3_held,
            caps_on,
            num_on,
        );

        // Temporarily adjust modifiers when the location needs a different
        // state than the client holds.
        let mut temp_press = Vec::new();
        let mut temp_release = Vec::new();
        if plan.press_shift {
            if let Some(kc) = self.shift_keycode {
                temp_press.push(kc);
            }
        }
        if plan.release_shift {
            for m in [keysym::SHIFT_L, keysym::SHIFT_R] {
                if let Some(&kc) = self.pressed.get(&m) {
                    temp_release.push(kc);
                }
            }
        }
        if plan.press_level3 {
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

    /// Release the key a (canonical) keysym's release belongs to.
    fn release(&mut self, ks: u32) -> Result<()> {
        self.held_modifiers.remove(&ks);
        let mut held = self.pressed.remove(&ks);
        if held.is_none() {
            if let Some(other) = held_counterpart(&self.keymap, &self.pressed, ks) {
                held = self.pressed.remove(&other);
            }
        }
        let keycode = match held {
            Some(kc) => kc,
            // Release without press (e.g. pressed before connect): best
            // effort on the key it would map to. A release of a key that is
            // not down is dropped by the server, so this cannot harm.
            None => match self.keymap.find(ks) {
                Some(loc) => loc.keycode,
                None => return Ok(()),
            },
        };
        self.fake(KEY_RELEASE, keycode, 0, 0)?;
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

    const KP_HOME: u32 = keysym::KP_HOME;
    const KP_7: u32 = keysym::KP_0 + 7;
    const CYRILLIC_EF: u32 = 0x06c6;
    const CYRILLIC_EF_UPPER: u32 = 0x06e6;
    const AOGONEK: u32 = 0x01b1;
    const AOGONEK_UPPER: u32 = 0x01a1;

    fn ks(c: char) -> u32 {
        keysym::keysym_from_char(c)
    }

    fn at(keycode: u8, level: u8, lock: Lock) -> Option<KeyLocation> {
        Some(KeyLocation {
            keycode,
            level,
            lock,
        })
    }

    /// Rows shaped the way an XKB server exports a single-layout map: the
    /// first two levels, the same two again for the copied second group,
    /// then levels 3-4 (`xmodmap -pke` on Xvfb with `setxkbmap pl` shows
    /// `a A a A aogonek Aogonek`).
    fn map() -> Keymap {
        #[rustfmt::skip]
        let syms = vec![
            0x61, 0x41, 0x61, 0x41, AOGONEK, AOGONEK_UPPER, // 8: a A, AltGr ą Ą
            0x31, 0x21, 0x31, 0x21, 0, 0,                   // 9: 1 !
            keysym::SHIFT_L, 0, keysym::SHIFT_L, 0, 0, 0,   // 10
            0, 0, 0, 0, 0, 0,                               // 11: spare
            0x65, 0, 0, 0, 0, 0,                            // 12: e alone, core style
            keysym::ISO_LEVEL3_SHIFT, 0, keysym::ISO_LEVEL3_SHIFT, 0, 0, 0, // 13
            KP_HOME, KP_7, KP_HOME, KP_7, 0, 0,             // 14: keypad 7
            CYRILLIC_EF, CYRILLIC_EF_UPPER, CYRILLIC_EF, CYRILLIC_EF_UPPER, 0, 0, // 15: ф Ф
            0x71, 0x51, 0x71, 0x51, 0x40, 0x07d9,           // 16: q Q, AltGr @ Ω (Greek_OMEGA)
            0xe9, 0, 0xe9, 0, 0, 0,                         // 17: é on a one-level key
        ];
        Keymap::from_reply(8, 17, 6, syms)
    }

    #[test]
    fn keymap_lookup() {
        let m = map();
        assert_eq!(m.find(0x61), at(8, 0, Lock::Caps));
        assert_eq!(m.find(0x41), at(8, 1, Lock::Caps));
        assert_eq!(m.find(0x21), at(9, 1, Lock::None));
        assert_eq!(m.find(0x40), at(16, 2, Lock::None));
        assert_eq!(m.find(ks('Ω')), at(16, 3, Lock::None));
        assert_eq!(m.find(0x07d9), at(16, 3, Lock::None));
        // Implicit uppercase for a lone lowercase letter, but not for a
        // one-level key the server exported with its copy in column 2.
        assert_eq!(m.find(0x45), at(12, 1, Lock::Caps));
        assert_eq!(m.find(0x65), at(12, 0, Lock::Caps));
        assert_eq!(m.find(0xe9), at(17, 0, Lock::None));
        assert_eq!(m.find(0xc9), None);
        assert_eq!(m.find(KP_7), at(14, 1, Lock::Num));
        assert_eq!(m.find(KP_HOME), at(14, 0, Lock::Num));
        assert_eq!(m.plain_keycode(keysym::SHIFT_L), Some(10));
        assert_eq!(m.plain_keycode(0x41), None);
        assert_eq!(m.find(0xff0d), None);
        assert_eq!(m.spare_keycodes(), vec![11]);
    }

    /// The client sends the Unicode form of a letter; the layout stores the
    /// legacy keysym. Both spellings must land on the same key, and the
    /// implicit-uppercase rule must understand the legacy spelling too.
    #[test]
    fn legacy_and_unicode_spellings_find_the_same_key() {
        let m = map();
        assert_eq!(m.find(ks('ą')), at(8, 2, Lock::Caps));
        assert_eq!(m.find(AOGONEK), at(8, 2, Lock::Caps));
        assert_eq!(m.find(ks('Ą')), at(8, 3, Lock::Caps));
        assert_eq!(m.find(ks('ф')), at(15, 0, Lock::Caps));
        assert_eq!(m.find(CYRILLIC_EF), at(15, 0, Lock::Caps));
        assert_eq!(m.find(ks('Ф')), at(15, 1, Lock::Caps));
        assert_eq!(m.find(CYRILLIC_EF_UPPER), at(15, 1, Lock::Caps));
        // A lone legacy lowercase letter gains its uppercase as well.
        let lone = Keymap::from_reply(8, 8, 4, vec![CYRILLIC_EF, 0, 0, 0]);
        assert_eq!(lone.find(ks('Ф')), at(8, 1, Lock::Caps));
    }

    /// With two layouts loaded, columns 2-3 are the second group and column
    /// 4 belongs to whichever group has more levels; neither may be typed on
    /// the first group's key, so both are left for dynamic binding.
    #[test]
    fn second_group_columns_are_not_mistaken_for_level3() {
        #[rustfmt::skip]
        let syms = vec![
            0x61, 0x41, CYRILLIC_EF, CYRILLIC_EF_UPPER, 0xe4, 0xc4, // 8: us,de-style row
            keysym::ISO_LEVEL3_SHIFT, 0, keysym::ISO_LEVEL3_SHIFT, 0, 0, 0,
        ];
        let m = Keymap::from_reply(8, 9, 6, syms);
        assert_eq!(m.find(0x61), at(8, 0, Lock::Caps));
        assert_eq!(m.find(ks('ф')), None);
        assert_eq!(m.find(0xe4), None);
    }

    /// Without a Level3 key, level-3 characters are unreachable and Alt_R is
    /// not a substitute: it is Mod1 and would open a menu instead.
    #[test]
    fn level3_needs_a_level3_key() {
        #[rustfmt::skip]
        let syms = vec![
            0x61, 0x41, 0x61, 0x41, 0xe4, 0xc4,
            keysym::ALT_R, 0, keysym::ALT_R, 0, 0, 0,
        ];
        let m = Keymap::from_reply(8, 9, 6, syms);
        assert_eq!(m.find(0xe4), None);
        assert_eq!(m.plain_keycode(keysym::ISO_LEVEL3_SHIFT), None);
    }

    #[test]
    fn pair_locks_follow_xkbcomp_automatic_types() {
        assert_eq!(pair_lock(0x61, 0x41), Lock::Caps);
        assert_eq!(pair_lock(CYRILLIC_EF, CYRILLIC_EF_UPPER), Lock::Caps);
        assert_eq!(pair_lock(0x31, 0x21), Lock::None);
        assert_eq!(pair_lock(0xe9, 0xe9), Lock::None);
        assert_eq!(pair_lock(0xdf, 0x3f), Lock::None); // ß ? -- no single uppercase
        assert_eq!(pair_lock(KP_HOME, KP_7), Lock::Num);
        assert_eq!(pair_lock(keysym::KP_DECIMAL, keysym::KP_DELETE), Lock::Num);
        assert_eq!(pair_lock(0x61, 0), Lock::None);
    }

    fn alpha(level: u8) -> KeyLocation {
        KeyLocation {
            keycode: 38,
            level,
            lock: Lock::Caps,
        }
    }

    /// The decision finding 14 turns on: with Caps Lock on, the client sends
    /// 'H' for the h key and Shift+h arrives as 'h', and pressing Shift for
    /// the former or leaving it held for the latter inverts the case.
    #[test]
    fn caps_lock_inverts_the_shift_decision_on_alphabetic_keys() {
        let plain = ModifierPlan::default();
        let press = ModifierPlan {
            press_shift: true,
            ..plain
        };
        let release = ModifierPlan {
            release_shift: true,
            ..plain
        };
        // Lock off: 'A' needs Shift, 'a' with Shift held needs it let go.
        assert_eq!(
            plan_modifiers(alpha(1), false, false, false, false, false),
            press
        );
        assert_eq!(
            plan_modifiers(alpha(0), false, false, false, false, false),
            plain
        );
        assert_eq!(
            plan_modifiers(alpha(0), false, true, false, false, false),
            release
        );
        assert_eq!(
            plan_modifiers(alpha(1), false, true, false, false, false),
            plain
        );
        // Lock on: 'A' is the plain press and 'a' the shifted one.
        assert_eq!(
            plan_modifiers(alpha(1), false, false, false, true, false),
            plain
        );
        assert_eq!(
            plan_modifiers(alpha(0), false, false, false, true, false),
            press
        );
        assert_eq!(
            plan_modifiers(alpha(0), false, true, false, true, false),
            plain
        );
        assert_eq!(
            plan_modifiers(alpha(1), false, true, false, true, false),
            release
        );
        // Caps Lock means nothing to a key that is not alphabetic.
        let bang = KeyLocation {
            keycode: 10,
            level: 1,
            lock: Lock::None,
        };
        assert_eq!(
            plan_modifiers(bang, false, false, false, true, false),
            press
        );
        // Nor does it to the keypad, which answers to Num Lock instead.
        let kp7 = KeyLocation {
            keycode: 79,
            level: 1,
            lock: Lock::Num,
        };
        let kp_home = KeyLocation { level: 0, ..kp7 };
        assert_eq!(plan_modifiers(kp7, false, false, false, true, false), press);
        assert_eq!(plan_modifiers(kp7, false, false, false, false, true), plain);
        assert_eq!(
            plan_modifiers(kp_home, false, false, false, false, true),
            press
        );
        assert_eq!(
            plan_modifiers(kp_home, false, false, false, false, false),
            plain
        );
    }

    #[test]
    fn level3_and_modifier_presses() {
        let at_sign = KeyLocation {
            keycode: 24,
            level: 2,
            lock: Lock::None,
        };
        assert_eq!(
            plan_modifiers(at_sign, false, false, false, false, false),
            ModifierPlan {
                press_level3: true,
                ..ModifierPlan::default()
            }
        );
        assert_eq!(
            plan_modifiers(at_sign, false, false, true, false, false),
            ModifierPlan::default()
        );
        // Shift held with an AltGr character wanted: level 4 would come out.
        assert_eq!(
            plan_modifiers(at_sign, false, true, false, false, false),
            ModifierPlan {
                release_shift: true,
                press_level3: true,
                press_shift: false,
            }
        );
        // A modifier press is never wrapped in a Shift release.
        let shift_r = KeyLocation {
            keycode: 62,
            level: 0,
            lock: Lock::None,
        };
        assert_eq!(
            plan_modifiers(shift_r, true, true, false, false, false),
            ModifierPlan::default()
        );
    }

    /// Finding 13: a release spelled differently from its press must still
    /// find the held key -- by keycode when both spellings share one, by
    /// case when a spare keycode holds the letter.
    #[test]
    fn releases_find_their_press_under_another_spelling() {
        let m = map();
        let mut pressed = HashMap::new();
        pressed.insert(0x41, 8u8); // 'A' down
        assert_eq!(held_counterpart(&m, &pressed, 0x61), Some(0x41));
        pressed.clear();
        pressed.insert(0x40, 16); // '@' down; 'q' shares the key
        assert_eq!(held_counterpart(&m, &pressed, 0x71), Some(0x40));
        pressed.clear();
        pressed.insert(ks('Я'), 200); // dynamically bound, no key for 'я'
        assert_eq!(held_counterpart(&m, &pressed, ks('я')), Some(ks('Я')));
        assert_eq!(held_counterpart(&m, &pressed, ks('Ю')), None);
        pressed.clear();
        pressed.insert(keysym::SHIFT_L, 10);
        assert_eq!(held_counterpart(&m, &pressed, 0x61), None);
    }
}
