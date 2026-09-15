use std::fmt;

use crate::frame;

/// Video frame
pub struct VFrame {
    pub vframe: Vec<u8>,
    pub size: (u32, u32),
    /// https://docs.rs/rsmpeg/latest/rsmpeg/?search=AV_PIX_FMT
    pub pix_fmt: i32,
}

impl fmt::Debug for VFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[allow(unused)]
        #[derive(Debug)]
        struct VFrameDebug {
            vframe: usize,
            size: (u32, u32),
            pix_fmt: i32,
        }
        fmt::Debug::fmt(
            &VFrameDebug {
                vframe: self.vframe.len(),
                size: self.size,
                pix_fmt: self.pix_fmt,
            },
            f,
        )
    }
}

impl AsRef<[u8]> for VFrame {
    fn as_ref(&self) -> &[u8] {
        self.vframe.as_ref()
    }
}

impl frame::VFrame for VFrame {
    fn size(&self) -> (u32, u32) {
        (self.size.0, self.size.1)
    }

    fn pix_fmt(&self) -> i32 {
        self.pix_fmt
    }
}

pub struct RealTimeVFrame {
    pub vframe: Vec<u8>,
    pub size: (u32, u32),
    pub pix_fmt: i32,
}

impl fmt::Debug for RealTimeVFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[allow(unused)]
        #[derive(Debug)]
        struct RealTimeVFrameDebug {
            vframe: usize,
            size: (u32, u32),
            pix_fmt: i32,
        }
        fmt::Debug::fmt(
            &RealTimeVFrameDebug {
                vframe: self.vframe.len(),
                size: self.size,
                pix_fmt: self.pix_fmt,
            },
            f,
        )
    }
}

impl AsRef<[u8]> for RealTimeVFrame {
    fn as_ref(&self) -> &[u8] {
        self.vframe.as_ref()
    }
}

impl frame::VFrame for RealTimeVFrame {
    fn size(&self) -> (u32, u32) {
        (self.size.0, self.size.1)
    }

    fn pix_fmt(&self) -> i32 {
        self.pix_fmt
    }

    fn ts(&self) -> Option<u128> {
        use std::{sync::LazyLock, time::Instant};

        static TIMER: LazyLock<Instant> = LazyLock::new(Instant::now);
        Some(TIMER.elapsed().as_millis())
    }
}

impl From<VFrame> for RealTimeVFrame {
    fn from(vframe: VFrame) -> Self {
        Self {
            vframe: vframe.vframe,
            size: vframe.size,
            pix_fmt: vframe.pix_fmt,
        }
    }
}

pub struct VFrameRef<'a> {
    pub vframe: &'a [u8],
    pub size: (u32, u32),
    pub pix_fmt: i32,
}

impl fmt::Debug for VFrameRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[allow(unused)]
        #[derive(Debug)]
        struct VFrameRefDebug {
            vframe: usize,
            size: (u32, u32),
            pix_fmt: i32,
        }
        fmt::Debug::fmt(
            &VFrameRefDebug {
                vframe: self.vframe.len(),
                size: self.size,
                pix_fmt: self.pix_fmt,
            },
            f,
        )
    }
}

impl AsRef<[u8]> for VFrameRef<'_> {
    fn as_ref(&self) -> &[u8] {
        self.vframe
    }
}

impl frame::VFrame for VFrameRef<'_> {
    fn size(&self) -> (u32, u32) {
        (self.size.0, self.size.1)
    }

    fn pix_fmt(&self) -> i32 {
        self.pix_fmt
    }
}

impl<'a> From<&'a VFrame> for VFrameRef<'a> {
    fn from(vframe: &'a VFrame) -> Self {
        Self {
            vframe: vframe.vframe.as_ref(),
            size: vframe.size,
            pix_fmt: vframe.pix_fmt,
        }
    }
}

pub struct RealTimeVFrameRef<'a> {
    pub vframe: &'a [u8],
    pub size: (u32, u32),
    pub pix_fmt: i32,
}

impl fmt::Debug for RealTimeVFrameRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[allow(unused)]
        #[derive(Debug)]
        struct RealTimeVFrameRefDebug {
            vframe: usize,
            size: (u32, u32),
            pix_fmt: i32,
        }
        fmt::Debug::fmt(
            &RealTimeVFrameRefDebug {
                vframe: self.vframe.len(),
                size: self.size,
                pix_fmt: self.pix_fmt,
            },
            f,
        )
    }
}

impl AsRef<[u8]> for RealTimeVFrameRef<'_> {
    fn as_ref(&self) -> &[u8] {
        self.vframe
    }
}

impl frame::VFrame for RealTimeVFrameRef<'_> {
    fn size(&self) -> (u32, u32) {
        (self.size.0, self.size.1)
    }

    fn pix_fmt(&self) -> i32 {
        self.pix_fmt
    }

    fn ts(&self) -> Option<u128> {
        use std::{sync::LazyLock, time::Instant};

        static TIMER: LazyLock<Instant> = LazyLock::new(Instant::now);
        Some(TIMER.elapsed().as_millis())
    }
}

impl<'a> From<&'a VFrame> for RealTimeVFrameRef<'a> {
    fn from(vframe: &'a VFrame) -> Self {
        Self {
            vframe: vframe.vframe.as_ref(),
            size: vframe.size,
            pix_fmt: vframe.pix_fmt,
        }
    }
}
