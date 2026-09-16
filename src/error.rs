//! Error types for the screen capture library.

use thiserror::Error;

/// Everything that can go wrong while setting up or running a capture.
///
/// Variants are grouped by what a caller can *do* about them: [`Cancelled`] and
/// [`PermissionDenied`] are user decisions, [`TargetNotFound`] and the `Invalid*` variants
/// are wrong arguments, [`Unsupported`] is a missing capability, and the rest are failures
/// reported by the platform.
///
/// [`Cancelled`]: Self::Cancelled
/// [`PermissionDenied`]: Self::PermissionDenied
/// [`TargetNotFound`]: Self::TargetNotFound
/// [`Unsupported`]: Self::Unsupported
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CaptureError {
    /// The user dismissed the system picker or the permission prompt without choosing.
    ///
    /// Not a malfunction: ask again, or give up quietly.
    #[error("the user cancelled the capture request")]
    Cancelled,

    /// The system refuses to let this process capture the screen.
    ///
    /// macOS: Screen Recording in System Settings > Privacy & Security, which only takes
    /// effect after the app is restarted. Linux: the portal's permission dialog.
    #[error("screen capture permission was denied")]
    PermissionDenied,

    /// Nothing on the system matched the requested [`Target`](crate::config::Target).
    ///
    /// The monitor index is out of range, or no window has that id or title.
    #[error("no capture target matched")]
    TargetNotFound,

    /// [`Target::WindowName`](crate::config::Target::WindowName) is not a valid regex.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[error("the window name pattern is not a valid regex: {0}")]
    BadWindowRegex(#[from] regex::Error),

    /// `channel_capacity` was 0 on a video or audio config.
    ///
    /// Frames are published with `try_send`, which on a rendezvous channel only lands
    /// while a consumer happens to be parked in `recv`, so 0 would drop nearly everything.
    #[error("channel_capacity must be at least 1")]
    ZeroChannelCapacity,

    /// The running system cannot do what the configuration asked for.
    #[error("unsupported on this system: {0}")]
    Unsupported(#[from] Unsupported),

    /// The capture thread terminated unexpectedly.
    #[error("the capture thread terminated unexpectedly")]
    WorkerGone,

    #[allow(missing_docs)]
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),

    #[cfg(target_os = "windows")]
    #[allow(missing_docs)]
    #[error("Failed to call Windows Api: {0}")]
    Win(#[from] windows::core::Error),

    #[cfg(target_os = "linux")]
    #[allow(missing_docs)]
    #[error("Failed to call D-Bus Api: {0}")]
    Dbus(#[from] dbus::Error),
    #[cfg(target_os = "linux")]
    #[allow(missing_docs)]
    #[error("Failed to call PipeWire Api: {0}")]
    Pipewire(#[from] pipewire::Error),
    /// The ScreenCast portal refused the request or answered it with something unusable.
    #[cfg(target_os = "linux")]
    #[error("the ScreenCast portal: {0}")]
    Portal(#[from] PortalError),
    /// The PipeWire stream parameters could not be built. A bug in this crate.
    #[cfg(target_os = "linux")]
    #[error("could not build the PipeWire stream parameters")]
    SpaParams,

    /// ScreenCaptureKit reported a failure with no more specific meaning here.
    #[cfg(target_os = "macos")]
    #[error("ScreenCaptureKit: {0}")]
    ScreenCaptureKit(#[from] ScreenCaptureKitError),
}

/// A capability the running system does not provide.
///
/// Every variant is a hard no for this machine, so the only useful reactions are to turn
/// the feature off and retry, or to tell the user why it is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum Unsupported {
    /// Screen capture at all: Windows.Graphics.Capture is missing or disabled.
    #[error("screen capture")]
    ScreenCapture,
    /// Direct3D 11 with BGRA support, which the Windows backend copies frames through.
    #[error("Direct3D 11")]
    Direct3D,
    /// Hiding windows from the capture. No Wayland or X11 mechanism exists for it: the
    /// portal's `SelectSources` takes no exclusion list, and X11 capture is an unmediated
    /// read of the root window.
    #[error("hiding windows from the capture")]
    HideWindows,
    /// Capturing one window. On Linux the portal's own picker makes the final choice, so a
    /// caller cannot name the window.
    #[error("capturing a single window")]
    WindowTarget,
    /// The system picker. macOS needs 14.0 or newer for `SCContentSharingPicker`.
    #[error("the system picker")]
    Picker,
    /// Capturing audio. macOS needs 13.0 or newer for it.
    #[error("capturing audio")]
    Audio,
    /// The audio format the system negotiated is not one [`SampleFmt`] can name.
    ///
    /// [`SampleFmt`]: crate::format::SampleFmt
    #[error("the negotiated audio sample format")]
    SampleFormat,
}

/// Which ScreenCast portal call a [`PortalError`] came from.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PortalCall {
    /// `org.freedesktop.portal.ScreenCast.CreateSession`
    CreateSession,
    /// `org.freedesktop.portal.ScreenCast.SelectSources`
    SelectSources,
    /// `org.freedesktop.portal.ScreenCast.Start`
    Start,
}

#[cfg(target_os = "linux")]
impl core::fmt::Display for PortalCall {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::CreateSession => "CreateSession",
            Self::SelectSources => "SelectSources",
            Self::Start => "Start",
        })
    }
}

/// A failure reported by the XDG desktop portal.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PortalError {
    /// The portal advertises no source of the kind the target asked for, so the picker
    /// would have nothing to offer. Both fields are `AvailableSourceTypes` bitmasks.
    #[error("no source of the requested kind (wanted {wanted:#x}, available {available:#x})")]
    NoMatchingSource {
        /// What [`Target`](crate::config::Target) asked for.
        wanted: u32,
        /// What the portal has.
        available: u32,
    },
    /// The portal answered the call with a failure code other than "cancelled".
    #[error("{call} failed with response {response}")]
    Refused {
        /// The call that failed.
        call: PortalCall,
        /// The portal's `Response` code.
        response: u32,
    },
    /// The reply arrived without something the ScreenCast spec requires.
    #[error("{call} returned a malformed reply")]
    MalformedReply {
        /// The call that answered.
        call: PortalCall,
    },
    /// The request signal never carried a response.
    #[error("{call} produced no response")]
    NoResponse {
        /// The call that was waited on.
        call: PortalCall,
    },
}

/// A failure reported by ScreenCaptureKit.
///
/// The cases a caller can act on -- permission refused, the user cancelling, the user
/// pressing Stop Sharing -- are lifted into [`CaptureError`] itself; what is left is
/// carried here with its raw `NSError` code.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ScreenCaptureKitError {
    /// `getShareableContent` returned neither content nor an error.
    #[error("no shareable content")]
    NoShareableContent,
    /// The stream would not start.
    #[error("the stream failed to start")]
    FailedToStart,
    /// The system ended the stream -- the user pressed Stop Sharing, or the display went
    /// away. The frame channels close right after.
    #[error("the system stopped the stream")]
    Stopped,
    /// A frame or audio output could not be attached to the stream.
    #[error("the stream output could not be attached")]
    OutputAttach,
    /// Some other `SCStreamErrorDomain` code; see `SCStreamErrorCode`.
    #[error("SCStream error {0}")]
    Stream(isize),
    /// An `NSError` from a domain other than `SCStreamErrorDomain`.
    #[error("error {0} from another error domain")]
    Other(isize),
}

/// Result type for the screen capture library, using `CaptureError` for error handling.
pub type Result<T> = core::result::Result<T, CaptureError>;
