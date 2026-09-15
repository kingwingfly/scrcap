//! The frames that flow over the channels, and the traits encoders consume

use std::fmt;

use crate::format::{PixFmt, SampleFmt};

/// Owned video frame.
#[derive(Clone)]
pub struct VideoFrame {
    /// Video data
    pub vframe: Vec<u8>,
    /// (Width, Height) of the video frame
    pub size: (u32, u32),
    /// Pixel format of the video frame
    pub pix_fmt: PixFmt,
    /// Capture timestamp, see [`VFrame::ts`]
    pub ts: u64,
}

impl fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[allow(unused)]
        #[derive(Debug)]
        struct VideoFrameDebug {
            vframe: usize,
            size: (u32, u32),
            pix_fmt: PixFmt,
            ts: u64,
        }
        fmt::Debug::fmt(
            &VideoFrameDebug {
                vframe: self.vframe.len(),
                size: self.size,
                pix_fmt: self.pix_fmt,
                ts: self.ts,
            },
            f,
        )
    }
}

impl AsRef<[u8]> for VideoFrame {
    fn as_ref(&self) -> &[u8] {
        self.vframe.as_ref()
    }
}

impl VFrame for VideoFrame {
    fn size(&self) -> (u32, u32) {
        (self.size.0, self.size.1)
    }

    fn pix_fmt(&self) -> PixFmt {
        self.pix_fmt
    }

    fn ts(&self) -> u64 {
        self.ts
    }
}

/// Owned audio frame.
#[derive(Clone)]
pub struct AudioFrame {
    /// Audio data
    pub aframe: Vec<u8>,
    /// Number of audio samples **per channel**
    pub nb_samples: i32,
    /// Sample rate of the audio
    pub sample_rate: i32,
    /// Number of audio channels
    pub nb_channels: i32,
    /// Sample format of the audio frame
    pub sample_fmt: SampleFmt,
    /// Capture timestamp, see [`AFrame::ts`]
    pub ts: u64,
}

impl fmt::Debug for AudioFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[allow(unused)]
        #[derive(Debug)]
        struct AudioFrameDebug {
            aframe: usize,
            nb_samples: i32,
            sample_rate: i32,
            nb_channels: i32,
            sample_fmt: SampleFmt,
            ts: u64,
        }
        fmt::Debug::fmt(
            &AudioFrameDebug {
                aframe: self.aframe.len(),
                nb_samples: self.nb_samples,
                sample_rate: self.sample_rate,
                nb_channels: self.nb_channels,
                sample_fmt: self.sample_fmt,
                ts: self.ts,
            },
            f,
        )
    }
}

impl AsRef<[u8]> for AudioFrame {
    fn as_ref(&self) -> &[u8] {
        self.aframe.as_ref()
    }
}

impl AFrame for AudioFrame {
    fn nb_samples(&self) -> i32 {
        self.nb_samples
    }

    fn sample_rate(&self) -> i32 {
        self.sample_rate
    }

    fn nb_channels(&self) -> i32 {
        self.nb_channels
    }

    fn sample_fmt(&self) -> SampleFmt {
        self.sample_fmt
    }

    fn ts(&self) -> u64 {
        self.ts
    }
}

/// What a video frame must expose for an encoder to consume it.
pub trait VFrame: AsRef<[u8]> {
    /// (Width, Height) of the frame.
    fn size(&self) -> (u32, u32);
    /// Pixel format of the frame.
    fn pix_fmt(&self) -> PixFmt;
    /// When the frame was captured, in nanoseconds on a monotonic platform clock.
    ///
    /// The origin is platform defined and meaningless on its own, so subtract the first
    /// timestamp of the session. Video and audio of one session share the clock.
    fn ts(&self) -> u64;
}

/// What an audio frame must expose for an encoder to consume it.
pub trait AFrame: AsRef<[u8]> {
    /// Number of samples **per channel**.
    fn nb_samples(&self) -> i32;
    /// Sample rate of the frame.
    fn sample_rate(&self) -> i32;
    /// Number of channels.
    fn nb_channels(&self) -> i32;
    /// Sample format of the frame.
    fn sample_fmt(&self) -> SampleFmt;
    /// When the frame was captured, see [`VFrame::ts`].
    fn ts(&self) -> u64;
}
