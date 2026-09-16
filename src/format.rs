//! Pixel and sample formats

/// Pixel format of a video frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PixFmt {
    /// Packed BGRA, 8 bits per channel.
    Bgra,
    /// Packed RGBA, 8 bits per channel.
    Rgba,
    /// Packed ARGB, 8 bits per channel.
    Argb,
    /// Packed ABGR, 8 bits per channel.
    Abgr,
    /// Packed BGRX, 8 bits per channel, the 4th byte is undefined.
    Bgr0,
    /// Packed RGBX, 8 bits per channel, the 4th byte is undefined.
    Rgb0,
}

impl PixFmt {
    /// Bytes one pixel occupies.
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Bgra | Self::Rgba | Self::Argb | Self::Abgr | Self::Bgr0 | Self::Rgb0 => 4,
        }
    }

    /// The matching ffmpeg `AVPixelFormat` id.
    pub const fn as_ffmpeg_id(self) -> i32 {
        match self {
            Self::Argb => 25,
            Self::Rgba => 26,
            Self::Abgr => 27,
            Self::Bgra => 28,
            Self::Rgb0 => 119,
            Self::Bgr0 => 121,
        }
    }

    /// The format an ffmpeg `AVPixelFormat` id denotes, if it is one of these.
    pub const fn from_ffmpeg_id(id: i32) -> Option<Self> {
        match id {
            25 => Some(Self::Argb),
            26 => Some(Self::Rgba),
            27 => Some(Self::Abgr),
            28 => Some(Self::Bgra),
            119 => Some(Self::Rgb0),
            121 => Some(Self::Bgr0),
            _ => None,
        }
    }
}

/// Sample format of an audio frame.
///
/// The `*P` variants are planar: one plane per channel, instead of channels interleaved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SampleFmt {
    /// Unsigned 8 bits, interleaved.
    U8,
    /// Signed 16 bits, interleaved.
    S16,
    /// Signed 32 bits, interleaved.
    S32,
    /// 32 bit float, interleaved.
    F32,
    /// 64 bit float, interleaved.
    F64,
    /// Unsigned 8 bits, planar.
    U8P,
    /// Signed 16 bits, planar.
    S16P,
    /// Signed 32 bits, planar.
    S32P,
    /// 32 bit float, planar.
    F32P,
    /// 64 bit float, planar.
    F64P,
}

impl SampleFmt {
    /// Whether each channel gets its own plane.
    pub const fn is_planar(self) -> bool {
        match self {
            Self::U8 | Self::S16 | Self::S32 | Self::F32 | Self::F64 => false,
            Self::U8P | Self::S16P | Self::S32P | Self::F32P | Self::F64P => true,
        }
    }

    /// Bytes one sample of one channel occupies.
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::U8 | Self::U8P => 1,
            Self::S16 | Self::S16P => 2,
            Self::S32 | Self::S32P | Self::F32 | Self::F32P => 4,
            Self::F64 | Self::F64P => 8,
        }
    }

    /// The matching ffmpeg `AVSampleFormat` id.
    pub const fn as_ffmpeg_id(self) -> i32 {
        match self {
            Self::U8 => 0,
            Self::S16 => 1,
            Self::S32 => 2,
            Self::F32 => 3,
            Self::F64 => 4,
            Self::U8P => 5,
            Self::S16P => 6,
            Self::S32P => 7,
            Self::F32P => 8,
            Self::F64P => 9,
        }
    }

    /// The format an ffmpeg `AVSampleFormat` id denotes, if it is one of these.
    pub const fn from_ffmpeg_id(id: i32) -> Option<Self> {
        match id {
            0 => Some(Self::U8),
            1 => Some(Self::S16),
            2 => Some(Self::S32),
            3 => Some(Self::F32),
            4 => Some(Self::F64),
            5 => Some(Self::U8P),
            6 => Some(Self::S16P),
            7 => Some(Self::S32P),
            8 => Some(Self::F32P),
            9 => Some(Self::F64P),
            _ => None,
        }
    }
}
