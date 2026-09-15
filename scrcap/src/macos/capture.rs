use std::{
    ops::Deref,
    sync::Arc,
    thread::{self, JoinHandle},
};

use super::delegate::{StreamDelegate, VideoStreamOutput};
use crate::{
    capture_desc::CaptureDescriptor,
    config::{CaptureConfig, Target},
    error::{CaptureError, Result},
    frame::Frame,
};

use block2::RcBlock;
use crossbeam_channel::{Receiver, bounded};
use crossbeam_utils::sync::{Parker, Unparker};
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::{AnyThread as _, MainThreadMarker, rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{NSView, NSWindowSharingType};
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
        let (tx, rx) = bounded(self.channel_capacity);
        let size = Arc::new(Mutex::new((0, 0)));
        let parker = Parker::new();
        let unparker = parker.unparker().clone();
        let audio_desc = self.audio.as_ref().map(|_| CaptureAudioDesc {
            sample_rate: Arc::new(Mutex::new(0)),
        });
        if !self.video.hide.is_empty() {
            assert!(
                MainThreadMarker::new().is_some(),
                "can only set window hidden on main thread"
            );
            for ns_windows in self
                .video
                .hide
                .iter()
                .filter_map(|ns_view| unsafe { Retained::retain(*ns_view as *mut NSView) })
                .filter_map(|ns_view| ns_view.window())
            {
                ns_windows.setSharingType(NSWindowSharingType::None)
            }
        }

        let jh = thread::spawn({
            let size = size.clone();
            let mut size_guard = size.lock_arc();
            let sample_rate = audio_desc.as_ref().map(|desc| desc.sample_rate.clone());
            move || unsafe {
                let (target_tx, target_rx) = bounded(1);
                let block =
                    RcBlock::new(move |shareable: *mut SCShareableContent, e: *mut NSError| {
                        debug_assert!(e.is_null(), "{}", &*e);
                        match self.video.target {
                            Target::Primary => {
                                let display = (*shareable)
                                    .displays()
                                    .firstObject()
                                    .expect("Primary display should exist");
                                let _ = target_tx.send((
                                    SCContentFilter::initWithDisplay_excludingWindows(
                                        SCContentFilter::alloc(),
                                        &display,
                                        &NSArray::new(),
                                    ),
                                    display.width() as usize,
                                    display.height() as usize,
                                ));
                            }
                            Target::Monitor(_) => todo!(),
                            Target::Window(_) => todo!(),
                            Target::WindowName(_) => todo!(),
                            Target::Pick => todo!(),
                        };
                    });
                SCShareableContent::getShareableContentWithCompletionHandler(&block);
                let (filter, width, height) = target_rx.recv().unwrap();

                let stream_config = SCStreamConfiguration::streamConfigurationWithPreset(
                    SCStreamConfigurationPreset::CaptureHDRScreenshotLocalDisplay,
                );
                stream_config.setWidth(width);
                stream_config.setHeight(height);
                stream_config.setCapturesAudio(self.audio.is_some());
                stream_config.setCaptureMicrophone(false);
                stream_config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));

                let stream_delegate = StreamDelegate::new(parker.unparker().clone());
                let stream = SCStream::initWithFilter_configuration_delegate(
                    SCStream::alloc(),
                    &filter,
                    &stream_config,
                    Some(ProtocolObject::from_ref(&*stream_delegate)),
                );
                let output_delegrate = VideoStreamOutput::new(tx, size, sample_rate);
                let video_queue = DispatchQueue::new("dev.scrcap.video", DispatchQueueAttr::SERIAL);
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(
                        ProtocolObject::from_ref(&*output_delegrate),
                        SCStreamOutputType::Screen,
                        Some(&video_queue),
                    )
                    .unwrap();
                let audio_queue = self.audio.is_some().then(|| {
                    let queue = DispatchQueue::new("dev.scrcap.audio", DispatchQueueAttr::SERIAL);
                    stream
                        .addStreamOutput_type_sampleHandlerQueue_error(
                            ProtocolObject::from_ref(&*output_delegrate),
                            SCStreamOutputType::Audio,
                            Some(&queue),
                        )
                        .unwrap();
                    queue
                });

                stream.startCaptureWithCompletionHandler(Some(&RcBlock::new(|e: *mut NSError| {
                    if e.is_null() {
                        return;
                    }
                    error!("{}", &*e);
                })));
                *size_guard = (width as u32, height as u32);
                drop(size_guard); // Drop size_guard, the main thread continues
                parker.park();
                let output = ProtocolObject::from_ref(&*output_delegrate);
                let _ = stream.removeStreamOutput_type_error(output, SCStreamOutputType::Screen);
                if audio_queue.is_some() {
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
        let _ = *size.lock(); // Guard in screen capture dropped -> capture started
        Ok(CaptureDesc {
            jh: Some(jh),
            unparker,
            rx,
            video_desc: CaptureVideoDesc { size },
            audio_desc,
        })
    }
}

/// A description of the capture, including control and size.
///
/// Drop this descriptor to terminate the capture and clean up resources.
#[derive(Debug)]
pub struct CaptureDesc {
    unparker: Unparker,
    rx: Receiver<Frame>,
    video_desc: CaptureVideoDesc,
    audio_desc: Option<CaptureAudioDesc>,
    jh: Option<JoinHandle<Result<()>>>,
}

#[derive(Debug)]
struct CaptureVideoDesc {
    size: Arc<Mutex<(u32, u32)>>,
}

impl CaptureVideoDesc {
    fn size(&self) -> (u32, u32) {
        *self.size.lock()
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

    fn size(&self) -> (u32, u32) {
        self.video_desc.size()
    }

    fn sample_rate(&self) -> Option<i32> {
        self.audio_desc
            .as_ref()
            .map(|audio_desc| audio_desc.sample_rate())
    }
}

impl Deref for CaptureDesc {
    type Target = Receiver<Frame>;

    fn deref(&self) -> &Self::Target {
        &self.rx
    }
}

impl Drop for CaptureDesc {
    fn drop(&mut self) {
        self.terminate();
        if let Some(jh) = self.jh.take() {
            let _ = jh.join();
        }
    }
}
