use std::fmt;

use crate::frame;

/// Audio frames
pub struct AFrame {
    pub aframe: Vec<u8>,
    pub nb_samples: i32,
    pub sample_rate: i32,
    pub nb_channels: i32,
    pub sample_fmt: i32,
}

impl fmt::Debug for AFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[allow(unused)]
        #[derive(Debug)]
        struct AFrameDebug {
            aframe: usize,
            nb_samples: i32,
            sample_rate: i32,
            nb_channels: i32,
            sample_fmt: i32,
        }
        fmt::Debug::fmt(
            &AFrameDebug {
                aframe: self.aframe.len(),
                nb_samples: self.nb_samples,
                sample_rate: self.sample_rate,
                nb_channels: self.nb_samples,
                sample_fmt: self.sample_fmt,
            },
            f,
        )
    }
}

impl AsRef<[u8]> for AFrame {
    fn as_ref(&self) -> &[u8] {
        self.aframe.as_ref()
    }
}

impl frame::AFrame for AFrame {
    fn nb_samples(&self) -> i32 {
        self.nb_samples
    }

    fn sample_rate(&self) -> i32 {
        self.sample_rate
    }

    fn nb_channels(&self) -> i32 {
        self.nb_channels
    }

    fn sample_fmt(&self) -> i32 {
        self.sample_fmt
    }
}

/// Audio frames
pub struct AFrameRef<'a> {
    pub aframe: &'a [u8],
    pub nb_samples: i32,
    pub sample_rate: i32,
    pub nb_channels: i32,
    pub sample_fmt: i32,
}

impl fmt::Debug for AFrameRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[allow(unused)]
        #[derive(Debug)]
        struct AFrameRefDebug {
            aframe: usize,
            nb_samples: i32,
            sample_rate: i32,
            nb_channels: i32,
            sample_fmt: i32,
        }
        fmt::Debug::fmt(
            &AFrameRefDebug {
                aframe: self.aframe.len(),
                nb_samples: self.nb_samples,
                sample_rate: self.sample_rate,
                nb_channels: self.nb_samples,
                sample_fmt: self.sample_fmt,
            },
            f,
        )
    }
}

impl AsRef<[u8]> for AFrameRef<'_> {
    fn as_ref(&self) -> &[u8] {
        self.aframe
    }
}

impl frame::AFrame for AFrameRef<'_> {
    fn nb_samples(&self) -> i32 {
        self.nb_samples
    }

    fn sample_rate(&self) -> i32 {
        self.sample_rate
    }

    fn nb_channels(&self) -> i32 {
        self.nb_channels
    }

    fn sample_fmt(&self) -> i32 {
        self.sample_fmt
    }
}
