//! Nudging a compositing window manager into presenting a frame.
//!
//! A root resize at the wrong moment can leave the desktop black. The case
//! that was actually seen: a client connects while GNOME is still starting,
//! its fullscreen viewport resizes the root once, mutter answers by applying
//! the output's preferred mode, and the client resizes it back -- three
//! `RRScreenChangeNotify`s in three hundred milliseconds, all while the shell
//! is bringing its stage up. Afterwards every process in the session is
//! healthy and answering D-Bus, the stage has a full redraw pending for the
//! new size, and the frame clock that would paint it never ticks. Nothing
//! else in the session draws, so nothing kicks it: the user sees the X
//! server's framebuffer, which is black, for as long as they care to wait.
//! Mapping any window -- a `glxgears` from a shell -- made the desktop appear
//! at once, which is the whole observation this module rests on.
//!
//! So that is what this does, deliberately. [`Repainter`] owns a one-pixel
//! override-redirect window that is never shown for longer than a housekeeping
//! tick. Mapping it gives the compositor a new window to account for, which
//! queues a stage redraw, which runs the frame clock, which paints whatever
//! was pending -- the whole screen, if that is what was owed. Unmapping it a
//! moment later does the same again. Without a compositor the map paints one
//! pixel of the root's own background and the unmap exposes it back, and the
//! encoder sees no change to send.
//!
//! Nudges are scheduled rather than fired once, because the compositor that
//! needs one may not be running yet: the resize that stalls it is the one
//! that lands while it is starting, so a single nudge sent at the moment of
//! the resize is the one it is guaranteed to miss. Each root size change arms
//! a short series of them, and so does the arrival of a compositing manager,
//! which announces itself by taking the `_NET_WM_CM_S<screen>` selection.
//! Either can only ever help: a nudge to a compositor that is painting fine is
//! a redraw it would have coalesced anyway, and the session's own frames come
//! from DAMAGE, so nothing is sent to the client that the screen did not
//! change.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::{xfixes, Event};

use super::XDisplay;

/// When, after a trigger, the window is mapped: once at the trigger, and
/// again after the gaps a compositor needs to come up behind it.
///
/// The spread is the point. The first nudge catches a compositor that is
/// running and merely stalled; the later ones catch one that was still
/// starting when the resize landed, which on the host this was seen on took
/// about a second from `_NET_WM_CM_S0` to its first painted frame. A series
/// that ended earlier would be a single nudge with extra steps.
const NUDGE_DELAYS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_millis(1000),
    Duration::from_millis(3000),
];

/// How long the window stays mapped before it is unmapped again. One
/// housekeeping tick is plenty: the map and the unmap are two redraws each,
/// and the compositor only needs one of them to notice.
const NUDGE_HOLD: Duration = Duration::from_millis(100);

/// The schedule of pending nudges, kept apart from the X side so it can be
/// tested without a display.
#[derive(Debug, Default)]
struct Schedule {
    due: VecDeque<Instant>,
}

impl Schedule {
    /// Replace whatever was pending with a fresh series starting at `now`.
    ///
    /// Replace, not append: three resizes in a burst should produce one series
    /// timed from the last of them, not nine nudges timed from each.
    fn arm(&mut self, now: Instant) {
        self.due = NUDGE_DELAYS.iter().map(|d| now + *d).collect();
    }

    /// Take the next nudge if its time has come.
    fn take_due(&mut self, now: Instant) -> bool {
        match self.due.front() {
            Some(&at) if at <= now => {
                self.due.pop_front();
                true
            }
            _ => false,
        }
    }

    fn is_empty(&self) -> bool {
        self.due.is_empty()
    }
}

/// Sends redraw nudges to whatever is compositing the screen.
pub struct Repainter {
    display: Arc<XDisplay>,
    window: xproto::Window,
    /// The `_NET_WM_CM_S<screen>` selection a compositing manager owns.
    cm_selection: xproto::Atom,
    schedule: Schedule,
    /// When the window was last mapped, while it still is.
    mapped_at: Option<Instant>,
}

impl Repainter {
    /// Create the nudge window and ask to hear when a compositor arrives.
    ///
    /// The window is created once and kept, so a nudge is a map and an
    /// unmap rather than a create, map, unmap and destroy: the compositor's
    /// window scan sees the same id each time and nothing accumulates.
    pub fn new(display: Arc<XDisplay>) -> Result<Self> {
        let conn = display.conn();
        let window = display.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            display.root(),
            0,
            0,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new()
                // Override-redirect so that a window manager neither frames
                // it nor gives it focus: it is a redraw trigger, not a window
                // anybody should see or type into.
                .override_redirect(1)
                // Painted in the root's own background, so where there is no
                // compositor the one pixel it covers does not change colour,
                // and the frame the map damages is one the encoder drops as
                // identical to the last.
                .background_pixmap(u32::from(xproto::BackPixmap::PARENT_RELATIVE)),
        )?
        .check()
        .context("create repaint nudge window")?;
        let cm_selection = display.atom(&format!("_NET_WM_CM_S{}", display.screen_num()))?;
        if display.ext.xfixes {
            xfixes::select_selection_input(
                conn,
                window,
                cm_selection,
                xfixes::SelectionEventMask::SET_SELECTION_OWNER,
            )?
            .check()
            .context("subscribe to compositing manager selection")?;
        }
        display.flush()?;
        Ok(Self {
            display,
            window,
            cm_selection,
            schedule: Schedule::default(),
            mapped_at: None,
        })
    }

    /// The root changed size. Arm a series of nudges timed from now.
    pub fn screen_changed(&mut self) {
        self.schedule.arm(Instant::now());
    }

    /// Notice a compositing manager taking the screen.
    ///
    /// Returns whether the event was that. The caller logs it, since it is
    /// the one event here worth a line in the session log: it dates when the
    /// desktop became able to show anything at all.
    pub fn handle_event(&mut self, event: &Event) -> bool {
        match event {
            Event::XfixesSelectionNotify(e)
                if e.selection == self.cm_selection && e.owner != x11rb::NONE =>
            {
                self.schedule.arm(Instant::now());
                true
            }
            _ => false,
        }
    }

    /// Whether anything is pending: nudges still to send, or a window still
    /// to unmap.
    pub fn is_idle(&self) -> bool {
        self.schedule.is_empty() && self.mapped_at.is_none()
    }

    /// Send whatever is due. Called on the housekeeping tick and right after
    /// a trigger, so the first nudge of a series goes out at once.
    pub fn tick(&mut self) -> Result<()> {
        let now = Instant::now();
        if let Some(at) = self.mapped_at {
            if now.duration_since(at) < NUDGE_HOLD {
                return Ok(());
            }
            self.display.conn().unmap_window(self.window)?;
            self.display.flush()?;
            self.mapped_at = None;
        }
        if self.schedule.take_due(now) {
            log::debug!("nudging the compositor to repaint");
            self.display.conn().map_window(self.window)?;
            self.display.flush()?;
            self.mapped_at = Some(now);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arming_schedules_the_whole_series_from_now() {
        let mut s = Schedule::default();
        let t0 = Instant::now();
        s.arm(t0);
        assert!(s.take_due(t0), "the first nudge is due at once");
        assert!(!s.take_due(t0), "the second is not");
        assert!(!s.take_due(t0 + NUDGE_DELAYS[1] - Duration::from_millis(1)));
        assert!(s.take_due(t0 + NUDGE_DELAYS[1]));
        assert!(s.take_due(t0 + NUDGE_DELAYS[2]));
        assert!(s.is_empty());
        assert!(!s.take_due(t0 + Duration::from_secs(60)));
    }

    /// A burst of resizes is one series timed from the last, not a series
    /// per resize.
    #[test]
    fn rearming_replaces_rather_than_appends() {
        let mut s = Schedule::default();
        let t0 = Instant::now();
        s.arm(t0);
        assert!(s.take_due(t0));
        let t1 = t0 + Duration::from_millis(300);
        s.arm(t1);
        assert_eq!(s.due.len(), NUDGE_DELAYS.len());
        assert!(s.take_due(t1));
        // The second nudge of the *first* series would have been due here;
        // it was replaced by one timed from the re-arm.
        assert!(!s.take_due(t0 + NUDGE_DELAYS[1]));
        assert!(s.take_due(t1 + NUDGE_DELAYS[1]));
    }

    /// Only ever one nudge per tick, however late the tick is: two maps in
    /// one flush would be one map to the compositor.
    #[test]
    fn a_late_tick_takes_one_nudge_at_a_time() {
        let mut s = Schedule::default();
        let t0 = Instant::now();
        s.arm(t0);
        let late = t0 + Duration::from_secs(10);
        assert!(s.take_due(late));
        assert_eq!(s.due.len(), NUDGE_DELAYS.len() - 1);
        assert!(s.take_due(late));
        assert!(s.take_due(late));
        assert!(!s.take_due(late));
    }

    #[test]
    fn the_series_outlasts_a_compositor_startup() {
        let span = NUDGE_DELAYS[NUDGE_DELAYS.len() - 1];
        assert!(span >= Duration::from_secs(2), "{span:?}");
        assert!(NUDGE_HOLD < Duration::from_millis(250), "{NUDGE_HOLD:?}");
    }
}
