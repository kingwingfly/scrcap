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

    /// sample_rate of the captured audio.
    fn sample_rate(&self) -> Option<i32>;

    /// Update the capture configuration.
    fn update_config(&mut self, config: CaptureConfig) -> Result<()> {
        *self = Self::try_from(config)?;
        Ok(())
    }
}
