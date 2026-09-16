use crate::{
    capture_desc::CaptureDescriptor,
    config::CaptureConfig,
    error::{CaptureError, Result},
    frame::{AudioFrame, VideoFrame},
    platform::pipewire::PipewireSession,
};

use crossbeam_channel::{Receiver, bounded};

impl CaptureConfig {
    /// Spawns a thread to capture the screen and returns a `CaptureDesc` that can be used to control the capture.
    pub fn create(self) -> Result<CaptureDesc> {
        self.validate()?;
        let (v_tx, v_rx) = bounded(self.video.channel_capacity);
        let (a_tx, a_rx) = match self.audio.as_ref() {
            Some(audio) => {
                let (tx, rx) = bounded(audio.channel_capacity);
                (Some(tx), Some(rx))
            }
            None => (None, None),
        };
        let pipewire_session = PipewireSession::new(v_tx, a_tx, self.video, self.audio)?;
        Ok(CaptureDesc {
            pipewire_session,
            v_rx: Some(v_rx),
            a_rx,
        })
    }
}

/// A description of the capture, including control and size.
///
/// Drop this descriptor to terminate the capture and clean up resources.
#[derive(Debug)]
pub struct CaptureDesc {
    pipewire_session: PipewireSession,
    v_rx: Option<Receiver<VideoFrame>>,
    a_rx: Option<Receiver<AudioFrame>>,
}

impl TryFrom<CaptureConfig> for CaptureDesc {
    type Error = CaptureError;

    fn try_from(config: CaptureConfig) -> std::result::Result<Self, Self::Error> {
        config.create()
    }
}

impl CaptureDescriptor for CaptureDesc {
    fn terminate(&self) {
        self.pipewire_session.terminate();
    }

    fn video(&self) -> &Receiver<VideoFrame> {
        self.v_rx.as_ref().unwrap()
    }

    fn audio(&self) -> Option<&Receiver<AudioFrame>> {
        self.a_rx.as_ref()
    }

    fn size(&self) -> (u32, u32) {
        self.pipewire_session.size()
    }

    fn sample_rate(&self) -> Option<i32> {
        self.pipewire_session.sample_rate()
    }
}

impl Drop for CaptureDesc {
    fn drop(&mut self) {
        // Stop first: the callbacks only `try_send`, so disconnecting early just wastes copies.
        self.pipewire_session.terminate();
        let _ = self.v_rx.take();
        let _ = self.a_rx.take();
    }
}
