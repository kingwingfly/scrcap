//! Screen capture implementation for macOS.

mod capture;
mod delegate;

pub use capture::*;

use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use dispatch2::DispatchQueue;
use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::NSView;

/// The capture's width and height, shared between the capture thread and the descriptor.
///
/// One `u64` rather than a lock: the output delegate writes it from ScreenCaptureKit's serial
/// queue, where it must never block. The packing lives here alone, so the write and the read
/// cannot drift apart, and `store` taking `u32`s is what keeps a 64-bit height out of the
/// width half.
#[derive(Default)]
pub(crate) struct AtomicSize(AtomicU64);

impl AtomicSize {
    pub(crate) fn store(&self, width: u32, height: u32) {
        self.0
            .store((width as u64) << 32 | height as u64, Ordering::Relaxed);
    }

    pub(crate) fn load(&self) -> (u32, u32) {
        let packed = self.0.load(Ordering::Relaxed);
        ((packed >> 32) as u32, packed as u32)
    }
}

impl fmt::Debug for AtomicSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.load().fmt(f)
    }
}

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
