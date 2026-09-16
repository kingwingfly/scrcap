//! Capture configuration

/// Configuration for the capture process.
#[derive(Debug)]
pub struct CaptureConfig {
    /// The config for video
    pub video: VideoConfig,
    /// The config for audio
    pub audio: Option<AudioConfig>,
}

/// Configuration for the capture video.
#[derive(Debug)]
pub struct VideoConfig {
    /// Capacity of the channel carrying video frames.
    ///
    /// Video and audio get a channel each: a frame is orders of magnitude larger than an
    /// audio buffer, and dropping one is cheap where dropping audio is not.
    pub channel_capacity: usize,
    /// The window id to hide from capture:
    /// - Windows: HWND
    /// - macOS: NSView ptr. Hiding is done on the main thread, so that thread must be
    ///   running its loop if `create` is called from anywhere else.
    /// - Linux: unsupported, a non-empty list is [`CaptureError::Unsupported`]. Neither
    ///   Wayland nor X11 lets a client opt a window out of a screencast.
    ///
    /// [`CaptureError::Unsupported`]: crate::error::CaptureError::Unsupported
    pub hide: Vec<isize>,
    /// The target to capture.
    ///
    /// Everything is supported everywhere except where noted:
    /// - Linux: the XDG portal picker always makes the final choice, so `Primary` and
    ///   `Monitor` only restrict it to monitors (the index is *not* honoured), and
    ///   `Window`/`WindowName` are [`CaptureError::Unsupported`].
    /// - `Pick` blocks until the user chooses, so on Windows and macOS it must not be
    ///   called from the thread driving the UI.
    ///
    /// [`CaptureError::Unsupported`]: crate::error::CaptureError::Unsupported
    pub target: Target,
    /// Cap on delivered frames per second, `None` to leave the source uncapped.
    ///
    /// A cap, not a target: it cannot raise a rate the machine cannot sustain. The
    /// platform's own knob is set where there is one, and frames that still arrive too
    /// fast are dropped before being copied out of the capture buffer.
    pub fps: Option<u32>,
}

/// Configuration for the capture audio.
#[derive(Debug)]
pub struct AudioConfig {
    /// Capacity of the channel carrying audio frames.
    ///
    /// Worth setting far deeper than the video one: audio frames are small, and a gap is an
    /// audible artefact rather than a frame the encoder can stretch over.
    pub channel_capacity: usize,
}

/// Capture target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Capture the primary monitor.
    Primary,
    /// Capture a specific monitor by its index.
    Monitor(isize),
    /// Capture a specific window by its window id:
    /// - Windows: `HWND`
    /// - macOS: `CGWindowID`
    /// - Linux: unsupported, [`CaptureError::Unsupported`]
    ///
    /// [`CaptureError::Unsupported`]: crate::error::CaptureError::Unsupported
    Window(isize),
    /// Capture the first visible window whose title matches the given regex.
    WindowName(String),
    /// Let the user choose with the system picker.
    ///
    /// The inner value is the `HWND` to present the picker from; it has to be a window this
    /// process owns.
    ///
    /// `create` blocks until the user has chosen, and the wait does not pump messages, so it
    /// must not be called from the thread that owns that `HWND` (or from any other STA
    /// thread) — doing so deadlocks.
    #[cfg(target_os = "windows")]
    Pick(isize),
    /// Let the user choose with the system picker.
    ///
    /// macOS needs 14.0 or newer, and `create` blocks until the user has chosen. The picker
    /// answers on the main queue, so `create` must not be called from the main thread —
    /// doing so deadlocks.
    #[cfg(not(target_os = "windows"))]
    Pick,
}
