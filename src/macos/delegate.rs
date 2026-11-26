use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI32, Ordering},
};

use crossbeam_channel::Sender;
use crossbeam_utils::sync::Unparker;
use objc2::{
    AnyThread as _, DefinedClass as _, Message as _, define_class, msg_send, rc::Retained,
    runtime::AnyObject,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, kAudioFormatFlagIsBigEndian,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatFlagIsSignedInteger, kAudioFormatFlagsNativeEndian, kAudioFormatLinearPCM,
};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMClock, CMSampleBuffer,
    CMTime, CMTimeFlags,
};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferIsPlanar,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use objc2_screen_capture_kit::{
    SCContentFilter, SCContentSharingPicker, SCContentSharingPickerObserver, SCFrameStatus,
    SCStream, SCStreamDelegate, SCStreamErrorCode, SCStreamErrorDomain, SCStreamFrameInfoStatus,
    SCStreamOutput, SCStreamOutputType,
};
use parking_lot::Mutex;
use tracing::error;

use super::AtomicSize;
use crate::{
    error::{CaptureError, Result, ScreenCaptureKitError},
    format::{PixFmt, SampleFmt},
    fps::FpsGate,
    frame::{AudioFrame, VideoFrame},
    util::{pack_rows, row_span},
};

#[derive(Debug)]
pub(crate) struct StreamOutput {
    v_tx: Sender<VideoFrame>,
    a_tx: Option<Sender<AudioFrame>>,
    gate: Mutex<FpsGate>,
    size: Arc<AtomicSize>,
    sample_rate: Option<Arc<AtomicI32>>,
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
                        // A full channel would drop the frame anyway; do not lock or copy it.
                        if self.ivars().v_tx.is_full() {
                            return;
                        }
                        if !is_complete_frame(sample_buffer) {
                            return;
                        }
                        let Some(buffer) = sample_buffer.image_buffer() else {
                            return;
                        };
                        if CVPixelBufferGetPixelFormatType(&buffer) != kCVPixelFormatType_32BGRA
                            || CVPixelBufferIsPlanar(&buffer)
                        {
                            return;
                        }
                        if CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly)
                            != kCVReturnSuccess
                        {
                            return;
                        }
                        let bytes_per_row = CVPixelBufferGetBytesPerRow(&buffer);
                        let width = CVPixelBufferGetWidth(&buffer);
                        let height = CVPixelBufferGetHeight(&buffer);
                        let addr = CVPixelBufferGetBaseAddress(&buffer);
                        let layout = (|| -> Option<(usize, u32, u32)> {
                            if addr.is_null() {
                                return None;
                            }
                            let row_bytes = width.checked_mul(4)?;
                            row_span(bytes_per_row, row_bytes, height)?;
                            let w = u32::try_from(width).ok()?;
                            let h = u32::try_from(height).ok()?;
                            Some((row_bytes, w, h))
                        })();
                        let Some((row_bytes, w, h)) = layout else {
                            CVPixelBufferUnlockBaseAddress(
                                &buffer,
                                CVPixelBufferLockFlags::ReadOnly,
                            );
                            return;
                        };
                        if !self.ivars().gate.lock().allow(ts) {
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
                        self.ivars().size.store(w, h);
                        let _ = self.ivars().v_tx.try_send(VideoFrame {
                            vframe,
                            size: (w, h),
                            pix_fmt: PixFmt::Bgra,
                            ts,
                        });
                    }
                    SCStreamOutputType::Audio => {
                        let Ok(nb_samples) = i32::try_from(sample_buffer.num_samples()) else {
                            return;
                        };
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
                        let Ok(nb_channels) = i32::try_from(asbd.mChannelsPerFrame) else {
                            return;
                        };
                        if rate <= 0 || nb_channels <= 0 || nb_samples <= 0 {
                            return;
                        }

                        if let Some(sample_rate) = self.ivars().sample_rate.as_ref() {
                            sample_rate.store(rate, Ordering::Relaxed);
                        }
                        let Some(a_tx) = self.ivars().a_tx.as_ref() else {
                            return;
                        };
                        if a_tx.is_full() {
                            return;
                        }
                        let Some(aframe) = copy_audio_planes(
                            sample_buffer,
                            &asbd,
                            sample_fmt,
                            nb_samples as usize,
                        ) else {
                            return;
                        };
                        let _ = a_tx.try_send(AudioFrame {
                            aframe,
                            nb_samples,
                            sample_rate: rate,
                            nb_channels,
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

/// Whether ScreenCaptureKit marked this sample buffer as a complete video frame.
unsafe fn is_complete_frame(sample_buffer: &CMSampleBuffer) -> bool {
    unsafe {
        let Some(attachments) = sample_buffer.sample_attachments_array(false) else {
            return false;
        };
        let attachments: &NSArray<AnyObject> = attachments.cast_unchecked::<AnyObject>().as_ref();
        if attachments.is_empty() {
            return false;
        }
        let first = attachments.objectAtIndex(0);
        let Some(dict) = first.downcast_ref::<NSDictionary>() else {
            return false;
        };
        let key: &AnyObject = &*(SCStreamFrameInfoStatus as *const NSString).cast();
        let Some(status) = dict.objectForKey(key) else {
            return false;
        };
        let Some(status) = status.downcast_ref::<NSNumber>() else {
            return false;
        };
        SCFrameStatus(status.integerValue()) == SCFrameStatus::Complete
    }
}

/// A `CMTime` as nanoseconds on the host clock.
fn cm_time_ns(time: CMTime) -> Option<u64> {
    let (value, timescale, flags) = (time.value, time.timescale, time.flags);
    if !flags.contains(CMTimeFlags::Valid)
        || flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
        || timescale <= 0
        || value <= 0
    {
        return None;
    }
    u64::try_from(value as u128 * 1_000_000_000 / timescale as u128)
        .ok()
        .filter(|&ns| ns > 0)
}

/// The [`SampleFmt`] an `AudioStreamBasicDescription` describes, if it is one we can name.
fn asbd_sample_fmt(asbd: &AudioStreamBasicDescription) -> Option<SampleFmt> {
    if asbd.mFormatID != kAudioFormatLinearPCM {
        return None;
    }
    let flags = asbd.mFormatFlags;
    if flags & kAudioFormatFlagIsBigEndian != kAudioFormatFlagsNativeEndian {
        return None;
    }
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
unsafe fn copy_audio_planes(
    sample_buffer: &CMSampleBuffer,
    asbd: &AudioStreamBasicDescription,
    format: SampleFmt,
    samples: usize,
) -> Option<Vec<u8>> {
    unsafe {
        let (planes, channels_per_plane) = if format.is_planar() {
            (asbd.mChannelsPerFrame as usize, 1)
        } else {
            (1, asbd.mChannelsPerFrame)
        };
        let frame_bytes = format
            .bytes_per_sample()
            .checked_mul(channels_per_plane as usize)?;
        if frame_bytes != asbd.mBytesPerFrame as usize {
            return None;
        }
        let plane_bytes = frame_bytes.checked_mul(samples)?;
        let total = plane_bytes
            .checked_mul(planes)
            .filter(|&n| n <= isize::MAX as usize)?;
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
        let _block = Retained::from_raw(block)?;
        let capacity = needed.checked_sub(core::mem::offset_of!(AudioBufferList, mBuffers))?
            / size_of::<AudioBuffer>();
        if (*list).mNumberBuffers as usize != planes || planes > capacity {
            return None;
        }
        // mBuffers is the first element of a variable-length C array. Keep the pointer
        // into the entire list allocation when accessing its subsequent elements.
        let buffers = core::slice::from_raw_parts(
            (&raw const (*list).mBuffers).cast::<AudioBuffer>(),
            planes,
        );
        if buffers.iter().any(|buffer| {
            buffer.mData.is_null()
                || buffer.mNumberChannels != channels_per_plane
                || (buffer.mDataByteSize as usize) < plane_bytes
        }) {
            return None;
        }
        let mut aframe = Vec::with_capacity(total);
        for buffer in buffers {
            aframe.extend_from_slice(core::slice::from_raw_parts(
                buffer.mData as *const u8,
                plane_bytes,
            ));
        }
        Some(aframe)
    }
}

impl VideoStreamOutput {
    pub(crate) fn new(
        v_tx: Sender<VideoFrame>,
        a_tx: Option<Sender<AudioFrame>>,
        size: Arc<AtomicSize>,
        sample_rate: Option<Arc<AtomicI32>>,
        fps: Option<u32>,
    ) -> Retained<Self> {
        unsafe {
            let this = Self::alloc().set_ivars(StreamOutput {
                v_tx,
                a_tx,
                gate: Mutex::new(FpsGate::new(fps)),
                size,
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
            stream: Option<&SCStream>,
        ) {
            if stream.is_none() {
                let _ = self.ivars().tx.try_send(Ok(filter.retain()));
            }
        }

        #[unsafe(method(contentSharingPicker:didCancelForStream:))]
        unsafe fn contentSharingPicker_didCancelForStream(
            &self,
            _picker: &SCContentSharingPicker,
            stream: Option<&SCStream>,
        ) {
            if stream.is_none() {
                let _ = self.ivars().tx.try_send(Err(CaptureError::Cancelled));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn time(value: i64, timescale: i32, flags: CMTimeFlags) -> CMTime {
        CMTime {
            value,
            timescale,
            flags,
            epoch: 0,
        }
    }

    #[test]
    fn cm_time_ns_rejects_invalid_implied_non_positive_and_overflow() {
        assert_eq!(
            cm_time_ns(time(2, 1_000, CMTimeFlags::Valid)),
            Some(2_000_000)
        );
        assert_eq!(
            cm_time_ns(time(1, 1, CMTimeFlags::Valid | CMTimeFlags::HasBeenRounded)),
            Some(1_000_000_000)
        );
        assert_eq!(cm_time_ns(time(0, 1_000, CMTimeFlags::Valid)), None);
        assert_eq!(cm_time_ns(time(-5, 1_000, CMTimeFlags::Valid)), None);
        assert_eq!(cm_time_ns(time(5, 0, CMTimeFlags::Valid)), None);
        assert_eq!(cm_time_ns(time(5, -1, CMTimeFlags::Valid)), None);
        assert_eq!(cm_time_ns(time(5, 1, CMTimeFlags::empty())), None);
        assert_eq!(
            cm_time_ns(time(5, 1, CMTimeFlags::Valid | CMTimeFlags::Indefinite)),
            None
        );
        assert_eq!(
            cm_time_ns(time(
                5,
                1,
                CMTimeFlags::Valid | CMTimeFlags::PositiveInfinity
            )),
            None
        );
        assert_eq!(
            cm_time_ns(time(
                5,
                1,
                CMTimeFlags::Valid | CMTimeFlags::NegativeInfinity
            )),
            None
        );
        assert_eq!(cm_time_ns(time(i64::MAX, 1, CMTimeFlags::Valid)), None);
        assert_eq!(cm_time_ns(time(1, 2_000_000_000, CMTimeFlags::Valid)), None);
    }

    #[test]
    fn asbd_sample_fmt_rejects_big_endian_lpcm() {
        let make = |flags: u32, bits: u32| AudioStreamBasicDescription {
            mSampleRate: 48_000.0,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: flags,
            mBytesPerPacket: 4,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4,
            mChannelsPerFrame: 2,
            mBitsPerChannel: bits,
            mReserved: 0,
        };
        let f32_native = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;
        assert_eq!(asbd_sample_fmt(&make(f32_native, 32)), Some(SampleFmt::F32));
        assert_eq!(
            asbd_sample_fmt(&make(f32_native | kAudioFormatFlagIsNonInterleaved, 32)),
            Some(SampleFmt::F32P)
        );
        assert_eq!(
            asbd_sample_fmt(&make(f32_native | kAudioFormatFlagIsBigEndian, 32)),
            None
        );
        let s16_native = kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked;
        assert_eq!(asbd_sample_fmt(&make(s16_native, 16)), Some(SampleFmt::S16));
        assert_eq!(
            asbd_sample_fmt(&make(s16_native | kAudioFormatFlagIsBigEndian, 16)),
            None
        );
    }
}
