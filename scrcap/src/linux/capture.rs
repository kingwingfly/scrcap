use std::{ops::Deref, sync::atomic::Ordering};

use crate::{
    capture_desc::CaptureDescriptor,
    config::CaptureConfig,
    error::{CaptureError, Result},
    frame::Frame,
    platform::pipewire::PipewireSession,
};

use crossbeam_channel::{Receiver, bounded};

impl CaptureConfig {
    /// Spawns a thread to capture the screen and returns a `CaptureDesc` that can be used to control the capture.
    pub fn create(self) -> Result<CaptureDesc> {
        let (tx, rx) = bounded(self.channel_capacity);
        let pipewire_session = PipewireSession::new(tx, self.video, self.audio);
        Ok(CaptureDesc {
            pipewire_session,
            rx: Some(rx),
        })
    }
}

/// A description of the capture, including control and size.
///
/// Drop this descriptor to terminate the capture and clean up resources.
#[derive(Debug)]
pub struct CaptureDesc {
    pipewire_session: PipewireSession,
    rx: Option<Receiver<Frame>>,
}

impl TryFrom<CaptureConfig> for CaptureDesc {
    type Error = CaptureError;

    fn try_from(config: CaptureConfig) -> std::result::Result<Self, Self::Error> {
        config.create()
    }
}

impl CaptureDescriptor for CaptureDesc {
    fn terminate(&self) {
        self.pipewire_session
            .terminate
            .store(true, Ordering::Relaxed);
    }

    fn size(&self) -> (u32, u32) {
        self.pipewire_session.size()
    }

    fn sample_rate(&self) -> Option<i32> {
        self.pipewire_session.sample_rate()
    }
}

impl Deref for CaptureDesc {
    type Target = Receiver<Frame>;

    fn deref(&self) -> &Self::Target {
        self.rx.as_ref().unwrap()
    }
}

impl Drop for CaptureDesc {
    fn drop(&mut self) {
        let _ = self.rx.take();
    }
}
