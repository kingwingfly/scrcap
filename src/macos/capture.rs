use std::{
    sync::Arc,
    thread::{self, JoinHandle},
};

use super::delegate::{StreamDelegate, VideoStreamOutput};
use crate::{
    capture_desc::CaptureDescriptor,
    config::{CaptureConfig, Target},
    error::{CaptureError, Result},
    frame::{AudioFrame, VideoFrame},
};

use block2::RcBlock;
use crossbeam_channel::{Receiver, bounded};
use crossbeam_utils::sync::{Parker, Unparker};
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::{AnyThread as _, MainThreadMarker, rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{NSView, NSWindowSharingType};
use objc2_core_media::{CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamConfigurationPreset, SCStreamOutputType,
};
use parking_lot::Mutex;
use tracing::error;

impl CaptureConfig {
    /// Spawns a thread to capture the screen and returns a `CaptureDesc` that can be used to control the capture.
    ///
    /// Must be called on main thread if `self.video.hide` is set.
    pub fn create(self) -> Result<CaptureDesc> {
        if self.video.target != Target::Primary {
            return Err(CaptureError::Unsupported);
        }

        let (v_tx, v_rx) = bounded(self.video.channel_capacity);
        let (a_tx, a_rx) = match self.audio.as_ref() {
            Some(audio) => {
                let (tx, rx) = bounded(audio.channel_capacity);
                (Some(tx), Some(rx))
            }
            None => (None, None),
        };
        let size = Arc::new(Mutex::new((0, 0)));
        let parker = Parker::new();
        let unparker = parker.unparker().clone();
        let audio_desc = self.audio.as_ref().map(|_| CaptureAudioDesc {
            sample_rate: Arc::new(Mutex::new(0)),
        });
        let mut hidden = Vec::new();
        if !self.video.hide.is_empty() {
            assert!(
                MainThreadMarker::new().is_some(),
                "can only set window hidden on main thread"
            );
            for ns_window in self
                .video
                .hide
                .iter()
                .filter_map(|ns_view| unsafe { Retained::retain(*ns_view as *mut NSView) })
                .filter_map(|ns_view| ns_view.window())
            {
                hidden.push((
                    Retained::as_ptr(&ns_window) as isize,
                    ns_window.sharingType(),
                ));
                ns_window.setSharingType(NSWindowSharingType::None);
            }
        }

        let (setup_tx, setup_rx) = bounded::<Result<()>>(1);

        let jh = thread::spawn({
            let size = size.clone();
            let mut size_guard = size.lock_arc();
            let sample_rate = audio_desc.as_ref().map(|desc| desc.sample_rate.clone());
            let setup_tx = setup_tx.clone();
            move || unsafe {
                let run = || -> Result<(Retained<SCStream>, Retained<VideoStreamOutput>, bool)> {
                    let (target_tx, target_rx) = bounded(1);
                    let block =
                        RcBlock::new(move |shareable: *mut SCShareableContent, e: *mut NSError| {
                            if !e.is_null() {
                                let _ = target_tx
                                    .send(Err(CaptureError::ScreenCaptureKit((*e).to_string())));
                                return;
                            }
                            if shareable.is_null() {
                                let _ = target_tx.send(Err(CaptureError::ScreenCaptureKit(
                                    "getShareableContent returned no content".into(),
                                )));
                                return;
                            }
                            let Some(display) = (*shareable).displays().firstObject() else {
                                let _ = target_tx.send(Err(CaptureError::TargetNotFound));
                                return;
                            };
                            let _ = target_tx.send(Ok((
                                SCContentFilter::initWithDisplay_excludingWindows(
                                    SCContentFilter::alloc(),
                                    &display,
                                    &NSArray::new(),
                                ),
                                display.width() as usize,
                                display.height() as usize,
                            )));
                        });
                    SCShareableContent::getShareableContentWithCompletionHandler(&block);
                    let (filter, width, height) =
                        target_rx.recv().map_err(|_| CaptureError::WorkerGone)??;

                    let stream_config = SCStreamConfiguration::streamConfigurationWithPreset(
                        SCStreamConfigurationPreset::CaptureHDRScreenshotLocalDisplay,
                    );
                    stream_config.setWidth(width);
                    stream_config.setHeight(height);
                    stream_config.setCapturesAudio(self.audio.is_some());
                    stream_config.setCaptureMicrophone(false);
                    stream_config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));
                    if let Some(fps) = self.video.fps.filter(|fps| *fps > 0) {
                        stream_config.setMinimumFrameInterval(CMTime {
                            value: 1,
                            timescale: fps as i32,
                            flags: CMTimeFlags::Valid,
                            epoch: 0,
                        });
                    }

                    let stream_delegate = StreamDelegate::new(parker.unparker().clone());
                    let stream = SCStream::initWithFilter_configuration_delegate(
                        SCStream::alloc(),
                        &filter,
                        &stream_config,
                        Some(ProtocolObject::from_ref(&*stream_delegate)),
                    );
                    let output_delegrate =
                        VideoStreamOutput::new(v_tx, a_tx, size, sample_rate, self.video.fps);
                    let video_queue =
                        DispatchQueue::new("dev.scrcap.video", DispatchQueueAttr::SERIAL);
                    stream
                        .addStreamOutput_type_sampleHandlerQueue_error(
                            ProtocolObject::from_ref(&*output_delegrate),
                            SCStreamOutputType::Screen,
                            Some(&video_queue),
                        )
                        .map_err(|e| CaptureError::ScreenCaptureKit(e.to_string()))?;
                    let has_audio = self.audio.is_some();
                    if has_audio {
                        let queue =
                            DispatchQueue::new("dev.scrcap.audio", DispatchQueueAttr::SERIAL);
                        stream
                            .addStreamOutput_type_sampleHandlerQueue_error(
                                ProtocolObject::from_ref(&*output_delegrate),
                                SCStreamOutputType::Audio,
                                Some(&queue),
                            )
                            .map_err(|e| CaptureError::ScreenCaptureKit(e.to_string()))?;
                    }

                    let (start_tx, start_rx) = bounded(1);
                    stream.startCaptureWithCompletionHandler(Some(&RcBlock::new(
                        move |e: *mut NSError| {
                            let _ = start_tx.send((!e.is_null()).then(|| (*e).to_string()));
                        },
                    )));
                    match start_rx.recv() {
                        Ok(Some(e)) => return Err(CaptureError::ScreenCaptureKit(e)),
                        Ok(None) => {}
                        Err(_) => return Err(CaptureError::WorkerGone),
                    }
                    *size_guard = (width as u32, height as u32);
                    drop(size_guard); // Drop size_guard, the main thread continues
                    Ok((stream, output_delegrate, has_audio))
                };

                let (stream, output_delegrate, has_audio) = match run() {
                    Ok(v) => {
                        let _ = setup_tx.send(Ok(()));
                        v
                    }
                    Err(e) => {
                        let _ = setup_tx.send(Err(e));
                        return Ok(());
                    }
                };

                parker.park();
                let output = ProtocolObject::from_ref(&*output_delegrate);
                let _ = stream.removeStreamOutput_type_error(output, SCStreamOutputType::Screen);
                if has_audio {
                    let _ = stream.removeStreamOutput_type_error(output, SCStreamOutputType::Audio);
                }
                let (stop_tx, stop_rx) = bounded(1);
                stream.stopCaptureWithCompletionHandler(Some(&RcBlock::new(
                    move |e: *mut NSError| {
                        let _ = stop_tx.send((!e.is_null()).then(|| (*e).to_string()));
                    },
                )));
                if let Ok(Some(e)) = stop_rx.recv() {
                    error!("failed to stop capture: {e}");
                }
                Ok(())
            }
        });
        drop(setup_tx);

        let setup = setup_rx.recv().unwrap_or(Err(CaptureError::WorkerGone));
        let _ = *size.lock(); // Guard in screen capture dropped -> capture started
        if let Err(e) = setup {
            let _ = jh.join();
            restore_sharing_types(&hidden);
            return Err(e);
        }

        Ok(CaptureDesc {
            jh: Some(jh),
            unparker,
            v_rx: Some(v_rx),
            a_rx,
            video_desc: CaptureVideoDesc { size, hidden },
            audio_desc,
        })
    }
}

fn restore_sharing_types(hidden: &[(isize, NSWindowSharingType)]) {
    if hidden.is_empty() {
        return;
    }
    if MainThreadMarker::new().is_none() {
        error!("cannot restore window sharing type off the main thread");
        return;
    }
    for (ptr, sharing_type) in hidden {
        if let Some(window) = unsafe { Retained::retain(*ptr as *mut objc2_app_kit::NSWindow) } {
            window.setSharingType(*sharing_type);
        }
    }
}

/// A description of the capture, including control and size.
///
/// Drop this descriptor to terminate the capture and clean up resources.
#[derive(Debug)]
pub struct CaptureDesc {
    unparker: Unparker,
    v_rx: Option<Receiver<VideoFrame>>,
    a_rx: Option<Receiver<AudioFrame>>,
    video_desc: CaptureVideoDesc,
    audio_desc: Option<CaptureAudioDesc>,
    jh: Option<JoinHandle<Result<()>>>,
}

#[derive(Debug)]
struct CaptureVideoDesc {
    size: Arc<Mutex<(u32, u32)>>,
    hidden: Vec<(isize, NSWindowSharingType)>,
}

impl CaptureVideoDesc {
    fn size(&self) -> (u32, u32) {
        *self.size.lock()
    }
}

impl Drop for CaptureVideoDesc {
    fn drop(&mut self) {
        restore_sharing_types(&self.hidden);
    }
}

#[derive(Debug)]
struct CaptureAudioDesc {
    sample_rate: Arc<Mutex<i32>>,
}

impl CaptureAudioDesc {
    fn sample_rate(&self) -> i32 {
        *self.sample_rate.lock()
    }
}

impl TryFrom<CaptureConfig> for CaptureDesc {
    type Error = CaptureError;

    fn try_from(config: CaptureConfig) -> std::result::Result<Self, Self::Error> {
        config.create()
    }
}

impl CaptureDescriptor for CaptureDesc {
    fn terminate(&self) {
        self.unparker.unpark();
    }

    fn video(&self) -> &Receiver<VideoFrame> {
        self.v_rx.as_ref().unwrap()
    }

    fn audio(&self) -> Option<&Receiver<AudioFrame>> {
        self.a_rx.as_ref()
    }

    fn size(&self) -> (u32, u32) {
        self.video_desc.size()
    }

    fn sample_rate(&self) -> Option<i32> {
        self.audio_desc
            .as_ref()
            .map(|audio_desc| audio_desc.sample_rate())
    }
}

impl Drop for CaptureDesc {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.v_rx.take();
        let _ = self.a_rx.take();
        if let Some(jh) = self.jh.take() {
            let _ = jh.join();
        }
    }
}
