//! Resize ownership and debounce, independent of native window events.

use std::time::Instant;
use winit::dpi::PhysicalSize;

use super::RESIZE_DEBOUNCE;

type Size = (u32, u32);

#[derive(Default)]
pub(super) struct ResizeSync {
    pending: Option<(Instant, Size)>,
    correction: Option<(Size, Size)>,
}

impl ResizeSync {
    pub fn viewport_changed(&mut self, target: Size, now: Instant) {
        self.correction = None;
        self.pending = Some((now, target));
    }

    pub fn remote_changed(&mut self, target: Size, remote: Size, now: Instant) {
        if target == remote {
            self.correction = None;
            self.pending = None;
        } else if self.correction != Some((target, remote)) {
            // An identical reply can be a server's configured size limit.
            // Correct it once; do not turn it into an endless request/reply
            // loop. A successful resize or a new viewport resets this guard.
            self.correction = Some((target, remote));
            // A stream of desktop resize events must not postpone a request
            // already queued for the current viewport indefinitely.
            if !self.pending.is_some_and(|(_, size)| size == target) {
                self.pending = Some((now, target));
            }
        }
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn take_due(&mut self, now: Instant, remote: Size) -> Option<Size> {
        let (since, target) = self.pending?;
        if now.saturating_duration_since(since) < RESIZE_DEBOUNCE {
            return None;
        }
        self.pending = None;
        let target = (target.0.min(u16::MAX as u32), target.1.min(u16::MAX as u32));
        (target != remote && target.0 >= 64 && target.1 >= 64).then_some(target)
    }
}

pub(super) fn initial_window_size(
    remote: Size,
    scale: u32,
    monitor: Option<PhysicalSize<u32>>,
    automatic: bool,
) -> PhysicalSize<u32> {
    let requested = PhysicalSize::new(
        remote.0.saturating_mul(scale.max(1)),
        remote.1.saturating_mul(scale.max(1)),
    );
    if let (true, Some(monitor)) = (automatic, monitor) {
        if monitor.width > 0 && monitor.height > 0 {
            return PhysicalSize::new(
                requested.width.min(monitor.width),
                requested.height.min(monitor.height),
            );
        }
    }
    requested
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_desktop_resize_is_corrected_after_an_earlier_request_completed() {
        let now = Instant::now();
        let target = (1728, 1084);
        let mut sync = ResizeSync::default();
        sync.viewport_changed(target, now);
        assert_eq!(
            sync.take_due(now + RESIZE_DEBOUNCE, (1920, 1080)),
            Some(target)
        );
        sync.remote_changed(target, target, now + RESIZE_DEBOUNCE);
        assert!(!sync.is_pending());
        let later = now + RESIZE_DEBOUNCE * 2;
        sync.remote_changed(target, (4096, 2160), later);
        assert_eq!(sync.take_due(later, (4096, 2160)), None);
        assert_eq!(
            sync.take_due(later + RESIZE_DEBOUNCE, (4096, 2160)),
            Some(target)
        );
    }

    #[test]
    fn a_clamped_reply_does_not_cause_an_endless_resize_exchange() {
        let now = Instant::now();
        let target = (2560, 1440);
        let limit = (1600, 1200);
        let mut sync = ResizeSync::default();
        sync.remote_changed(target, limit, now);
        assert_eq!(sync.take_due(now + RESIZE_DEBOUNCE, limit), Some(target));
        for n in 2..10 {
            let later = now + RESIZE_DEBOUNCE * n;
            sync.remote_changed(target, limit, later);
            assert!(!sync.is_pending());
            assert_eq!(sync.take_due(later + RESIZE_DEBOUNCE, limit), None);
        }
        // A user resize or reconnect gives the same target a fresh attempt.
        sync.viewport_changed(target, now);
        assert_eq!(sync.take_due(now + RESIZE_DEBOUNCE, limit), Some(target));
    }

    #[test]
    fn a_successful_resize_allows_a_later_desktop_change_to_be_corrected_again() {
        let now = Instant::now();
        let target = (1000, 700);
        let other = (1600, 1200);
        let mut sync = ResizeSync::default();
        for n in 0..3 {
            let later = now + RESIZE_DEBOUNCE * (n * 2);
            sync.remote_changed(target, other, later);
            assert_eq!(sync.take_due(later + RESIZE_DEBOUNCE, other), Some(target));
            sync.remote_changed(target, target, later + RESIZE_DEBOUNCE);
        }
    }

    #[test]
    fn server_events_do_not_postpone_the_latest_viewport_request() {
        let now = Instant::now();
        let target = (1200, 800);
        let mut sync = ResizeSync::default();
        sync.viewport_changed((1000, 700), now);
        let later = now + RESIZE_DEBOUNCE;
        sync.viewport_changed(target, later);
        sync.remote_changed(target, (4096, 2160), later + RESIZE_DEBOUNCE / 2);
        assert_eq!(
            sync.take_due(later + RESIZE_DEBOUNCE, (4096, 2160)),
            Some(target)
        );
    }

    #[test]
    fn minimized_or_already_matched_viewports_do_not_send_requests() {
        let now = Instant::now();
        let mut sync = ResizeSync::default();
        for target in [(0, 0), (32, 700), (800, 600)] {
            sync.viewport_changed(target, now);
            assert_eq!(sync.take_due(now + RESIZE_DEBOUNCE, (800, 600)), None);
            assert!(!sync.is_pending());
        }
    }

    #[test]
    fn automatic_initial_windows_fit_the_monitor_at_the_chosen_scale() {
        assert_eq!(
            initial_window_size((4096, 2160), 2, Some(PhysicalSize::new(3456, 2234)), true),
            PhysicalSize::new(3456, 2234)
        );
        assert_eq!(
            initial_window_size((800, 600), 1, Some(PhysicalSize::new(1920, 1080)), true),
            PhysicalSize::new(800, 600)
        );
        assert_eq!(
            initial_window_size((4096, 2160), 2, Some(PhysicalSize::new(3456, 2234)), false),
            PhysicalSize::new(8192, 4320)
        );
        assert_eq!(
            initial_window_size((800, 600), 2, None, true),
            PhysicalSize::new(1600, 1200)
        );
    }
}
