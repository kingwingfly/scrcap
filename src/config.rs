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
    /// - macOS: NSView ptr
    /// - linux: unsupported
    pub hide: Vec<isize>,
    /// The target to capture.
    ///
    /// Linux: Primary/Monitor pick a monitor, Pick anything; Window/WindowName unsupported
    /// macOS: only primary monitor
    /// Windows: except WindowName and Pick
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
    /// - Windows: HWND
    Window(isize),
    /// Capture a specific window first found by matching the given regex.
    WindowName(String),
    /// Use picker api to select the target.
    Pick,
}
