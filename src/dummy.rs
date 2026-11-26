//! A test-only dummy capture backend that generates synthetic frames without a display.
//!
//! Video frames are random 1080x720 [`PixFmt::Bgra`] images. When [`CaptureConfig::audio`] is
//! `Some`, silent 48 kHz stereo [`SampleFmt::F32`] audio frames are produced as well.
//!
//! Note: the `dummy` Cargo feature is **non-additive** and intended for headless tests only —
//! enabling it swaps out the real platform backend and omits `scrcap::platform`.

use crate::{
    capture_desc::CaptureDescriptor,
    config::CaptureConfig,
    error::CaptureError,
    format::{PixFmt, SampleFmt},
    fps::FpsGate,
    frame::{AudioFrame, VideoFrame},
};

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, bounded};
use rand::{RngExt as _, SeedableRng as _, rngs::SmallRng};

use crate::error::Result;

const WIDTH: u32 = 1080;
const HEIGHT: u32 = 720;
const AUDIO_SAMPLE_RATE: i32 = 48_000;
const AUDIO_CHANNELS: i32 = 2;
const AUDIO_SAMPLE_FMT: SampleFmt = SampleFmt::F32;
/// 10 ms of audio per packet (100 packets/s at 48 kHz).
const AUDIO_CHUNK_SAMPLES: i32 = 480;
const AUDIO_PACKETS_PER_SEC: u32 = 100;

fn elapsed_ns(start: &Instant) -> u64 {
    (start.elapsed().as_nanos().min(u64::MAX as u128) as u64).max(1)
}

impl CaptureConfig {
    /// Create a new capture configuration.
    pub fn create(self) -> Result<CaptureDesc> {
        self.validate()?;
        let (v_tx, v_rx) = bounded(self.video.channel_capacity);
        let (a_tx, a_rx, sample_rate) = match self.audio.as_ref() {
            Some(audio) => {
                let (tx, rx) = bounded(audio.channel_capacity);
                (Some(tx), Some(rx), Some(AUDIO_SAMPLE_RATE))
            }
            None => (None, None, None),
        };
        let mut video_gate = FpsGate::new(self.video.fps);
        let mut audio_gate = FpsGate::new(Some(AUDIO_PACKETS_PER_SEC));
        let control = Arc::new(AtomicBool::new(false));
        let control_ = Arc::clone(&control);
        let start = Instant::now();
        let jh = thread::Builder::new()
            .name("scrcap-dummy".to_string())
            .spawn(move || {
                let mut rng = SmallRng::from_seed([42; 32]);
                let num_bytes = WIDTH as usize * HEIGHT as usize * 4;
                let audio_bytes = AUDIO_CHUNK_SAMPLES as usize
                    * AUDIO_CHANNELS as usize
                    * AUDIO_SAMPLE_FMT.bytes_per_sample();
                loop {
                    if control_.load(Ordering::Acquire) {
                        break;
                    }
                    let mut sent = false;
                    if !v_tx.is_full() {
                        let ts = elapsed_ns(&start);
                        if video_gate.allow(ts) {
                            let mut vframe = vec![0u8; num_bytes];
                            rng.fill(&mut vframe[..]);
                            let _ = v_tx.try_send(VideoFrame {
                                vframe,
                                size: (WIDTH, HEIGHT),
                                pix_fmt: PixFmt::Bgra,
                                ts,
                            });
                            sent = true;
                        }
                    }
                    if let Some(a_tx) = &a_tx
                        && !a_tx.is_full()
                    {
                        let ts = elapsed_ns(&start);
                        if audio_gate.allow(ts) {
                            let _ = a_tx.try_send(AudioFrame {
                                aframe: vec![0u8; audio_bytes],
                                nb_samples: AUDIO_CHUNK_SAMPLES,
                                sample_rate: AUDIO_SAMPLE_RATE,
                                nb_channels: AUDIO_CHANNELS,
                                sample_fmt: AUDIO_SAMPLE_FMT,
                                ts,
                            });
                            sent = true;
                        }
                    }
                    if !sent {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            })?;
        Ok(CaptureDesc {
            control,
            size: (WIDTH, HEIGHT),
            rx: Some(v_rx),
            audio_rx: a_rx,
            sample_rate,
            jh: Some(jh),
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
    rx: Option<Receiver<VideoFrame>>,
    audio_rx: Option<Receiver<AudioFrame>>,
    sample_rate: Option<i32>,
    jh: Option<JoinHandle<()>>,
}

impl TryFrom<CaptureConfig> for CaptureDesc {
    type Error = CaptureError;

    fn try_from(config: CaptureConfig) -> std::result::Result<Self, Self::Error> {
        config.create()
    }
}

impl CaptureDescriptor for CaptureDesc {
    fn terminate(&self) {
        self.control.store(true, Ordering::Release);
    }

    fn video(&self) -> &Receiver<VideoFrame> {
        self.rx.as_ref().unwrap()
    }

    fn audio(&self) -> Option<&Receiver<AudioFrame>> {
        self.audio_rx.as_ref()
    }

    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn sample_rate(&self) -> Option<i32> {
        self.sample_rate
    }
}

impl Drop for CaptureDesc {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.rx.take();
        let _ = self.audio_rx.take();
        if let Some(jh) = self.jh.take() {
            let _ = jh.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AudioConfig, Target, VideoConfig};

    fn base_config(audio: bool) -> CaptureConfig {
        CaptureConfig {
            video: VideoConfig {
                channel_capacity: 2,
                hide: vec![],
                target: Target::Primary,
                fps: Some(60),
            },
            audio: audio.then_some(AudioConfig {
                channel_capacity: 16,
            }),
        }
    }

    #[test]
    fn video_only_omits_audio() {
        let capture = base_config(false).create().unwrap();
        assert!(capture.audio().is_none());
        assert_eq!(capture.sample_rate(), None);
        let frame = capture
            .video()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(frame.size, (WIDTH, HEIGHT));
        assert_eq!(frame.pix_fmt, PixFmt::Bgra);
        assert!(frame.ts > 0);
    }

    #[test]
    fn audio_config_produces_silent_frames_and_update_config_replaces_worker() {
        let capture = base_config(true).create().unwrap();
        assert_eq!(capture.sample_rate(), Some(AUDIO_SAMPLE_RATE));
        let audio = capture.audio().expect("audio channel should exist");
        let aframe = audio.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(aframe.ts > 0);
        assert_eq!(aframe.nb_samples, AUDIO_CHUNK_SAMPLES);
        assert_eq!(aframe.sample_rate, AUDIO_SAMPLE_RATE);
        assert_eq!(aframe.nb_channels, AUDIO_CHANNELS);
        assert_eq!(aframe.sample_fmt, AUDIO_SAMPLE_FMT);
        assert_eq!(
            aframe.aframe.len(),
            AUDIO_CHUNK_SAMPLES as usize
                * AUDIO_CHANNELS as usize
                * AUDIO_SAMPLE_FMT.bytes_per_sample()
        );
        assert!(aframe.aframe.iter().all(|&b| b == 0));

        // Replacing the config drops and joins the old worker before spawning the new one.
        let capture = capture.update_config(base_config(false)).unwrap();
        assert!(capture.audio().is_none());
        let vframe = capture
            .video()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(vframe.ts > 0);
    }
}
