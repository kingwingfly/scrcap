use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use crossbeam_channel::Sender;
use crossbeam_utils::sync::Unparker;
use objc2::{
    AnyThread as _, DefinedClass as _, Message as _, define_class, msg_send, rc::Retained,
};
use objc2_core_audio_types::{
    AudioBufferList, AudioStreamBasicDescription, kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked, kAudioFormatFlagIsSignedInteger,
    kAudioFormatLinearPCM,
};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMClock, CMSampleBuffer,
    CMTime, CMTimeFlags,
};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVReturnSuccess,
};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCContentSharingPicker, SCContentSharingPickerObserver, SCStream,
    SCStreamDelegate, SCStreamErrorCode, SCStreamErrorDomain, SCStreamOutput, SCStreamOutputType,
};
use parking_lot::Mutex;
use tracing::error;

use crate::{
    error::{CaptureError, Result, ScreenCaptureKitError},
    format::{PixFmt, SampleFmt},
    fps::FpsGate,
    frame::{AudioFrame, VideoFrame},
    util::pack_rows,
};

#[derive(Debug)]
pub(crate) struct StreamOutput {
    v_tx: Sender<VideoFrame>,
    a_tx: Option<Sender<AudioFrame>>,
    gate: Mutex<FpsGate>,
    size: Arc<Mutex<(u32, u32)>>,
    /// The last size written to `size`, packed as `width << 32 | height`.
    last_size: AtomicU64,
    sample_rate: Option<Arc<Mutex<Option<i32>>>>,
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
                let ts = cm_time_ns(sample_buffer.presentation_time_stamp())
                    .unwrap_or_else(|| cm_time_ns(CMClock::host_time_clock().time()).unwrap_or(0));
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
                        let width = CVPixelBufferGetWidth(&buffer);
                        let height = CVPixelBufferGetHeight(&buffer);
                        let addr = CVPixelBufferGetBaseAddress(&buffer);
                        let row_bytes = width * 4;
                        if addr.is_null()
                            || bytes_per_row < row_bytes
                            || !self.ivars().gate.lock().allow(ts)
                        {
                            CVPixelBufferUnlockBaseAddress(
                                &buffer,
                                CVPixelBufferLockFlags::ReadOnly,
                            );
                            return;
                        }
                        let vframe = pack_rows(addr as *const u8, bytes_per_row, row_bytes, height);
                        CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly);
                        let Some(vframe) = vframe else {
                            return;
                        };
                        let packed = (width as u64) << 32 | height as u64;
                        if self.ivars().last_size.swap(packed, Ordering::Relaxed) != packed {
                            *self.ivars().size.lock() = (width as u32, height as u32);
                        }
                        let _ = self.ivars().v_tx.try_send(VideoFrame {
                            vframe,
                            size: (width as u32, height as u32),
                            pix_fmt: PixFmt::Bgra,
                            ts,
                        });
                    }
                    SCStreamOutputType::Audio => {
                        let nb_samples = sample_buffer.num_samples() as i32;
                        let Some(format_desc) = sample_buffer.format_description() else {
                            return;
                        };
                        let asbd_ptr =
                            CMAudioFormatDescriptionGetStreamBasicDescription(&format_desc);
                        if asbd_ptr.is_null() {
                            return;
                        }
                        let asbd = *asbd_ptr;
                        let Some(sample_fmt) = asbd_sample_fmt(&asbd) else {
                            return;
                        };
                        let rate = asbd.mSampleRate as i32;
                        if rate <= 0 || asbd.mChannelsPerFrame == 0 {
                            return;
                        }
                        if let Some(sample_rate) = self.ivars().sample_rate.as_ref() {
                            *sample_rate.lock() = Some(rate);
                        }
                        let Some(aframe) = copy_audio_planes(sample_buffer) else {
                            return;
                        };
                        let Some(a_tx) = self.ivars().a_tx.as_ref() else {
                            return;
                        };
                        let _ = a_tx.try_send(AudioFrame {
                            aframe,
                            nb_samples,
                            sample_rate: rate,
                            nb_channels: asbd.mChannelsPerFrame as i32,
                            sample_fmt,
                            ts,
                        });
                    }
                    _ => {}
                }
            }
        }
    }
);

/// A `CMTime` as nanoseconds on the host clock.
fn cm_time_ns(time: CMTime) -> Option<u64> {
    let (value, timescale, flags) = (time.value, time.timescale, time.flags);
    if !flags.contains(CMTimeFlags::Valid) || timescale <= 0 || value < 0 {
        return None;
    }
    Some((value as u128 * 1_000_000_000 / timescale as u128) as u64)
}

/// The [`SampleFmt`] an `AudioStreamBasicDescription` describes, if it is one we can name.
fn asbd_sample_fmt(asbd: &AudioStreamBasicDescription) -> Option<SampleFmt> {
    if asbd.mFormatID != kAudioFormatLinearPCM {
        return None;
    }
    let flags = asbd.mFormatFlags;
    let planar = flags & kAudioFormatFlagIsNonInterleaved != 0;
    let float = flags & kAudioFormatFlagIsFloat != 0;
    let signed = flags & kAudioFormatFlagIsSignedInteger != 0;
    let packed = flags & kAudioFormatFlagIsPacked != 0;
    match (float, signed, packed, asbd.mBitsPerChannel) {
        (true, _, _, 32) => Some(if planar {
            SampleFmt::F32P
        } else {
            SampleFmt::F32
        }),
        (true, _, _, 64) => Some(if planar {
            SampleFmt::F64P
        } else {
            SampleFmt::F64
        }),
        (false, true, true, 16) => Some(if planar {
            SampleFmt::S16P
        } else {
            SampleFmt::S16
        }),
        (false, true, true, 32) => Some(if planar {
            SampleFmt::S32P
        } else {
            SampleFmt::S32
        }),
        (false, false, true, 8) => Some(if planar {
            SampleFmt::U8P
        } else {
            SampleFmt::U8
        }),
        _ => None,
    }
}

/// Copy the sample buffer's audio planes out back to back.
///
/// The plane pointers come from an `AudioBufferList`; the raw `CMBlockBuffer` is not
/// documented to hold them in that layout.
unsafe fn copy_audio_planes(sample_buffer: &CMSampleBuffer) -> Option<Vec<u8>> {
    unsafe {
        let mut needed = 0usize;
        if sample_buffer.audio_buffer_list_with_retained_block_buffer(
            &mut needed,
            core::ptr::null_mut(),
            0,
            None,
            None,
            0,
            core::ptr::null_mut(),
        ) != 0
            || needed == 0
        {
            return None;
        }
        let mut storage: Vec<AudioBufferList> =
            vec![core::mem::zeroed(); needed.div_ceil(size_of::<AudioBufferList>())];
        let list = storage.as_mut_ptr();
        let mut block: *mut CMBlockBuffer = core::ptr::null_mut();
        if sample_buffer.audio_buffer_list_with_retained_block_buffer(
            core::ptr::null_mut(),
            list,
            needed,
            None,
            None,
            0,
            &mut block,
        ) != 0
        {
            return None;
        }
        let block = Retained::from_raw(block);
        let buffers =
            core::slice::from_raw_parts((*list).mBuffers.as_ptr(), (*list).mNumberBuffers as usize);
        let mut aframe = Vec::with_capacity(buffers.iter().map(|b| b.mDataByteSize as usize).sum());
        for buffer in buffers {
            if buffer.mData.is_null() {
                return None;
            }
            aframe.extend_from_slice(core::slice::from_raw_parts(
                buffer.mData as *const u8,
                buffer.mDataByteSize as usize,
            ));
        }
        drop(block);
        Some(aframe)
    }
}

impl VideoStreamOutput {
    pub(crate) fn new(
        v_tx: Sender<VideoFrame>,
        a_tx: Option<Sender<AudioFrame>>,
        size: Arc<Mutex<(u32, u32)>>,
        sample_rate: Option<Arc<Mutex<Option<i32>>>>,
        fps: Option<u32>,
    ) -> Retained<Self> {
        unsafe {
            let this = Self::alloc().set_ivars(StreamOutput {
                v_tx,
                a_tx,
                gate: Mutex::new(FpsGate::new(fps)),
                size,
                last_size: AtomicU64::new(0),
                sample_rate,
            });
            msg_send![super(this), init]
        }
    }
}

/// The most actionable [`CaptureError`] an `NSError` from ScreenCaptureKit stands for.
///
/// The codes a caller can respond to become top-level variants; the rest keep their raw
/// `NSError` code so it can still be looked up.
pub(crate) fn ns_error(e: &NSError) -> CaptureError {
    let code = e.code();
    if &*e.domain() != unsafe { SCStreamErrorDomain } {
        return ScreenCaptureKitError::Other(code).into();
    }
    match SCStreamErrorCode(code) {
        SCStreamErrorCode::UserDeclined | SCStreamErrorCode::MissingEntitlements => {
            CaptureError::PermissionDenied
        }
        SCStreamErrorCode::UserStopped | SCStreamErrorCode::SystemStoppedStream => {
            ScreenCaptureKitError::Stopped.into()
        }
        SCStreamErrorCode::FailedToStart | SCStreamErrorCode::FailedToStartAudioCapture => {
            ScreenCaptureKitError::FailedToStart.into()
        }
        _ => ScreenCaptureKitError::Stream(code).into(),
    }
}

#[derive(Debug)]
pub(crate) struct StreamDelegateIvars {
    unparker: Unparker,
    /// Set before unparking, so the worker knows the stream is already stopped and must not
    /// ask it to stop again.
    stopped: Arc<AtomicBool>,
    /// Also answers the worker's stop wait, so that wait is satisfied whether the worker
    /// stopped the stream or the stream stopped itself first.
    stop_tx: Sender<Option<CaptureError>>,
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
            self.ivars().stopped.store(true, Ordering::Release);
            let _ = self.ivars().stop_tx.try_send(None);
            self.ivars().unparker.unpark();
        }
    }
);

impl StreamDelegate {
    pub(crate) fn new(
        unparker: Unparker,
        stopped: Arc<AtomicBool>,
        stop_tx: Sender<Option<CaptureError>>,
    ) -> Retained<Self> {
        unsafe {
            let this = Self::alloc().set_ivars(StreamDelegateIvars {
                unparker,
                stopped,
                stop_tx,
            });
            msg_send![super(this), init]
        }
    }
}

#[derive(Debug)]
pub(crate) struct PickerObserverIvars {
    tx: Sender<Result<Retained<SCContentFilter>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = PickerObserverIvars]
    #[derive(Debug)]
    pub(crate) struct PickerObserver;

    unsafe impl NSObjectProtocol for PickerObserver {}

    #[allow(non_snake_case)]
    unsafe impl SCContentSharingPickerObserver for PickerObserver {
        #[unsafe(method(contentSharingPicker:didUpdateWithFilter:forStream:))]
        unsafe fn contentSharingPicker_didUpdateWithFilter_forStream(
            &self,
            _picker: &SCContentSharingPicker,
            filter: &SCContentFilter,
            _stream: Option<&SCStream>,
        ) {
            let _ = self.ivars().tx.try_send(Ok(filter.retain()));
        }

        #[unsafe(method(contentSharingPicker:didCancelForStream:))]
        unsafe fn contentSharingPicker_didCancelForStream(
            &self,
            _picker: &SCContentSharingPicker,
            _stream: Option<&SCStream>,
        ) {
            let _ = self.ivars().tx.try_send(Err(CaptureError::Cancelled));
        }

        #[unsafe(method(contentSharingPickerStartDidFailWithError:))]
        unsafe fn contentSharingPickerStartDidFailWithError(&self, error: &NSError) {
            let _ = self.ivars().tx.try_send(Err(ns_error(error)));
        }
    }
);

impl PickerObserver {
    pub(crate) fn new(tx: Sender<Result<Retained<SCContentFilter>>>) -> Retained<Self> {
        unsafe {
            let this = Self::alloc().set_ivars(PickerObserverIvars { tx });
            msg_send![super(this), init]
        }
    }
}
