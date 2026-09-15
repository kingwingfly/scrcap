//! CaptureDesc trait

use std::ops::Deref;

use crossbeam_channel::Receiver;

use crate::{
    Frame,
    config::CaptureConfig,
    error::{CaptureError, Result},
};

/// CaptureDesc trait
pub trait CaptureDescriptor:
    Deref<Target = Receiver<Frame>> + TryFrom<CaptureConfig, Error = CaptureError>
{
    /// Stop the capture.
    fn terminate(&self);

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
