//! OS drag loops do not necessarily emit winit CursorMoved events.
//! Read the current position when the OS delivers a dropped file.
use winit::{dpi::PhysicalPosition, window::Window};

pub(crate) fn position(window: &Window) -> Option<PhysicalPosition<f64>> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::POINT, Graphics::Gdi::ScreenToClient, UI::WindowsAndMessaging::GetCursorPos,
        };
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let RawWindowHandle::Win32(handle) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        let mut point = POINT { x: 0, y: 0 };
        // The handle belongs to this live winit window; both calls write POINT.
        unsafe {
            if GetCursorPos(&mut point) == 0
                || ScreenToClient(handle.hwnd.get() as _, &mut point) == 0
            {
                return None;
            }
        }
        Some(PhysicalPosition::new(
            f64::from(point.x),
            f64::from(point.y),
        ))
    }
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::NSView;
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        // Called on the main event thread while winit owns the NSView.
        let view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
        let native = view.window()?;
        let point = view.convertPoint_fromView(native.mouseLocationOutsideOfEventStream(), None);
        let y = if view.isFlipped() {
            point.y
        } else {
            view.bounds().size.height - point.y
        };
        let scale = window.scale_factor();
        Some(PhysicalPosition::new(point.x * scale, y * scale))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use x11rb::protocol::xproto::ConnectionExt as _;
        let id = match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Xlib(h) => u32::try_from(h.window).ok()?,
            RawWindowHandle::Xcb(h) => h.window.get(),
            _ => return None,
        };
        let (conn, _) = x11rb::connect(None).ok()?;
        let point = conn.query_pointer(id).ok()?.reply().ok()?;
        point.same_screen.then_some(PhysicalPosition::new(
            f64::from(point.win_x),
            f64::from(point.win_y),
        ))
    }
}
