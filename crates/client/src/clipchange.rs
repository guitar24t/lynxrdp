//! Deciding when the local clipboard is worth reading.
//!
//! Reading it is not free. `arboard::Clipboard::get_image` transfers the whole
//! image out of the window system and decodes it before the caller can so much
//! as ask whether it changed, and the caller then hashes every byte to find
//! out: 37-54 ms for a 4K screenshot, every 700 ms, on the winit thread that
//! also decodes frames and answers `FrameAck`. The session shares that thread,
//! so the cost is not hidden anywhere -- it is a periodic stall in a remote
//! desktop.
//!
//! Two of the three platforms will simply tell us. Windows keeps a clipboard
//! sequence number and macOS an `NSPasteboard` change count; each is one call
//! that touches no clipboard data at all, and each moves on *any* copy, so a
//! single read of the counter answers for the text poll and the image poll
//! together.
//!
//! X11 and Wayland have nothing equivalent that is safe to use here. XFIXES
//! will report a change of selection owner, but `arboard` is built with
//! `wayland-data-control` and takes the Wayland protocol wherever one exists,
//! so an XFIXES watcher would be reporting on a selection the clipboard code
//! is not reading: wrong, and confidently silent about it. What is left is to
//! look less often when looking keeps finding nothing, and to look again at
//! once when something turns up or when the user comes back to the window.
//! Text keeps its steady interval there -- `get_text` moves a few kilobytes,
//! not a screenshot -- and only the image poll backs off.

use std::time::{Duration, Instant};

/// Shortest gap between image polls on a platform with no change counter.
///
/// Equal to the caller's own poll interval, so the first look after a copy is
/// as prompt as it was before any of this existed.
pub const MIN_IMAGE_POLL: Duration = Duration::from_millis(700);

/// Longest gap between image polls on a platform with no change counter.
///
/// The ceiling is what an idle session settles at, so it is the number that
/// decides how much a session costs when nobody is copying anything. Five
/// seconds is also the worst case for noticing an image copied in another
/// application -- acceptable because only the *offer* is delayed: the bytes
/// move when the session asks for them, and a paste inside the session is
/// preceded by the user returning to the window, which resets the backoff.
pub const MAX_IMAGE_POLL: Duration = Duration::from_secs(5);

/// Whether this build *might* have a platform change counter.
///
/// Compile-time only, and deliberately not the thing the watcher branches on:
/// whether a counter actually answers is decided per call, so a Windows
/// station without clipboard access degrades to the backoff rather than to a
/// clipboard that appears never to change.
pub const HAS_COUNTER: bool = cfg!(any(windows, target_os = "macos"));

/// What is worth reading on this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look {
    /// Read the clipboard text.
    pub text: bool,
    /// Read the clipboard image -- the expensive one.
    pub image: bool,
}

/// Rising interval for a platform that cannot be asked.
///
/// Kept separate from the watcher, and free of any platform call, so the
/// schedule can be tested on a machine that has a counter and would otherwise
/// never take this path.
#[derive(Clone, Copy, Debug)]
struct Backoff {
    interval: Duration,
    next: Instant,
}

impl Backoff {
    fn new(now: Instant) -> Self {
        Self {
            interval: MIN_IMAGE_POLL,
            next: now,
        }
    }

    fn due(&self, now: Instant) -> bool {
        now >= self.next
    }

    /// Record what a look found and schedule the next one.
    fn looked(&mut self, found_change: bool, now: Instant) {
        self.interval = if found_change {
            MIN_IMAGE_POLL
        } else {
            (self.interval * 2).min(MAX_IMAGE_POLL)
        };
        self.next = now + self.interval;
    }

    /// Look again immediately, whatever the schedule said.
    fn reset(&mut self, now: Instant) {
        self.interval = MIN_IMAGE_POLL;
        self.next = now;
    }
}

/// Tracks whether the local clipboard is worth reading.
#[derive(Debug)]
pub struct ClipboardWatcher {
    /// Last value of the platform change counter, where there is one.
    last: Option<u64>,
    backoff: Backoff,
}

impl ClipboardWatcher {
    /// Start watching. The first tick always reads: whatever is on the
    /// clipboard when a session opens has never been offered to it.
    pub fn new(now: Instant) -> Self {
        Self {
            last: None,
            backoff: Backoff::new(now),
        }
    }

    /// Decide what to read this tick.
    pub fn tick(&mut self, now: Instant) -> Look {
        self.tick_with(change_counter(), now)
    }

    /// Report whether the image read found something new.
    ///
    /// The schedule is kept even where a counter exists -- it costs two field
    /// writes and it is never consulted there -- rather than being skipped
    /// under a `cfg`, because a schedule that only advances on the platforms
    /// nobody develops on is a schedule with no tests.
    pub fn image_read(&mut self, found_change: bool, now: Instant) {
        self.backoff.looked(found_change, now);
    }

    /// The user came back to the window, so look again without waiting.
    ///
    /// Only the backoff needs this. A change counter is already exact -- if
    /// nothing was copied while the window was away it has not moved, and
    /// forcing a read here would put the full cost of a 4K `get_image` on
    /// every alt-tab, which is precisely the cost this module exists to avoid.
    pub fn wake(&mut self, now: Instant) {
        self.backoff.reset(now);
    }

    /// The decision, given whatever the platform counter says.
    ///
    /// Split out so the counter path can be tested without one, and the
    /// backoff path can be tested on a machine that has one.
    fn tick_with(&mut self, counter: Option<u64>, now: Instant) -> Look {
        match counter {
            Some(c) => {
                // Any copy moves the counter, so one comparison covers both
                // formats -- and text becomes *more* responsive than the old
                // unconditional poll, not less, because nothing is read at all
                // until something has actually been copied.
                let changed = self.last != Some(c);
                self.last = Some(c);
                Look {
                    text: changed,
                    image: changed,
                }
            }
            None => Look {
                text: true,
                image: self.backoff.due(now),
            },
        }
    }
}

/// The platform's clipboard change counter, if it has one.
///
/// The value is opaque: only "differs from last time" is ever asked of it, so
/// wrapping, a reboot or a different width between platforms cost nothing.
#[cfg(windows)]
fn change_counter() -> Option<u64> {
    use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;
    // SAFETY: no arguments, no pointers, and it is documented as callable from
    // any thread without opening the clipboard -- which is the whole point,
    // since opening it is what we are trying not to do.
    let n = unsafe { GetClipboardSequenceNumber() };
    // Zero is the documented failure return -- a window station without
    // WINSTA_ACCESSCLIPBOARD. Reported as a value it would be indistinguishable
    // from a clipboard that never changes, and clipboard sync would stop dead
    // for the life of the session without a word. Reported as "no counter" it
    // costs nothing but a fall back to the polling everything else does.
    (n != 0).then(|| u64::from(n))
}

/// See the Windows note above.
///
/// `changeCount` is a plain property read on the shared pasteboard object; it
/// does not fault the pasteboard's data in, which is what makes it worth
/// calling in preference to `get_image`.
#[cfg(target_os = "macos")]
fn change_counter() -> Option<u64> {
    use objc2_app_kit::NSPasteboard;
    // The count only ever increases, so the cast is a formality; it is done
    // with `as` on the two's-complement value rather than a fallible
    // conversion because a negative count is not a case worth a branch --
    // the caller compares for inequality and nothing else.
    Some(NSPasteboard::generalPasteboard().changeCount() as u64)
}

/// X11 and Wayland: see the module comment for why there is nothing to return
/// here rather than an XFIXES watcher.
#[cfg(not(any(windows, target_os = "macos")))]
fn change_counter() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_gates_both_formats_on_one_comparison() {
        let t0 = Instant::now();
        let mut w = ClipboardWatcher::new(t0);
        // The first look always reads: the clipboard may hold something from
        // before this session and the session has never seen it.
        assert_eq!(
            w.tick_with(Some(7), t0),
            Look {
                text: true,
                image: true
            }
        );
        // Nothing copied, so nothing is read -- no matter how long we wait,
        // which is the difference from the backoff path.
        for i in 1..20 {
            assert_eq!(
                w.tick_with(Some(7), t0 + Duration::from_secs(i)),
                Look {
                    text: false,
                    image: false
                }
            );
        }
        assert_eq!(
            w.tick_with(Some(8), t0 + Duration::from_secs(20)),
            Look {
                text: true,
                image: true
            }
        );
    }

    #[test]
    fn without_a_counter_text_keeps_its_interval_and_the_image_backs_off() {
        let t0 = Instant::now();
        let mut w = ClipboardWatcher::new(t0);
        // Text is never gated here: `get_text` moves a few kilobytes and the
        // caller's own 700 ms tick is the only pacing it needs. Delaying it
        // would make pasting text into the session feel broken.
        let mut at = t0;
        for _ in 0..10 {
            assert!(w.tick_with(None, at).text);
            at += Duration::from_secs(1);
        }
    }

    #[test]
    fn a_fruitless_image_poll_doubles_the_gap_up_to_the_ceiling() {
        let t0 = Instant::now();
        let mut w = ClipboardWatcher::new(t0);
        assert!(w.tick_with(None, t0).image, "the first look is always due");
        w.image_read(false, t0);

        // Each fruitless look doubles the gap that follows it: 1.4 s, 2.8 s,
        // then the 5 s ceiling (5.6 s clamped) for as long as nothing turns up.
        let mut at = t0;
        for expected in [
            MIN_IMAGE_POLL * 2,
            MIN_IMAGE_POLL * 4,
            MAX_IMAGE_POLL,
            MAX_IMAGE_POLL,
        ] {
            // A shade before the interval elapses, nothing is due.
            assert!(
                !w.tick_with(None, at + expected - Duration::from_millis(1))
                    .image,
                "polled early with a {expected:?} interval"
            );
            at += expected;
            assert!(w.tick_with(None, at).image, "missed a {expected:?} poll");
            w.image_read(false, at);
        }
    }

    #[test]
    fn finding_an_image_puts_the_gap_back_to_the_floor() {
        // Someone copying a run of images is the case that must not be slow:
        // the first one pays the backoff, and after that the watcher is back
        // at its shortest interval.
        let t0 = Instant::now();
        let mut w = ClipboardWatcher::new(t0);
        w.image_read(false, t0);
        w.image_read(false, t0);
        w.image_read(false, t0);
        assert!(!w.tick_with(None, t0 + MIN_IMAGE_POLL).image);
        w.image_read(true, t0);
        assert!(w.tick_with(None, t0 + MIN_IMAGE_POLL).image);
    }

    #[test]
    fn coming_back_to_the_window_looks_straight_away() {
        // The realistic sequence: copy an image in another application, then
        // click back into the session and paste. Without this the paste waits
        // out however much of a five-second backoff is left.
        let t0 = Instant::now();
        let mut w = ClipboardWatcher::new(t0);
        for _ in 0..8 {
            w.image_read(false, t0);
        }
        assert!(!w.tick_with(None, t0).image);
        w.wake(t0);
        assert!(w.tick_with(None, t0).image);
    }

    #[test]
    fn the_backoff_never_runs_away() {
        let t0 = Instant::now();
        let mut b = Backoff::new(t0);
        for _ in 0..64 {
            b.looked(false, t0);
        }
        assert_eq!(b.interval, MAX_IMAGE_POLL);
        assert!(b.due(t0 + MAX_IMAGE_POLL));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn the_platform_counter_answers_and_does_not_move_on_its_own() {
        // A counter that changed every time it was read would make the watcher
        // useless in exactly the way that is hardest to notice: everything
        // would still work, and every tick would pay the full read.
        assert!(HAS_COUNTER);
        match change_counter() {
            Some(first) => assert_eq!(change_counter(), Some(first)),
            // A Windows station without clipboard access is the documented
            // degradation, not a failure -- the watcher falls back to the
            // backoff. macOS has no such case and must answer.
            None => assert!(cfg!(windows), "the pasteboard did not answer"),
        }
    }

    // A compile-time assertion rather than a runtime one: `HAS_COUNTER` is a
    // `const` per platform, so `assert!` on it is a lint (and would be checking
    // the compiler rather than the code). The behaviour it implies is still
    // asserted at run time below.
    #[cfg(not(any(windows, target_os = "macos")))]
    const _: () = assert!(
        !HAS_COUNTER,
        "this platform has no clipboard change counter"
    );

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn there_is_no_counter_to_trust_here() {
        assert_eq!(change_counter(), None);
    }
}
