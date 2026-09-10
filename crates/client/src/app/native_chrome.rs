//! Fullscreen title/menu bars live outside winit's content-view tracking area.
//! Sample only our own active window during the existing housekeeping tick;
//! no global event tap, accessibility grant, or synthetic remote input is used.

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Chrome {
    pub hovered: bool,
    pub top_inset: u32,
}

/// AppKit screen coordinates: points, bottom-left origin, possibly negative
/// on another display. Do not mix them with the physical, top-left framebuffer.
#[derive(Clone, Copy)]
struct Bounds {
    left: f64,
    bottom: f64,
    right: f64,
    top: f64,
}

fn geometry(
    pointer: (f64, f64),
    screen: Bounds,
    content: Bounds,
    titlebar: Option<Bounds>,
    scale: f64,
) -> Chrome {
    let overlap = titlebar
        .filter(|bar| {
            bar.bottom < content.top
                && bar.top >= content.top
                && bar.right > content.left
                && bar.left < content.right
        })
        .map_or(0.0, |bar| content.top - bar.bottom.max(content.bottom));
    // A notched display leaves a menu-bar strip above the content even when
    // the title bar is hidden. On a display without that strip, the final
    // screen-edge point starts the native reveal before it has animated in.
    let top_zone = (content.top - overlap).min(screen.top - 1.0);
    Chrome {
        hovered: pointer.0 >= content.left.max(screen.left)
            && pointer.0 < content.right.min(screen.right)
            && pointer.1 >= top_zone.max(screen.bottom)
            && pointer.1 <= screen.top,
        top_inset: (overlap * scale).ceil() as u32,
    }
}

#[cfg(target_os = "macos")]
pub(super) fn poll(window: &winit::window::Window) -> Option<Chrome> {
    use objc2_app_kit::{NSView, NSWindowButton, NSWindowStyleMask};
    use objc2_foundation::NSRect;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    // Called on the main event thread while this live winit window owns NSView.
    let view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
    let native = view.window()?;
    if !native.isKeyWindow() || !native.styleMask().contains(NSWindowStyleMask::FullScreen) {
        return None;
    }
    let screen = native.screen()?;
    let bounds = |rect: NSRect| Bounds {
        left: rect.origin.x,
        bottom: rect.origin.y,
        right: rect.origin.x + rect.size.width,
        top: rect.origin.y + rect.size.height,
    };
    let content = native.convertRectToScreen(view.convertRect_toView(view.bounds(), None));
    let titlebar = native
        .standardWindowButton(NSWindowButton::CloseButton)
        .filter(|button| !button.isHiddenOrHasHiddenAncestor())
        .and_then(|button| {
            // The retained button belongs to this live window, and AppKit's
            // view hierarchy is read only on its owning main thread.
            let row = unsafe { button.superview() }?;
            let host = row.window()?;
            // AppKit's fullscreen title bar is hosted separately. Its window
            // stays visible, with a fixed frame, even when the row slides out
            // of sight. Convert the *row*, not that host frame, to find the
            // actual overlap; contentLayoutRect also stays unchanged here.
            if !host.isVisible() || std::ptr::eq(&*host, &*native) {
                return None;
            }
            Some(bounds(host.convertRectToScreen(
                row.convertRect_toView(row.bounds(), None),
            )))
        });
    let pointer = native.convertPointToScreen(native.mouseLocationOutsideOfEventStream());
    Some(geometry(
        (pointer.x, pointer.y),
        bounds(screen.frame()),
        bounds(content),
        titlebar,
        window.scale_factor(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Bounds = Bounds {
        left: 0.0,
        bottom: 0.0,
        right: 1728.0,
        top: 1117.0,
    };
    const CONTENT: Bounds = Bounds {
        top: 1084.0,
        ..SCREEN
    };
    const HIDDEN: Bounds = Bounds {
        bottom: 1084.0,
        ..SCREEN
    };
    const SHOWN: Bounds = Bounds {
        bottom: 1052.0,
        top: 1084.0,
        ..SCREEN
    };

    #[test]
    fn screen_edge_and_native_titlebar_reveal_without_content_mouse_events() {
        assert_eq!(
            geometry((664.0, 1116.0), SCREEN, CONTENT, Some(HIDDEN), 2.0),
            Chrome {
                hovered: true,
                top_inset: 0
            }
        );
        for y in [1117.0, 1116.0, 1090.0, 1072.0, 1052.0] {
            assert_eq!(
                geometry((664.0, y), SCREEN, CONTENT, Some(SHOWN), 2.0),
                Chrome {
                    hovered: true,
                    top_inset: 64
                }
            );
        }
        assert!(!geometry((664.0, 1051.0), SCREEN, CONTENT, Some(SHOWN), 2.0).hovered);
        // Ordinary remote view movement keeps its existing dwell behavior.
        assert!(!geometry((664.0, 1080.0), SCREEN, CONTENT, Some(HIDDEN), 2.0).hovered);
    }

    #[test]
    fn non_notched_screen_uses_only_the_outermost_point_before_reveal() {
        assert!(geometry((500.0, 1116.0), SCREEN, SCREEN, None, 1.0).hovered);
        assert!(!geometry((500.0, 1115.0), SCREEN, SCREEN, None, 1.0).hovered);
    }

    #[test]
    fn other_displays_do_not_raise_this_windows_bar() {
        for point in [
            (-1.0, 1116.0),
            (1728.0, 1116.0),
            (500.0, 1118.0),
            (500.0, -1.0),
        ] {
            assert!(!geometry(point, SCREEN, CONTENT, Some(SHOWN), 2.0).hovered);
        }
        let shift = |b: Bounds| Bounds {
            left: b.left - 1728.0,
            right: b.right - 1728.0,
            bottom: b.bottom - 400.0,
            top: b.top - 400.0,
        };
        assert_eq!(
            geometry(
                (-1000.0, 672.0),
                shift(SCREEN),
                shift(CONTENT),
                Some(shift(SHOWN)),
                1.0
            ),
            Chrome {
                hovered: true,
                top_inset: 32
            }
        );
    }

    #[test]
    fn animated_overlap_rounds_up_to_keep_every_control_pixel_visible() {
        let row = Bounds {
            bottom: 1083.75,
            ..SHOWN
        };
        assert_eq!(
            geometry((500.0, 1116.0), SCREEN, CONTENT, Some(row), 2.0).top_inset,
            1
        );
    }
}
