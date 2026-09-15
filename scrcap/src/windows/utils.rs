use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE},
};

use crate::error::Result;

/// Hide the window from screen capture.
pub(crate) fn hide_window_from_capture(hwnd: HWND) -> Result<()> {
    unsafe {
        SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)?;
    }
    Ok(())
}
