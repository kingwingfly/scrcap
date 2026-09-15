//! Error types for the screen capture library.

use thiserror::Error;

/// Represents errors that can occur in the screen capture library.
#[allow(missing_docs)]
#[derive(Debug, Error)]
pub enum CaptureError {
    #[cfg(target_os = "windows")]
    #[error("Failed to call Windows Api: {0}")]
    Win(#[from] windows::core::Error),

    #[cfg(target_os = "linux")]
    #[error("Failed to call D-Bus Api: {0}")]
    Dbus(#[from] dbus::Error),
    #[cfg(target_os = "linux")]
    #[error("Failed to call PipeWire Api: {0}")]
    Pipewire(#[from] pipewire::Error),

    #[error("Capture session is not supported on your OS.")]
    Unsupported,
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
    #[error("No target found for the capture.")]
    TargetNotFound,
}

/// Result type for the screen capture library, using `CaptureError` for error handling.
pub(crate) type Result<T> = core::result::Result<T, CaptureError>;
