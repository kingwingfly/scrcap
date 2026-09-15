pub mod aframe;
pub mod frame;
pub mod vframe;

pub use aframe::*;
pub use vframe::*;

/// Represents a single frame of captured data.
#[derive(Debug, Clone)]
pub enum Frame {
    /// Video frame
    Video {
        /// Video data
        vframe: Vec<u8>,
        /// (Width, Height) of the video frame
        size: (u32, u32),
        /// Pixel format id of the video frame in ffmpeg
        /// (e.g., AV_PIX_FMT_BGRA is 28)
        pix_fmt: i32,
    },
    /// Audio frame
    Audio {
        /// Audio data
        aframe: Vec<u8>,
        /// Number of audio samples **per channel**
        nb_samples: i32,
        /// Sample rate of the audio
        sample_rate: i32,
        /// Number of audio channels
        nb_channels: i32,
        /// Sample format id of the audio frame in ffmpeg
        /// (e.g., AV_SAMPLE_FMT_FLTP is 8)
        sample_fmt: i32,
    },
}

impl AsRef<[u8]> for Frame {
    fn as_ref(&self) -> &[u8] {
        match self {
            Frame::Video { vframe, .. } => vframe,
            Frame::Audio { aframe, .. } => aframe,
        }
    }
}
