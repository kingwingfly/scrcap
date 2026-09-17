use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        GetWindowDisplayAffinity, GetWindowThreadProcessId, IsWindow, SetWindowDisplayAffinity,
        WDA_EXCLUDEFROMCAPTURE, WINDOW_DISPLAY_AFFINITY,
    },
};

use crate::error::Result;

/// One window a capture has hidden, to be handed back to [`unhide_window`] when it ends.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HideToken {
    hwnd: isize,
    /// Which claim on that handle this is. `HWND`s are recycled, so a hide of the window
    /// that holds the handle *now* gets a new generation and the old token stops matching.
    generation: u64,
}

/// A window this process has hidden: how many captures asked for it, the affinity it had
/// before the first of them did, and what it takes to recognise it later.
struct Hidden {
    hwnd: isize,
    generation: u64,
    captures: u32,
    previous: WINDOW_DISPLAY_AFFINITY,
    /// The window's thread. Half of telling a recycled handle apart; the other half is that
    /// the window we hid is still excluded.
    thread: u32,
}

/// Every window hidden through this crate.
///
/// Windows keeps one affinity per window, so two captures hiding the same one cannot each
/// save and restore their own idea of it: the first hide saves it and the last restore puts
/// it back. Without this, dropping either capture would un-hide a window the other is still
/// recording, and a window named twice in one `hide` would stay hidden for good.
static HIDDEN: Mutex<Vec<Hidden>> = Mutex::new(Vec::new());
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Hide the window from screen capture until the token comes back to [`unhide_window`].
pub(crate) fn hide_window_from_capture(hwnd: HWND) -> Result<HideToken> {
    let key = hwnd.0 as isize;
    let mut hidden = HIDDEN.lock();
    let mut affinity = 0u32;
    // Also proves the window is alive: this fails on a handle that no longer names one.
    unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity)? };
    let thread = unsafe { GetWindowThreadProcessId(hwnd, None) };
    if let Some(index) = hidden.iter().position(|entry| entry.hwnd == key) {
        let entry = &mut hidden[index];
        // Still the window we hid? Then only count this hide: reading the affinity again
        // would save the exclusion itself as the thing to restore. A window that is no
        // longer excluded, or now lives on another thread, is a different one wearing a
        // recycled handle, and the entry describing the old one is finished with.
        if entry.thread == thread && affinity == WDA_EXCLUDEFROMCAPTURE.0 {
            entry.captures += 1;
            return Ok(HideToken {
                hwnd: key,
                generation: entry.generation,
            });
        }
        hidden.swap_remove(index);
    }
    unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)? };
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
    hidden.push(Hidden {
        hwnd: key,
        generation,
        captures: 1,
        previous: WINDOW_DISPLAY_AFFINITY(affinity),
        thread,
    });
    Ok(HideToken {
        hwnd: key,
        generation,
    })
}

/// Undo one [`hide_window_from_capture`], restoring the affinity once the last one is undone.
pub(crate) fn unhide_window(token: HideToken) {
    let mut hidden = HIDDEN.lock();
    let Some(index) = hidden
        .iter()
        .position(|entry| entry.hwnd == token.hwnd && entry.generation == token.generation)
    else {
        // The window this token hid is gone, and its handle belongs to someone else now.
        return;
    };
    hidden[index].captures -= 1;
    if hidden[index].captures > 0 {
        return;
    }
    let entry = hidden.swap_remove(index);
    let hwnd = HWND(entry.hwnd as _);
    unsafe {
        // Undo only what is still ours: the window may be gone, its handle reused, or its
        // affinity changed by something else while the capture ran.
        let mut affinity = 0u32;
        if IsWindow(Some(hwnd)).as_bool()
            && GetWindowDisplayAffinity(hwnd, &mut affinity).is_ok()
            && affinity == WDA_EXCLUDEFROMCAPTURE.0
        {
            let _ = SetWindowDisplayAffinity(hwnd, entry.previous);
        }
    }
}
