use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        GetWindowDisplayAffinity, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
        WINDOW_DISPLAY_AFFINITY,
    },
};

use crate::error::Result;

/// Hide the window from screen capture, returning its previous affinity.
pub(crate) fn hide_window_from_capture(hwnd: HWND) -> Result<WINDOW_DISPLAY_AFFINITY> {
    unsafe {
        let mut previous = 0u32;
        GetWindowDisplayAffinity(hwnd, &mut previous)?;
        SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)?;
        Ok(WINDOW_DISPLAY_AFFINITY(previous))
    }
}

/// Restore the affinity returned by [`hide_window_from_capture`].
pub(crate) fn restore_window_capture_affinity(hwnd: HWND, affinity: WINDOW_DISPLAY_AFFINITY) {
    unsafe {
        let _ = SetWindowDisplayAffinity(hwnd, affinity);
    }
}
