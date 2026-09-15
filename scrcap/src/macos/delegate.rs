use std::sync::Arc;

use crossbeam_channel::Sender;
use crossbeam_utils::sync::Unparker;
use objc2::{AnyThread as _, DefinedClass as _, define_class, msg_send, rc::Retained};
use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVReturnSuccess,
};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    // SCRecordingOutput, SCRecordingOutputDelegate,
    SCStream,
    SCStreamDelegate,
    SCStreamOutput,
    SCStreamOutputType,
};
use parking_lot::Mutex;
use tracing::error;

use crate::frame::Frame;

#[derive(Debug)]
pub(crate) struct StreamOutput {
    tx: Sender<Frame>,
    size: Arc<Mutex<(u32, u32)>>,
    sample_rate: Option<Arc<Mutex<i32>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = StreamOutput]
    #[derive(Debug)]
    pub(crate) struct VideoStreamOutput;

    unsafe impl NSObjectProtocol for VideoStreamOutput {}

    #[allow(non_snake_case)]
    unsafe impl SCStreamOutput for VideoStreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            unsafe {
                if !sample_buffer.data_is_ready() {
                    return;
                }
                match r#type {
                    SCStreamOutputType::Screen => {
                        let Some(buffer) = sample_buffer.image_buffer() else {
                            return;
                        };
                        if CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly)
                            != kCVReturnSuccess
                        {
                            return;
                        }
                        let bytes_per_row = CVPixelBufferGetBytesPerRow(&buffer);
                        // `CVPixelBufferGetWidth` shouldn't be used, it returns unmatched width,
                        // since it's the result without padding
                        let width = bytes_per_row / 4;
                        let height = CVPixelBufferGetHeight(&buffer);
                        let addr = CVPixelBufferGetBaseAddress(&buffer);
                        let vframe =
                            core::slice::from_raw_parts(addr as *const u8, bytes_per_row * height)
                                .to_vec();
                        CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly);
                        *self.ivars().size.lock() = (width as u32, height as u32);
                        let _ = self.ivars().tx.try_send(Frame::Video {
                            vframe,
                            size: (width as u32, height as u32),
                            pix_fmt: 28, // AV_PIX_FMT_BGRA
                        });
                    }
                    SCStreamOutputType::Audio => {
                        let Some(buffer) = sample_buffer.data_buffer() else {
                            return;
                        };
                        let nb_samples = sample_buffer.num_samples() as i32;
                        let format_desc = sample_buffer.format_description().unwrap();
                        let asbd = *CMAudioFormatDescriptionGetStreamBasicDescription(&format_desc);
                        *self.ivars().sample_rate.as_ref().unwrap().lock() =
                            asbd.mSampleRate as i32;
                        let (mut size, mut addr) = (0usize, core::ptr::null_mut());
                        if buffer.data_pointer(
                            0,
                            core::ptr::null_mut(),
                            &mut size as *mut _,
                            &mut addr as *mut _,
                        ) != 0
                        {
                            return;
                        }
                        let aframe = core::slice::from_raw_parts(addr as *const _, size).to_vec();
                        drop(buffer); // essential: ensure buffer is still referenced during copying
                        let _ = self.ivars().tx.send(Frame::Audio {
                            aframe,
                            nb_samples,
                            sample_rate: asbd.mSampleRate as i32,
                            nb_channels: asbd.mChannelsPerFrame as i32,
                            sample_fmt: 8, // AV_SAMPLE_FMT_FLTP
                        });
                    }
                    SCStreamOutputType::Microphone => {}
                    _ => unreachable!(),
                }
            }
        }
    }
);

impl VideoStreamOutput {
    pub(crate) fn new(
        tx: Sender<Frame>,
        size: Arc<Mutex<(u32, u32)>>,
        sample_rate: Option<Arc<Mutex<i32>>>,
    ) -> Retained<Self> {
        unsafe {
            let this = Self::alloc().set_ivars(StreamOutput {
                tx,
                size,
                sample_rate,
            });
            msg_send![super(this), init]
        }
    }
}

#[derive(Debug)]
pub(crate) struct StreamDelegateIvars {
    unparker: Unparker,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = StreamDelegateIvars]
    #[derive(Debug)]
    pub(crate) struct StreamDelegate;

    unsafe impl NSObjectProtocol for StreamDelegate {}

    #[allow(non_snake_case)]
    unsafe impl SCStreamDelegate for StreamDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, e: &NSError) {
            error!("stream stopped: {e}");
            self.ivars().unparker.unpark();
        }
    }
);

impl StreamDelegate {
    pub(crate) fn new(unparker: Unparker) -> Retained<Self> {
        unsafe {
            let this = Self::alloc().set_ivars(StreamDelegateIvars { unparker });
            msg_send![super(this), init]
        }
    }
}
