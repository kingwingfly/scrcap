use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use super::delegate::{PickerObserver, StreamDelegate, VideoStreamOutput};
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
use objc2::{
    AnyThread as _, MainThreadMarker,
    rc::Retained,
    runtime::{AnyClass, ProtocolObject},
    sel,
};
use objc2_app_kit::{NSView, NSWindow, NSWindowSharingType};
use objc2_core_media::{CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSError, NSObjectProtocol as _};
use objc2_screen_capture_kit::{
    SCContentFilter, SCContentSharingPicker, SCDisplay, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamConfigurationPreset, SCStreamOutputType, SCWindow,
};
use parking_lot::Mutex;
use regex::Regex;
use tracing::error;

impl CaptureConfig {
    /// Spawns a thread to capture the screen and returns a `CaptureDesc` that can be used to control the capture.
    ///
    /// `video.hide` touches `NSWindow`, which is main-thread-only, so when this is called
    /// from anywhere else that work is sent to the main thread and waited on. The caller's
    /// main thread therefore has to be running its loop -- the same thing `Target::Pick`
    /// needs of it.
    pub fn create(self) -> Result<CaptureDesc> {
        let re = match &self.video.target {
            Target::WindowName(pattern) => Some(
                Regex::new(pattern)
                    .map_err(|e| CaptureError::InvalidTarget(format!("bad window regex: {e}")))?,
            ),
            _ => None,
        };

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
            let views = &self.video.hide;
            let hidden = &mut hidden;
            let mut hide = move || {
                for ns_window in views
                    .iter()
                    .filter_map(|ns_view| unsafe { Retained::retain(*ns_view as *mut NSView) })
                    .filter_map(|ns_view| ns_view.window())
                {
                    let sharing_type = ns_window.sharingType();
                    ns_window.setSharingType(NSWindowSharingType::None);
                    // Raw pointer, not `Retained`, so the descriptor stays `Send`.
                    hidden.push((Retained::into_raw(ns_window) as isize, sharing_type));
                }
            };
            // `NSWindow` is main-thread-only, and `Target::Pick` forces `create` off it.
            match MainThreadMarker::new() {
                Some(_) => hide(),
                None => DispatchQueue::main().exec_sync(hide),
            }
        }

        let (setup_tx, setup_rx) = bounded::<Result<()>>(1);

        let jh = thread::spawn({
            let size = size.clone();
            let mut size_guard = size.lock_arc();
            let sample_rate = audio_desc.as_ref().map(|desc| desc.sample_rate.clone());
            let target = self.video.target.clone();
            move || unsafe {
                // Set by the stream delegate when the stream stops itself.
                let stopped = Arc::new(AtomicBool::new(false));
                // The delegate answers this too, so the wait below never needs a timeout.
                let (stop_tx, stop_rx) = bounded::<Option<String>>(1);
                let run = || -> Result<(Retained<SCStream>, Retained<VideoStreamOutput>, bool)> {
                    let (filter, width, height) = if target == Target::Pick {
                        present_picker()?
                    } else {
                        let (target_tx, target_rx) = bounded(1);
                        let block = RcBlock::new(
                            move |shareable: *mut SCShareableContent, e: *mut NSError| {
                                if !e.is_null() {
                                    let _ = target_tx.send(Err(CaptureError::ScreenCaptureKit(
                                        (*e).to_string(),
                                    )));
                                    return;
                                }
                                if shareable.is_null() {
                                    let _ = target_tx.send(Err(CaptureError::ScreenCaptureKit(
                                        "getShareableContent returned no content".into(),
                                    )));
                                    return;
                                }
                                let _ = target_tx.send(content_filter(
                                    &*shareable,
                                    &target,
                                    re.as_ref(),
                                ));
                            },
                        );
                        SCShareableContent::getShareableContentWithCompletionHandler(&block);
                        target_rx.recv().map_err(|_| CaptureError::WorkerGone)??
                    };

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

                    let stream_delegate = StreamDelegate::new(
                        parker.unparker().clone(),
                        stopped.clone(),
                        stop_tx.clone(),
                    );
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
                {
                    let output = ProtocolObject::from_ref(&*output_delegrate);
                    let _ =
                        stream.removeStreamOutput_type_error(output, SCStreamOutputType::Screen);
                    if has_audio {
                        let _ =
                            stream.removeStreamOutput_type_error(output, SCStreamOutputType::Audio);
                    }
                }
                // These own the senders; a consumer sees the end only once they drop.
                drop(output_delegrate);
                // Asking an already-stopped stream to stop can leave the completion handler
                // never firing, and it can stop itself right after this check -- which is why
                // the delegate answers `stop_rx` too.
                if !stopped.load(Ordering::Acquire) {
                    let stop_tx = stop_tx.clone();
                    stream.stopCaptureWithCompletionHandler(Some(&RcBlock::new(
                        move |e: *mut NSError| {
                            let _ = stop_tx.try_send((!e.is_null()).then(|| (*e).to_string()));
                        },
                    )));
                }
                if let Ok(Some(e)) = stop_rx.recv() {
                    error!("failed to stop capture: {e}");
                }
                Ok(())
            }
        });

        let setup = setup_rx.recv().unwrap_or(Err(CaptureError::WorkerGone));
        let _ = *size.lock(); // Guard in screen capture dropped -> capture started
        if let Err(e) = setup {
            let _ = jh.join();
            restore_sharing_types(hidden);
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

unsafe fn present_picker() -> Result<(Retained<SCContentFilter>, usize, usize)> {
    unsafe {
        if AnyClass::get(c"SCContentSharingPicker").is_none() {
            return Err(CaptureError::Unsupported);
        }
        let (tx, rx) = bounded(1);
        let observer = PickerObserver::new(tx);
        let picker = SCContentSharingPicker::sharedPicker();
        picker.addObserver(ProtocolObject::from_ref(&*observer));
        picker.setActive(true);
        picker.present();
        let filter = rx.recv().map_err(|_| CaptureError::WorkerGone)?;
        picker.setActive(false);
        picker.removeObserver(ProtocolObject::from_ref(&*observer));
        let filter = filter?;
        let (width, height) = filter_size(&filter).ok_or(CaptureError::Unsupported)?;
        Ok((filter, width, height))
    }
}

/// ScreenCaptureKit reports geometry in points, `SCStreamConfiguration` wants pixels.
/// Pixel dimensions of a filter, or `None` on a system whose `SCContentFilter` predates
/// these two selectors.
unsafe fn filter_size(filter: &SCContentFilter) -> Option<(usize, usize)> {
    unsafe {
        if !filter.respondsToSelector(sel!(contentRect))
            || !filter.respondsToSelector(sel!(pointPixelScale))
        {
            return None;
        }
        let rect = filter.contentRect();
        let scale = filter.pointPixelScale() as f64;
        Some((
            (rect.size.width * scale) as usize,
            (rect.size.height * scale) as usize,
        ))
    }
}

unsafe fn content_filter(
    shareable: &SCShareableContent,
    target: &Target,
    re: Option<&Regex>,
) -> Result<(Retained<SCContentFilter>, usize, usize)> {
    unsafe {
        // Point size kept as a fallback: `filter_size` needs selectors older systems lack.
        let from_display = |display: &SCDisplay| {
            (
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    display,
                    &NSArray::new(),
                ),
                (display.width() as usize, display.height() as usize),
            )
        };
        let from_window = |window: &SCWindow| {
            let frame = window.frame();
            (
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window),
                (frame.size.width as usize, frame.size.height as usize),
            )
        };
        let filter = match target {
            Target::Primary => shareable
                .displays()
                .firstObject()
                .map(|display| from_display(&display)),
            Target::Monitor(index) => usize::try_from(*index)
                .ok()
                .and_then(|index| {
                    let displays = shareable.displays();
                    (index < displays.len()).then(|| displays.objectAtIndex(index))
                })
                .map(|display| from_display(&display)),
            Target::Window(id) => u32::try_from(*id)
                .ok()
                .and_then(|id| shareable.windows().iter().find(|w| w.windowID() == id))
                .map(|window| from_window(&window)),
            Target::WindowName(_) => re.and_then(|re| {
                shareable
                    .windows()
                    .iter()
                    .find(|w| {
                        w.isOnScreen() && w.title().is_some_and(|t| re.is_match(&t.to_string()))
                    })
                    .map(|window| from_window(&window))
            }),
            Target::Pick => return Err(CaptureError::Unsupported),
        };
        let (filter, points) = filter.ok_or(CaptureError::TargetNotFound)?;
        let (width, height) = filter_size(&filter).unwrap_or(points);
        Ok((filter, width, height))
    }
}

fn restore_sharing_types(hidden: Vec<(isize, NSWindowSharingType)>) {
    if hidden.is_empty() {
        return;
    }
    // `NSWindow` is main-thread-only and `CaptureDesc` is `Send`, so a drop elsewhere hops.
    if MainThreadMarker::new().is_none() {
        DispatchQueue::main().exec_async(move || restore_sharing_types(hidden));
        return;
    }
    for (ptr, sharing_type) in hidden {
        // Retakes the reference `create` stored.
        let window = unsafe { Retained::from_raw(ptr as *mut NSWindow) };
        if let Some(window) = window {
            window.setSharingType(sharing_type);
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
    /// Owned `NSWindow` pointers, kept as `isize` so this stays `Send`.
    hidden: Vec<(isize, NSWindowSharingType)>,
}

impl CaptureVideoDesc {
    fn size(&self) -> (u32, u32) {
        *self.size.lock()
    }
}

impl Drop for CaptureVideoDesc {
    fn drop(&mut self) {
        restore_sharing_types(std::mem::take(&mut self.hidden));
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
