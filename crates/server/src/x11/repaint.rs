//! Making a compositing window manager present its whole stage again.
//!
//! A root resize at the wrong moment can leave the desktop black. The case
//! that was seen, twice, on GNOME 46: a client connects while the shell is
//! still starting, its fullscreen viewport resizes the root once, mutter
//! answers by applying the output's preferred mode, and the client resizes
//! it back -- a handful of `RRScreenChangeNotify`s in under a second, all
//! while the shell is bringing its stage up. Afterwards every process in the
//! session is healthy, the shell's main loop sits idle in `poll`, and the
//! screen is black for as long as anyone cares to wait.
//!
//! What a probe of such a session showed is more specific than a stalled
//! frame clock. Mapping a window made mutter paint that window, frame and
//! all -- and nothing else: the wallpaper and the panel around it stayed
//! black. Mutter 46 as an X11 compositor presents a frame by painting only
//! the stage's accumulated redraw clip and copying only that rectangle onto
//! its stage window (`glXCopySubBuffer`, `meta-stage-impl.c`), and without
//! `GLX_EXT_buffer_age`, which software GL does not offer, it has no notion
//! of whether the window still holds a valid picture. The resize burst
//! emptied the window, mutter's last `ConfigureNotify` matched the size it
//! had already cached, so no full redraw was queued, and from then on every
//! frame was a rectangle of fresh pixels pasted onto black. A one-pixel
//! window mapped as a hint, which an earlier version of this module relied
//! on, only queues that pixel.
//!
//! What mutter does handle is an `Expose` on its stage window
//! (`meta-stage-x11.c`): it queues a redraw with the exposed rectangle as
//! the clip, and a clip equal to the whole view is a full redraw, which
//! takes the unclipped path and ends in a full buffer swap. So that is what
//! this sends. [`Repainter`] finds the compositor's stage window -- the
//! children of the Composite overlay window, which is where every
//! compositing manager that uses the overlay keeps it -- and asks the X
//! server for an `Expose` over all of it with a `ClearArea` whose
//! `exposures` flag is set. Nothing is painted by that request on a window
//! with no background, and a compositor that is drawing fine answers it with
//! one redraw it would have coalesced anyway.
//!
//! Nudges are scheduled rather than fired once, because the compositor that
//! needs one may not be running yet, or may not have processed the resize
//! yet: an `Expose` that reaches the stage before its layout has caught up
//! is a partial clip again. Each root size change arms a short series, and
//! so does the arrival of a compositing manager, which announces itself by
//! taking the `_NET_WM_CM_S<screen>` selection. Behind that, the session
//! keeps watching for a minute after the compositor arrives: a screen that
//! is still entirely black then is nudged again, because that is the one
//! state in which a desktop with a compositor has nothing to show. Without a
//! compositor there is no stage to expose and nothing is sent at all.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use x11rb::protocol::composite::ConnectionExt as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::{xfixes, Event};

use super::XDisplay;

/// When, after a trigger, the stage is exposed: once at the trigger, and
/// again after the gaps a compositor needs to come up, or to finish
/// applying the resize, behind it.
///
/// The spread is the point. The first nudge catches a compositor that is
/// running and merely holding a stale window; the later ones catch one that
/// was still starting when the resize landed, which on the host this was
/// seen on took about a second from `_NET_WM_CM_S0` to its first painted
/// frame, and longer on a slow one. A series that ended earlier would be a
/// single nudge with extra steps.
const NUDGE_DELAYS: [Duration; 4] = [
    Duration::ZERO,
    Duration::from_millis(1000),
    Duration::from_millis(3000),
    Duration::from_millis(6000),
];

/// How long after a compositor arrives an all-black screen is still taken
/// as a compositor that owes a frame, rather than as a desktop somebody
/// blanked on purpose.
const BLACK_WATCH: Duration = Duration::from_secs(60);

/// How often the black-screen check runs, and the least time between the
/// extra nudges it sends. A full stage repaint costs the compositor a few
/// milliseconds; the scan of the framebuffer costs the session about one.
const BLACK_CHECK_EVERY: Duration = Duration::from_secs(1);
const BLACK_NUDGE_EVERY: Duration = Duration::from_secs(2);

/// The schedule of pending nudges, kept apart from the X side so it can be
/// tested without a display.
#[derive(Default)]
struct Schedule {
    due: VecDeque<Instant>,
}

impl Schedule {
    /// Replace whatever is pending with a fresh series timed from `now`.
    ///
    /// A burst of resizes is one event to the compositor, so it is one series
    /// timed from the last of them, not nine nudges timed from each.
    fn arm(&mut self, now: Instant) {
        self.due.clear();
        self.due.extend(NUDGE_DELAYS.iter().map(|d| now + *d));
    }

    /// Add a single nudge due now, keeping whatever else is pending.
    fn arm_once(&mut self, now: Instant) {
        self.due.push_front(now);
    }

    /// Take the next nudge if its time has come.
    fn take_due(&mut self, now: Instant) -> bool {
        match self.due.front() {
            Some(at) if *at <= now => {
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

/// Exposes the stage of whatever is compositing the screen.
pub struct Repainter {
    display: Arc<XDisplay>,
    /// The `_NET_WM_CM_S<screen>` selection a compositing manager owns.
    cm_selection: xproto::Atom,
    schedule: Schedule,
    /// When a compositing manager last took the screen, while the
    /// black-screen watch for it is still running.
    compositor_since: Option<Instant>,
    last_black_check: Option<Instant>,
    last_nudge: Option<Instant>,
}

impl Repainter {
    /// Ask to hear when a compositor arrives.
    ///
    /// The window created here is never mapped; it exists because XFIXES
    /// delivers selection events to a window.
    pub fn new(display: Arc<XDisplay>) -> Result<Self> {
        let conn = display.conn();
        let cm_selection = display.atom(&format!("_NET_WM_CM_S{}", display.screen_num()))?;
        if display.ext.xfixes {
            let window = display.generate_id()?;
            conn.create_window(
                0,
                window,
                display.root(),
                0,
                0,
                1,
                1,
                0,
                xproto::WindowClass::INPUT_ONLY,
                x11rb::COPY_FROM_PARENT,
                &xproto::CreateWindowAux::new(),
            )?
            .check()
            .context("create selection listener window")?;
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
            cm_selection,
            schedule: Schedule::default(),
            compositor_since: None,
            last_black_check: None,
            last_nudge: None,
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
                let now = Instant::now();
                self.schedule.arm(now);
                self.compositor_since = Some(now);
                true
            }
            _ => false,
        }
    }

    /// Whether the caller should look at its framebuffer for
    /// [`Repainter::screen_is_black`]: a compositor arrived recently enough
    /// that an all-black screen still means a frame it owes, and the last
    /// look was long enough ago.
    pub fn wants_black_check(&mut self) -> bool {
        let now = Instant::now();
        let Some(since) = self.compositor_since else {
            return false;
        };
        if now.duration_since(since) > BLACK_WATCH {
            self.compositor_since = None;
            return false;
        }
        if self
            .last_black_check
            .is_some_and(|at| now.duration_since(at) < BLACK_CHECK_EVERY)
        {
            return false;
        }
        self.last_black_check = Some(now);
        true
    }

    /// The screen is entirely black although a compositor has had it for a
    /// while: send one more nudge, if the last was not a moment ago.
    pub fn screen_is_black(&mut self) {
        let now = Instant::now();
        if self
            .last_nudge
            .is_some_and(|at| now.duration_since(at) < BLACK_NUDGE_EVERY)
        {
            return;
        }
        log::info!("the screen is still black under a compositor; asking it to repaint");
        self.schedule.arm_once(now);
    }

    /// Whether anything is pending.
    pub fn is_idle(&self) -> bool {
        self.schedule.is_empty()
    }

    /// Send whatever is due. Called on the housekeeping tick and right after
    /// a trigger, so the first nudge of a series goes out at once.
    pub fn tick(&mut self) -> Result<()> {
        let now = Instant::now();
        if self.schedule.take_due(now) {
            self.last_nudge = Some(now);
            self.expose_stage()?;
        }
        Ok(())
    }

    /// Expose the compositor's stage window(s), if there is a compositor.
    ///
    /// The overlay window is taken and released within one flush. Taking it
    /// is what makes the X server map it, so for the compositors that use
    /// it -- mutter, KWin, xfwm4, picom -- this changes nothing they can
    /// see; a compositor that composites without the overlay would see it
    /// come and go, which exposes the windows beneath it once and is the
    /// price of not knowing in advance.
    fn expose_stage(&mut self) -> Result<()> {
        let conn = self.display.conn();
        let owner = conn
            .get_selection_owner(self.cm_selection)?
            .reply()
            .context("compositing manager selection owner")?
            .owner;
        if owner == x11rb::NONE {
            log::debug!("no compositing manager to nudge");
            return Ok(());
        }
        if !self.display.ext.composite {
            log::debug!("no Composite extension; cannot find the compositor's stage");
            return Ok(());
        }
        let root = self.display.root();
        let overlay = conn
            .composite_get_overlay_window(root)?
            .reply()
            .context("composite overlay window")?
            .overlay_win;
        let tree = conn.query_tree(overlay)?.reply();
        let release = conn.composite_release_overlay_window(root)?;
        let children = tree.context("overlay window tree")?.children;
        let targets: &[xproto::Window] = if children.is_empty() {
            std::slice::from_ref(&overlay)
        } else {
            &children
        };
        for &w in targets {
            conn.clear_area(true, w, 0, 0, 0, 0)?;
        }
        release.ignore_error();
        self.display.flush()?;
        log::debug!(
            "exposed {} compositor stage window(s) under overlay {overlay:#x}",
            targets.len()
        );
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
        assert!(s.take_due(t0 + NUDGE_DELAYS[3]));
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

    /// Only ever one nudge per tick, however late the tick is: two exposes
    /// in one flush would be one redraw to the compositor.
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
        assert!(s.take_due(late));
        assert!(!s.take_due(late));
    }

    /// The black-screen watch adds to a running series without discarding
    /// it, and goes first.
    #[test]
    fn a_single_nudge_joins_the_front_of_the_queue() {
        let mut s = Schedule::default();
        let t0 = Instant::now();
        s.arm(t0);
        assert!(s.take_due(t0));
        let t1 = t0 + Duration::from_millis(500);
        s.arm_once(t1);
        assert_eq!(s.due.len(), NUDGE_DELAYS.len());
        assert!(s.take_due(t1), "the extra nudge is due at once");
        assert!(!s.take_due(t1), "the series itself keeps its timing");
        assert!(s.take_due(t0 + NUDGE_DELAYS[1]));
    }

    #[test]
    fn the_series_outlasts_a_compositor_startup() {
        let span = NUDGE_DELAYS[NUDGE_DELAYS.len() - 1];
        assert!(span >= Duration::from_secs(5), "{span:?}");
        assert!(BLACK_WATCH > span);
        assert!(BLACK_NUDGE_EVERY >= BLACK_CHECK_EVERY);
    }
}
