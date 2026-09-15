//! A dummy capture implementation that generates random frames.
//!
//! The frames are BGRA (28), with a resolution of 1080x720. No audio.

use crate::{
    capture_desc::CaptureDescriptor,
    config::CaptureConfig,
    error::CaptureError,
    format::PixFmt,
    fps::FpsGate,
    frame::{AudioFrame, VideoFrame},
};

use std::{
    iter::repeat_with,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, bounded};
use rand::{RngExt as _, SeedableRng as _, rngs::SmallRng};

use crate::error::Result;

const WIDTH: u32 = 1080;
const HEIGHT: u32 = 720;

impl CaptureConfig {
    /// Create a new capture configuration.
    pub fn create(self) -> Result<CaptureDesc> {
        let (tx, rx) = bounded(self.video.channel_capacity);
        let mut gate = FpsGate::new(self.video.fps);
        let control = Arc::new(AtomicBool::new(false));
        let control_ = Arc::clone(&control);
        std::thread::Builder::new()
            .name("Capture".to_string())
            .spawn(move || {
                let start = Instant::now();
                let mut rng = SmallRng::from_seed([42; 32]);
                let num_pixel = WIDTH as usize * HEIGHT as usize * 4;
                loop {
                    if control_.load(Ordering::Relaxed) {
                        break;
                    }
                    if tx.is_full() {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    let ts = start.elapsed().as_nanos() as u64;
                    if !gate.allow(ts) {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    let vframe = repeat_with(|| rng.random()).take(num_pixel).collect();
                    let _ = tx.try_send(VideoFrame {
                        vframe,
                        size: (WIDTH, HEIGHT),
                        pix_fmt: PixFmt::Bgra,
                        ts,
                    });
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
#[derive(Debug)]
pub struct CaptureDesc {
    control: Arc<AtomicBool>,
    size: (u32, u32),
    rx: Receiver<VideoFrame>,
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

    fn video(&self) -> &Receiver<VideoFrame> {
        &self.rx
    }

    fn audio(&self) -> Option<&Receiver<AudioFrame>> {
        None
    }

    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn sample_rate(&self) -> Option<i32> {
        None
    }
}

impl Drop for CaptureDesc {
    fn drop(&mut self) {
        self.terminate();
    }
}
