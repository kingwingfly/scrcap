use parking_lot::Mutex;
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        GetWindowDisplayAffinity, IsWindow, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
        WINDOW_DISPLAY_AFFINITY,
    },
};

use crate::error::Result;

/// A window this process has hidden: how many captures asked for it, and the affinity it had
/// before the first of them did.
struct Hidden {
    hwnd: isize,
    captures: u32,
    previous: WINDOW_DISPLAY_AFFINITY,
}

/// Every window hidden through this crate.
///
/// Windows keeps one affinity per window, so two captures hiding the same one cannot each
/// save and restore their own idea of it: the first hide saves it and the last restore puts
/// it back. Without this, dropping either capture would un-hide a window the other is still
/// recording, and a window named twice in one `hide` would stay hidden for good.
static HIDDEN: Mutex<Vec<Hidden>> = Mutex::new(Vec::new());

/// Hide the window from screen capture until as many [`unhide_window`] calls come back.
pub(crate) fn hide_window_from_capture(hwnd: HWND) -> Result<()> {
    let key = hwnd.0 as isize;
    let mut hidden = HIDDEN.lock();
    if let Some(entry) = hidden.iter_mut().find(|entry| entry.hwnd == key) {
        entry.captures += 1;
        return Ok(());
    }
    // The affinity is only read here, on the first hide: reading it again while the window is
    // already excluded would save the exclusion as the thing to restore.
    unsafe {
        let mut previous = 0u32;
        GetWindowDisplayAffinity(hwnd, &mut previous)?;
        SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)?;
        hidden.push(Hidden {
            hwnd: key,
            captures: 1,
            previous: WINDOW_DISPLAY_AFFINITY(previous),
        });
    }
    Ok(())
}

/// Undo one [`hide_window_from_capture`], restoring the affinity once the last one is undone.
pub(crate) fn unhide_window(hwnd: HWND) {
    let key = hwnd.0 as isize;
    let mut hidden = HIDDEN.lock();
    let Some(index) = hidden.iter().position(|entry| entry.hwnd == key) else {
        return;
    };
    hidden[index].captures -= 1;
    if hidden[index].captures > 0 {
        return;
    }
    let entry = hidden.swap_remove(index);
    unsafe {
        // The window may be gone, and `HWND`s are recycled: restoring then aims at whatever
        // holds the handle now, so at least leave a destroyed window alone.
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = SetWindowDisplayAffinity(hwnd, entry.previous);
        }
    }
}
