//! Capture configuration

/// Configuration for the capture process.
#[derive(Debug)]
pub struct CaptureConfig {
    /// The capabilty of channel for the frames.
    pub channel_capacity: usize,
    /// The config for video
    pub video: VideoConfig,
    /// The config for audio
    pub audio: Option<AudioConfig>,
}

/// Configuration for the capture video.
#[derive(Debug)]
pub struct VideoConfig {
    /// The window id to hide from capture:
    /// - Windows: HWND
    /// - macOS: NSView ptr
    /// - linux: unsupported
    pub hide: Vec<isize>,
    /// The target to capture.
    ///
    /// Linux: omit, only pick
    /// macOS: only primary monitor
    /// Window: except WindowName and Pick
    pub target: Target,
}

/// Configuration for the capture audio.
#[derive(Debug)]
pub struct AudioConfig {}

/// Capture target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Capture the primary monitor.
    Primary,
    /// Capture a specific monitor by its index.
    Monitor(isize),
    /// Capture a specific window by its window id:
    /// - Windows: HWND
    Window(isize),
    /// Capture a specific window first found by matching the given regex.
    WindowName(String),
    /// Use picker api to select the target.
    Pick,
}
