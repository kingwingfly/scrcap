use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use super::delegate::{PickerObserver, StreamDelegate, VideoStreamOutput, ns_error};
use crate::{
    capture_desc::CaptureDescriptor,
    config::{CaptureConfig, Target},
    error::{CaptureError, Result, ScreenCaptureKitError, Unsupported},
    frame::{AudioFrame, VideoFrame},
};

use block2::RcBlock;
use crossbeam_channel::{Receiver, bounded};
use crossbeam_utils::sync::{Parker, Unparker};
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::{
    AnyThread as _,
    rc::Retained,
    runtime::{AnyClass, ProtocolObject},
    sel,
};
use objc2_core_media::{CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSError, NSNumber, NSObjectProtocol as _};
use objc2_screen_capture_kit::{
    SCContentFilter, SCContentSharingPicker, SCContentSharingPickerConfiguration, SCDisplay,
    SCShareableContent, SCShareableContentStyle, SCStream, SCStreamConfiguration,
    SCStreamOutputType, SCWindow,
};
use parking_lot::Mutex;
use regex::Regex;
use tracing::error;

impl CaptureConfig {
    /// Spawns a thread to capture the screen and returns a `CaptureDesc` that can be used to control the capture.
    pub fn create(self) -> Result<CaptureDesc> {
        self.validate()?;
        let re = match &self.video.target {
            Target::WindowName(pattern) => Some(Regex::new(pattern)?),
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
            sample_rate: Arc::new(Mutex::new(None)),
        });
        let excluded: Vec<u32> = self
            .video
            .hide
            .iter()
            .filter_map(|id| u32::try_from(*id).ok())
            .collect();

        let (setup_tx, setup_rx) = bounded::<Result<()>>(1);

        let jh = thread::spawn({
            let size = size.clone();
            let sample_rate = audio_desc.as_ref().map(|desc| desc.sample_rate.clone());
            let target = self.video.target.clone();
            move || unsafe {
                // Set by the stream delegate when the stream stops itself.
                let stopped = Arc::new(AtomicBool::new(false));
                // The delegate answers this too, so the wait below never needs a timeout.
                let (stop_tx, stop_rx) = bounded::<Option<CaptureError>>(1);
                #[allow(clippy::type_complexity)]
                let run = || -> Result<(
                    Retained<SCStream>,
                    Retained<StreamDelegate>,
                    Retained<VideoStreamOutput>,
                    bool,
                )> {
                    let (filter, width, height) = if target == Target::Pick {
                        present_picker(&excluded)?
                    } else {
                        let content = shareable_content()?;
                        content_filter(&content, &target, re.as_ref(), &excluded)?
                    };

                    // Deliberately no preset: the HDR ones change the dynamic range and colour
                    // space, which 8-bit BGRA cannot carry, and only exist on macOS 15.
                    let stream_config = SCStreamConfiguration::new();
                    stream_config.setWidth(width);
                    stream_config.setHeight(height);
                    // macOS 13+; on 12.3 the selector is missing and sending it aborts.
                    if stream_config.respondsToSelector(sel!(setCapturesAudio:)) {
                        stream_config.setCapturesAudio(self.audio.is_some());
                    } else if self.audio.is_some() {
                        return Err(Unsupported::Audio.into());
                    }
                    if stream_config.respondsToSelector(sel!(setCaptureMicrophone:)) {
                        stream_config.setCaptureMicrophone(false);
                    }
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
                    // Before the output exists, so a real frame's size always lands after it.
                    *size.lock() = (width as u32, height as u32);
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
                        .map_err(|_| ScreenCaptureKitError::OutputAttach)?;
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
                            .map_err(|_| ScreenCaptureKitError::OutputAttach)?;
                    }

                    let (start_tx, start_rx) = bounded(1);
                    stream.startCaptureWithCompletionHandler(Some(&RcBlock::new(
                        move |e: *mut NSError| {
                            let _ = start_tx.send((!e.is_null()).then(|| ns_error(&*e)));
                        },
                    )));
                    match start_rx.recv() {
                        Ok(Some(e)) => return Err(e),
                        Ok(None) => {}
                        Err(_) => return Err(CaptureError::WorkerGone),
                    }
                    Ok((stream, stream_delegate, output_delegrate, has_audio))
                };

                // `stream_delegate` is held to the end: ScreenCaptureKit does not keep its
                // delegate alive, and without it a stream that stops itself -- Stop Sharing,
                // an unplugged display -- would never unpark this thread or answer `stop_rx`.
                let (stream, stream_delegate, output_delegrate, has_audio) = match run() {
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
                            let _ = stop_tx.try_send((!e.is_null()).then(|| ns_error(&*e)));
                        },
                    )));
                }
                if let Ok(Some(e)) = stop_rx.recv() {
                    error!("failed to stop capture: {e}");
                }
                drop(stream_delegate);
                Ok(())
            }
        });

        let setup = setup_rx.recv().unwrap_or(Err(CaptureError::WorkerGone));
        if let Err(e) = setup {
            let _ = jh.join();
            return Err(e);
        }

        Ok(CaptureDesc {
            jh: Some(jh),
            unparker,
            v_rx: Some(v_rx),
            a_rx,
            video_desc: CaptureVideoDesc { size },
            audio_desc,
        })
    }
}

/// Block on `getShareableContent`, which answers asynchronously on an internal queue.
unsafe fn shareable_content() -> Result<Retained<SCShareableContent>> {
    unsafe {
        let (tx, rx) = bounded(1);
        let block = RcBlock::new(move |shareable: *mut SCShareableContent, e: *mut NSError| {
            let _ = tx.send(if !e.is_null() {
                Err(ns_error(&*e))
            } else {
                Retained::retain(shareable).ok_or(ScreenCaptureKitError::NoShareableContent.into())
            });
        });
        SCShareableContent::getShareableContentWithCompletionHandler(&block);
        rx.recv().map_err(|_| CaptureError::WorkerGone)?
    }
}

unsafe fn present_picker(excluded: &[u32]) -> Result<(Retained<SCContentFilter>, usize, usize)> {
    unsafe {
        if AnyClass::get(c"SCContentSharingPicker").is_none() {
            return Err(Unsupported::Picker.into());
        }
        let (tx, rx) = bounded(1);
        let observer = PickerObserver::new(tx);
        let picker = SCContentSharingPicker::sharedPicker();
        // The picker is a process-wide singleton, so this configuration outlives the call
        // and would silently narrow the next one. Always install ours, always put back
        // what was there.
        let previous = picker.defaultConfiguration();
        let config = SCContentSharingPickerConfiguration::new();
        let ids: Vec<_> = excluded.iter().copied().map(NSNumber::new_u32).collect();
        config.setExcludedWindowIDs(&NSArray::from_retained_slice(&ids));
        picker.setDefaultConfiguration(&config);
        picker.addObserver(ProtocolObject::from_ref(&*observer));
        picker.setActive(true);
        picker.present();
        let filter = rx.recv().map_err(|_| CaptureError::WorkerGone);
        picker.setActive(false);
        picker.removeObserver(ProtocolObject::from_ref(&*observer));
        picker.setDefaultConfiguration(&previous);

        let filter = exclude_from_picked(filter??, excluded)?;
        let (width, height) = filter_size(&filter).ok_or(Unsupported::Picker)?;
        Ok((filter, width, height))
    }
}

/// Carry `hide` into the filter the picker handed back.
///
/// `excludedWindowIDs` only keeps a window out of the *picker*; the filter it returns for a
/// display still contains it. A display filter is therefore rebuilt with the exclusions,
/// and a window or application filter needs none -- it captures what the user named.
unsafe fn exclude_from_picked(
    filter: Retained<SCContentFilter>,
    excluded: &[u32],
) -> Result<Retained<SCContentFilter>> {
    unsafe {
        if excluded.is_empty() || filter.style() != SCShareableContentStyle::Display {
            return Ok(filter);
        }
        let display = filter
            .includedDisplays()
            .firstObject()
            .ok_or(Unsupported::HideWindows)?;
        let content = shareable_content()?;
        Ok(SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::from_retained_slice(&windows_by_id(&content, excluded)),
        ))
    }
}

/// The `SCWindow`s the given `CGWindowID`s name, skipping any that are gone.
unsafe fn windows_by_id(content: &SCShareableContent, ids: &[u32]) -> Vec<Retained<SCWindow>> {
    unsafe {
        if ids.is_empty() {
            return Vec::new();
        }
        content
            .windows()
            .iter()
            .filter(|window| ids.contains(&window.windowID()))
            .collect()
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

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// The `CGDirectDisplayID` of the display with the menu bar, the one `SCDisplay::displayID`
    /// matches for `Target::Primary`.
    safe fn CGMainDisplayID() -> u32;
}

unsafe fn content_filter(
    shareable: &SCShareableContent,
    target: &Target,
    re: Option<&Regex>,
    excluded: &[u32],
) -> Result<(Retained<SCContentFilter>, usize, usize)> {
    unsafe {
        let excluded_windows = windows_by_id(shareable, excluded);
        // Point size kept as a fallback: `filter_size` needs selectors older systems lack.
        let from_display = |display: &SCDisplay| {
            (
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    display,
                    &NSArray::from_retained_slice(&excluded_windows),
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
            // `displays` has no documented order, so the first one is not necessarily main.
            Target::Primary => shareable
                .displays()
                .iter()
                .find(|display| display.displayID() == CGMainDisplayID())
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
            Target::Pick => return Err(Unsupported::Picker.into()),
        };
        let (filter, points) = filter.ok_or(CaptureError::TargetNotFound)?;
        let (width, height) = filter_size(&filter).unwrap_or(points);
        Ok((filter, width, height))
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
}

impl CaptureVideoDesc {
    fn size(&self) -> (u32, u32) {
        *self.size.lock()
    }
}

#[derive(Debug)]
struct CaptureAudioDesc {
    sample_rate: Arc<Mutex<Option<i32>>>,
}

impl CaptureAudioDesc {
    fn sample_rate(&self) -> Option<i32> {
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
            .and_then(|audio_desc| audio_desc.sample_rate())
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
