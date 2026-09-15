//! A dummy capture implementation that generates random frames.
//!
//! The frames are in RGBA8 format, with a resolution of 1920x1080.

use crate::{
    capture_desc::CaptureDescriptor, config::CaptureConfig, error::CaptureError, frame::Frame,
};

use std::{
    iter::repeat_with,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crossbeam_channel::{Receiver, bounded};
use rand::{RngExt as _, SeedableRng as _, rngs::SmallRng};

use crate::error::Result;

const WIDTH: u32 = 1080;
const HEIGHT: u32 = 720;

impl CaptureConfig {
    /// Create a new capture configuration.
    pub fn create(self) -> Result<CaptureDesc> {
        let (tx, rx) = bounded(self.channel_capacity);
        let control = Arc::new(AtomicBool::new(false));
        let control_ = Arc::clone(&control);
        std::thread::Builder::new()
            .name("Capture".to_string())
            .spawn(move || {
                let mut rng = SmallRng::from_seed([42; 32]);
                let num_pixel = WIDTH as usize * HEIGHT as usize * 4;
                loop {
                    if tx.is_full() {
                        continue;
                    }
                    let vframe = repeat_with(|| rng.random()).take(num_pixel).collect();
                    let _ = tx.try_send(Frame::Video {
                        vframe,
                        size: (WIDTH, HEIGHT),
                        pix_fmt: 28, // AV_PIX_FMT_BGRA
                    });
                    if control_.load(Ordering::Relaxed) {
                        break;
                    }
                }
            })?;
        Ok(CaptureDesc {
            control,
            size: (WIDTH, HEIGHT),
            rx,
        })
    }
}

/// A description of the capture, including control and size.
///
/// Drop this descriptor to terminate the capture and clean up resources.
#[derive(Debug, Clone)]
pub struct CaptureDesc {
    control: Arc<AtomicBool>,
    size: (u32, u32),
    rx: Receiver<Frame>,
}

impl TryFrom<CaptureConfig> for CaptureDesc {
    type Error = CaptureError;

    fn try_from(config: CaptureConfig) -> std::result::Result<Self, Self::Error> {
        config.create()
    }
}

impl CaptureDescriptor for CaptureDesc {
    fn terminate(&self) {
        self.control.store(true, Ordering::Relaxed);
    }

    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn sample_rate(&self) -> Option<i32> {
        None
    }
}

impl Deref for CaptureDesc {
    type Target = Receiver<Frame>;

    fn deref(&self) -> &Self::Target {
        &self.rx
    }
}

impl Drop for CaptureDesc {
    fn drop(&mut self) {
        self.terminate();
    }
}
