//! CaptureDesc trait

use crossbeam_channel::Receiver;

use crate::{
    config::CaptureConfig,
    error::{CaptureError, Result},
    frame::{AudioFrame, VideoFrame},
};

/// CaptureDesc trait
pub trait CaptureDescriptor: TryFrom<CaptureConfig, Error = CaptureError> {
    /// Stop the capture.
    fn terminate(&self);

    /// The captured video frames.
    fn video(&self) -> &Receiver<VideoFrame>;

    /// The captured audio frames, if audio was configured.
    fn audio(&self) -> Option<&Receiver<AudioFrame>>;

    /// Width and height of the captured video.
    fn size(&self) -> (u32, u32);

    /// Sample rate of the captured audio, once the format has been negotiated.
    ///
    /// `None` both when no audio was configured and while the system has not settled the
    /// format yet, which on Linux and macOS happens after `create` returns -- waiting for it
    /// there would mean hanging when a machine has nothing to capture. Every [`AudioFrame`]
    /// carries its own rate, so the first frame always answers this.
    fn sample_rate(&self) -> Option<i32>;

    /// Restart the capture with a new configuration.
    ///
    /// Takes the old descriptor by value and drops it first: both refer to the same OS
    /// windows, and tearing the old one down afterwards would restore the capture affinity
    /// the new one just set, putting a `hide`den window back into the recording.
    fn update_config(self, config: CaptureConfig) -> Result<Self>
    where
        Self: Sized,
    {
        drop(self);
        Self::try_from(config)
    }
}
