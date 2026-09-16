//! Screen capture implementation for macOS.

mod capture;
mod delegate;

pub use capture::*;

use dispatch2::DispatchQueue;
use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::NSView;

/// The `CGWindowID` of the window an `NSView` belongs to, for [`VideoConfig::hide`].
///
/// `raw-window-handle` hands out an `NSView`, ScreenCaptureKit filters on window ids, and
/// only the caller knows the view is still alive -- hence this rather than a pointer in the
/// config. `None` if the view is not in a window.
///
/// [`VideoConfig::hide`]: crate::config::VideoConfig::hide
///
/// # Safety
///
/// `ns_view` must be a pointer to a live `NSView` that stays alive for this call.
pub unsafe fn window_id_from_ns_view(ns_view: isize) -> Option<u32> {
    // `NSView`/`NSWindow` are main-thread-only, so a call from anywhere else hops.
    let read = move || {
        let view = unsafe { Retained::retain(ns_view as *mut NSView) }?;
        // `windowNumber` is the `CGWindowID` ScreenCaptureKit filters on.
        u32::try_from(view.window()?.windowNumber()).ok()
    };
    match MainThreadMarker::new() {
        Some(_) => read(),
        None => {
            let mut id = None;
            DispatchQueue::main().exec_sync(|| id = read());
            id
        }
    }
}
