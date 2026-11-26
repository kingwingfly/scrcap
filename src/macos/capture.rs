use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use super::AtomicSize;
use super::delegate::{PickerObserver, StreamDelegate, VideoStreamOutput, ns_error};
use crate::{
    capture_desc::CaptureDescriptor,
    config::{CaptureConfig, Target},
    error::{CaptureError, Result, ScreenCaptureKitError, Unsupported},
    frame::{AudioFrame, VideoFrame},
};

use block2::{DynBlock, RcBlock};
use crossbeam_channel::{Receiver, Sender, bounded};
use crossbeam_utils::sync::{Parker, Unparker};
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::{
    AnyThread as _, ClassType as _, msg_send,
    rc::Retained,
    runtime::{AnyClass, ProtocolObject},
    sel,
};
use objc2_core_media::{CMTime, kCMTimeZero};
use objc2_foundation::{NSArray, NSError, NSNumber, NSObjectProtocol as _};
use objc2_screen_capture_kit::{
    SCContentFilter, SCContentSharingPicker, SCContentSharingPickerConfiguration, SCDisplay,
    SCShareableContent, SCShareableContentStyle, SCStream, SCStreamConfiguration,
    SCStreamOutputType, SCWindow,
};
use parking_lot::{Mutex, MutexGuard};
use regex::Regex;
use tracing::error;

struct RunningStream {
    // Field order is the drop order: drop the output and stream before the delegate,
    // which ScreenCaptureKit holds unretained.
    output_delegate: Retained<VideoStreamOutput>,
    stream: Retained<SCStream>,
    stream_delegate: Retained<StreamDelegate>,
    has_audio: bool,
}

impl CaptureConfig {
    /// Spawns a thread to capture the screen and returns a `CaptureDesc`
    /// that can be used to control the capture.
    pub fn create(self) -> Result<CaptureDesc> {
        self.validate()?;
        let re = match &self.video.target {
            Target::WindowName(pattern) => Some(Regex::new(pattern)?),
            _ => None,
        };
        let excluded = parse_hide_ids(&self.video.hide)?;

        let (v_tx, v_rx) = bounded(self.video.channel_capacity);
        let (a_tx, a_rx) = match self.audio.as_ref() {
            Some(audio) => {
                let (tx, rx) = bounded(audio.channel_capacity);
                (Some(tx), Some(rx))
            }
            None => (None, None),
        };
        let size = Arc::new(AtomicSize::default());
        let parker = Parker::new();
        let unparker = parker.unparker().clone();
        let audio_desc = self.audio.as_ref().map(|_| CaptureAudioDesc {
            sample_rate: Arc::new(AtomicI32::new(0)),
        });

        let (setup_tx, setup_rx) = bounded::<Result<()>>(1);

        let jh = thread::Builder::new().name("scrcap-macos".into()).spawn({
            let size = size.clone();
            let sample_rate = audio_desc.as_ref().map(|desc| desc.sample_rate.clone());
            let target = self.video.target.clone();
            move || unsafe {
                let stopped = Arc::new(AtomicBool::new(false));
                // The delegate answers this too, so the wait below never needs a timeout.
                let (stop_tx, stop_rx) = bounded::<Option<CaptureError>>(1);
                let run = || -> Result<RunningStream> {
                    let (filter, width, height) = if target == Target::Pick {
                        present_picker(&excluded)?
                    } else {
                        let content = shareable_content()?;
                        content_filter(&content, &target, re.as_ref(), &excluded)?
                    };
                    let width_u32 = u32::try_from(width)
                        .ok()
                        .filter(|&w| w > 0)
                        .ok_or(ScreenCaptureKitError::FailedToStart)?;
                    let height_u32 = u32::try_from(height)
                        .ok()
                        .filter(|&h| h > 0)
                        .ok_or(ScreenCaptureKitError::FailedToStart)?;

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
                    let interval = match self.video.fps {
                        None | Some(0) => kCMTimeZero,
                        Some(fps) => CMTime::new(1, i32::try_from(fps).unwrap_or(i32::MAX)),
                    };
                    stream_config.setMinimumFrameInterval(interval);

                    // Declared before `stream` so early returns and unwinds drop `stream` first.
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
                    size.store(width_u32, height_u32);
                    let output_delegate =
                        VideoStreamOutput::new(v_tx, a_tx, size, sample_rate, self.video.fps);
                    let video_queue =
                        DispatchQueue::new("dev.scrcap.video", DispatchQueueAttr::SERIAL);
                    stream
                        .addStreamOutput_type_sampleHandlerQueue_error(
                            ProtocolObject::from_ref(&*output_delegate),
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
                                ProtocolObject::from_ref(&*output_delegate),
                                SCStreamOutputType::Audio,
                                Some(&queue),
                            )
                            .map_err(|_| ScreenCaptureKitError::OutputAttach)?;
                    }

                    let (start_tx, start_rx) = bounded(1);
                    stream.startCaptureWithCompletionHandler(Some(&RcBlock::new(
                        move |e: *mut NSError| {
                            let _ = start_tx.try_send((!e.is_null()).then(|| ns_error(&*e)));
                        },
                    )));
                    match start_rx.recv() {
                        Ok(Some(e)) => return Err(e),
                        Ok(None) => {}
                        Err(_) => return Err(CaptureError::WorkerGone),
                    }
                    Ok(RunningStream {
                        output_delegate,
                        stream,
                        stream_delegate,
                        has_audio,
                    })
                };

                // `stream_delegate` is bound first so it drops last even on unwind:
                // ScreenCaptureKit does not retain its delegate, and without it a stream that
                // stops itself -- Stop Sharing, an unplugged display -- would never unpark this
                // thread or answer `stop_rx`.
                let RunningStream {
                    stream_delegate,
                    stream,
                    output_delegate,
                    has_audio,
                } = match run() {
                    Ok(v) => {
                        let _ = setup_tx.send(Ok(()));
                        v
                    }
                    Err(e) => {
                        let _ = setup_tx.send(Err(e));
                        return;
                    }
                };

                parker.park();
                {
                    let output = ProtocolObject::from_ref(&*output_delegate);
                    let _ =
                        stream.removeStreamOutput_type_error(output, SCStreamOutputType::Screen);
                    if has_audio {
                        let _ =
                            stream.removeStreamOutput_type_error(output, SCStreamOutputType::Audio);
                    }
                }
                // These own the senders; a consumer sees the end only once they drop.
                drop(output_delegate);
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
                // Bounded, and not because the wait is expected to be slow: `stop_tx` is
                // still held here, so the channel can never close, and a `stopCapture`
                // whose completion block is simply dropped would park this thread for
                // good -- with `CaptureDesc::drop` joining it, that hangs the caller.
                match stop_rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(Some(e)) => error!("failed to stop capture: {e}"),
                    Ok(None) => {}
                    Err(_) => error!("ScreenCaptureKit never answered the stop request"),
                }
                drop(stream);
                drop(stream_delegate);
            }
        })?;

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

/// Validate `VideoConfig::hide` entries as non-zero `CGWindowID`s.
fn parse_hide_ids(hide: &[isize]) -> Result<Vec<u32>> {
    hide.iter()
        .map(|&id| {
            u32::try_from(id)
                .ok()
                .filter(|&id| id > 0)
                .ok_or(CaptureError::TargetNotFound)
        })
        .collect()
}

/// Block on an asynchronous `SCShareableContent` query.
unsafe fn await_shareable_content(
    fetch: impl FnOnce(&DynBlock<dyn Fn(*mut SCShareableContent, *mut NSError)>),
) -> Result<Retained<SCShareableContent>> {
    unsafe {
        let (tx, rx) = bounded(1);
        let block = RcBlock::new(move |shareable: *mut SCShareableContent, e: *mut NSError| {
            let _ = tx.try_send(if !e.is_null() {
                Err(ns_error(&*e))
            } else {
                Retained::retain(shareable).ok_or(ScreenCaptureKitError::NoShareableContent.into())
            });
        });
        fetch(&block);
        drop(block);
        rx.recv().map_err(|_| CaptureError::WorkerGone)?
    }
}

/// Block on `getShareableContent`, which answers asynchronously on an internal queue.
unsafe fn shareable_content() -> Result<Retained<SCShareableContent>> {
    unsafe {
        await_shareable_content(|block| {
            SCShareableContent::getShareableContentWithCompletionHandler(block);
        })
    }
}

/// Block on `getCurrentProcessShareableContent`, which requires no global TCC permission.
unsafe fn current_process_shareable_content() -> Result<Retained<SCShareableContent>> {
    unsafe {
        await_shareable_content(|block| {
            SCShareableContent::getCurrentProcessShareableContentWithCompletionHandler(block);
        })
    }
}

/// Main-thread state owned by [`PickerGuard`] while `SCContentSharingPicker` is presented.
struct PickerState {
    observer: Option<Retained<PickerObserver>>,
    previous: Option<Retained<SCContentSharingPickerConfiguration>>,
    deactivate: bool,
}

// SAFETY: `PickerState` is only ever constructed, accessed, and dropped on the main queue
// via `DispatchQueue::main().exec_sync` (when unarmed, all ObjC fields are `None`). While
// stored inside `PickerGuard` on the worker thread, it is never dereferenced or mutated.
unsafe impl Send for PickerState {}

impl PickerState {
    fn is_armed(&self) -> bool {
        self.observer.is_some() || self.previous.is_some() || self.deactivate
    }

    unsafe fn restore(mut self) {
        unsafe {
            let picker = SCContentSharingPicker::sharedPicker();
            if let Some(observer) = self.observer.take() {
                picker.removeObserver(ProtocolObject::from_ref(&*observer));
                drop(observer);
            }
            if let Some(previous) = self.previous.take() {
                picker.setDefaultConfiguration(&previous);
            }
            if self.deactivate {
                picker.setActive(false);
            }
        }
    }
}

/// Restores `SCContentSharingPicker`'s process-wide state on the main queue on exit or unwind.
struct PickerGuard {
    state: Option<PickerState>,
    // Held until `Drop::drop` finishes restoring the shared picker on the main queue.
    _presenting: MutexGuard<'static, ()>,
}

impl Drop for PickerGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.take()
            && state.is_armed()
        {
            DispatchQueue::main().exec_sync(move || unsafe {
                state.restore();
            });
        }
    }
}

/// Ferries the picker result channel sender onto the main queue to construct `PickerObserver`.
struct PickerSender(Sender<Result<Retained<SCContentFilter>>>);

// SAFETY: `SCContentFilter` is an immutable ScreenCaptureKit value object with atomic ObjC
// refcounting that ScreenCaptureKit delivers on the main queue and accepts on `SCStream`'s
// caller thread.
unsafe impl Send for PickerSender {}

unsafe fn present_picker(excluded: &[u32]) -> Result<(Retained<SCContentFilter>, usize, usize)> {
    // New-stream selections have no stream ID. Only one of our observers can own one.
    static PRESENTING: Mutex<()> = Mutex::new(());
    unsafe {
        if AnyClass::get(c"SCContentSharingPicker").is_none() {
            return Err(Unsupported::Picker.into());
        }
        let (tx, rx) = bounded(1);
        let tx = PickerSender(tx);
        let mut guard = PickerGuard {
            state: Some(PickerState {
                observer: None,
                previous: None,
                deactivate: false,
            }),
            _presenting: PRESENTING.lock(),
        };
        let state_slot = &mut guard.state;
        // `SCContentSharingPicker` drives system UI and observer callbacks on the main queue;
        // `present()` returns asynchronously, so we wait on `rx` back on the worker thread.
        DispatchQueue::main().exec_sync(|| {
            let tx = tx;
            let state = state_slot.as_mut().expect("picker state initialized");
            let picker = SCContentSharingPicker::sharedPicker();
            // The host may already restrict this shared picker. Layer our exclusions onto its
            // configuration only while we present it, arming restoration before each mutation.
            if !excluded.is_empty() {
                let previous = picker.defaultConfiguration();
                let updated = with_excluded_windows(&previous, excluded);
                state.previous = Some(previous);
                picker.setDefaultConfiguration(&updated);
            }
            let observer = state.observer.insert(PickerObserver::new(tx.0));
            picker.addObserver(ProtocolObject::from_ref(&**observer));
            if !picker.isActive() {
                state.deactivate = true;
                picker.setActive(true);
            }
            picker.present();
        });
        let filter = rx.recv().map_err(|_| CaptureError::WorkerGone);
        drop(guard);

        let filter = exclude_from_picked(filter??, excluded)?;
        let (width, height) = filter_size(&filter)?.ok_or(Unsupported::Picker)?;
        Ok((filter, width, height))
    }
}

unsafe fn with_excluded_windows(
    previous: &SCContentSharingPickerConfiguration,
    excluded: &[u32],
) -> Retained<SCContentSharingPickerConfiguration> {
    unsafe {
        // ScreenCaptureKit copies this configuration when setting it on the picker.
        let config: Retained<SCContentSharingPickerConfiguration> = msg_send![previous, copy];
        let ids: Vec<_> = excluded.iter().copied().map(NSNumber::new_u32).collect();
        let combined = previous
            .excludedWindowIDs()
            .arrayByAddingObjectsFromArray(&NSArray::from_retained_slice(&ids));
        config.setExcludedWindowIDs(&combined);
        config
    }
}

/// Carry `hide` into the filter the picker handed back.
///
/// `excludedWindowIDs` only keeps a window out of the *picker*; the filter it returns for a
/// display or application can still contain it. Rebuild those filters with exclusions.
unsafe fn exclude_from_picked(
    filter: Retained<SCContentFilter>,
    excluded: &[u32],
) -> Result<Retained<SCContentFilter>> {
    unsafe {
        if excluded.is_empty() || filter.style() == SCShareableContentStyle::Window {
            return Ok(filter);
        }
        // `includedDisplays` is macOS 15.2+, the picker only 14.0+: sending it to an older
        // filter raises an uncaught unrecognized-selector exception.
        if !filter.respondsToSelector(sel!(includedDisplays)) {
            return Err(Unsupported::HideWindows.into());
        }
        // Try current-process content first so hiding our own windows never demands global
        // Screen Recording TCC when the user only granted one-time picker consent.
        let has_current_process_api: bool = msg_send![
            SCShareableContent::class(),
            respondsToSelector: sel!(getCurrentProcessShareableContentWithCompletionHandler:)
        ];
        let mut hidden = if has_current_process_api {
            match current_process_shareable_content() {
                Ok(content) => {
                    let found = windows_by_id(&content, excluded);
                    excluded
                        .iter()
                        .all(|&id| found.iter().any(|w| w.windowID() == id))
                        .then_some(found)
                }
                Err(_) => None,
            }
        } else {
            None
        };
        let hidden = match hidden.take() {
            Some(found) => found,
            None => {
                let content = shareable_content()?;
                windows_by_id(&content, excluded)
            }
        };
        if hidden.is_empty() {
            return Ok(filter);
        }
        let displays = filter.includedDisplays();
        match filter.style() {
            SCShareableContentStyle::Display if displays.len() == 1 => {
                Ok(SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &displays.objectAtIndex(0),
                    &NSArray::from_retained_slice(&hidden),
                ))
            }
            SCShareableContentStyle::Application => {
                let apps = filter.includedApplications();
                if apps.is_empty() {
                    return Err(Unsupported::HideWindows.into());
                }
                // An exception owned by an unselected app would add that window to the share.
                let mut exceptions = Vec::new();
                for window in hidden {
                    let owner = window.owningApplication().ok_or(Unsupported::HideWindows)?;
                    if apps.iter().any(|app| app.processID() == owner.processID()) {
                        exceptions.push(window);
                    }
                }
                if exceptions.is_empty() {
                    return Ok(filter);
                }
                if displays.len() != 1 {
                    return Err(Unsupported::HideWindows.into());
                }
                Ok(
                    SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                        SCContentFilter::alloc(),
                        &displays.objectAtIndex(0),
                        &apps,
                        &NSArray::from_retained_slice(&exceptions),
                    ),
                )
            }
            _ => Err(Unsupported::HideWindows.into()),
        }
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

/// Round and validate scaled dimensions, rejecting zero, negative, non-finite, or `> u32::MAX`.
fn positive_scaled_size(width: f64, height: f64, scale: f64) -> Option<(usize, usize)> {
    if !scale.is_finite()
        || scale <= 0.0
        || !width.is_finite()
        || width <= 0.0
        || !height.is_finite()
        || height <= 0.0
    {
        return None;
    }
    let w = (width * scale).round();
    let h = (height * scale).round();
    let range = 1.0..=u32::MAX as f64;
    if !w.is_finite() || !h.is_finite() || !range.contains(&w) || !range.contains(&h) {
        return None;
    }
    Some((w as usize, h as usize))
}

/// ScreenCaptureKit reports geometry in points, `SCStreamConfiguration` wants pixels.
///
/// Returns `Ok(None)` on a system whose `SCContentFilter` predates these two selectors, and
/// `Err` if those selectors exist but report a non-positive, non-finite, or out-of-range size.
unsafe fn filter_size(filter: &SCContentFilter) -> Result<Option<(usize, usize)>> {
    unsafe {
        if !filter.respondsToSelector(sel!(contentRect))
            || !filter.respondsToSelector(sel!(pointPixelScale))
        {
            return Ok(None);
        }
        let rect = filter.contentRect();
        let scale = filter.pointPixelScale() as f64;
        let size = positive_scaled_size(rect.size.width, rect.size.height, scale)
            .ok_or(ScreenCaptureKitError::FailedToStart)?;
        Ok(Some(size))
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// The `CGDirectDisplayID` of the display with the menu bar, the one `SCDisplay::displayID`
    /// matches for `Target::Primary`.
    safe fn CGMainDisplayID() -> u32;
}

/// Sort displays deterministically: primary first (`main_id`, falling back to origin `(0, 0)`),
/// then by frame `(origin.x, origin.y)`, then by `displayID`.
fn sort_displays_by_key<T>(
    displays: &mut [T],
    main_id: u32,
    mut key: impl FnMut(&T) -> (u32, f64, f64),
) {
    let has_main = displays.iter().any(|d| key(d).0 == main_id);
    let norm = |v: f64| if v == 0.0 { 0.0 } else { v };
    displays.sort_by(|a, b| {
        let (a_id, a_x, a_y) = key(a);
        let (b_id, b_x, b_y) = key(b);
        let a_primary = if has_main {
            a_id == main_id
        } else {
            a_x == 0.0 && a_y == 0.0
        };
        let b_primary = if has_main {
            b_id == main_id
        } else {
            b_x == 0.0 && b_y == 0.0
        };
        (!a_primary)
            .cmp(&(!b_primary))
            .then_with(|| norm(a_x).total_cmp(&norm(b_x)))
            .then_with(|| norm(a_y).total_cmp(&norm(b_y)))
            .then_with(|| a_id.cmp(&b_id))
    });
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
        let from_display = |display: &SCDisplay| -> Option<_> {
            let width = u32::try_from(display.width()).ok().filter(|&w| w > 0)? as usize;
            let height = u32::try_from(display.height()).ok().filter(|&h| h > 0)? as usize;
            Some((
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    display,
                    &NSArray::from_retained_slice(&excluded_windows),
                ),
                (width, height),
            ))
        };
        let from_window = |window: &SCWindow| -> Option<_> {
            let frame = window.frame();
            let points = positive_scaled_size(frame.size.width, frame.size.height, 1.0)?;
            Some((
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window),
                points,
            ))
        };
        let filter = match target {
            // `displays` has no documented order, so the first one is not necessarily main.
            Target::Primary => shareable
                .displays()
                .iter()
                .find(|display| display.displayID() == CGMainDisplayID())
                .and_then(|display| from_display(&display)),
            Target::Monitor(index) => usize::try_from(*index)
                .ok()
                .and_then(|index| {
                    let mut displays: Vec<_> = shareable.displays().iter().collect();
                    sort_displays_by_key(&mut displays, CGMainDisplayID(), |d| {
                        let origin = d.frame().origin;
                        (d.displayID(), origin.x, origin.y)
                    });
                    displays.into_iter().nth(index)
                })
                .and_then(|display| from_display(&display)),
            Target::Window(id) => u32::try_from(*id)
                .ok()
                .filter(|&id| id > 0 && !excluded.contains(&id))
                .and_then(|id| shareable.windows().iter().find(|w| w.windowID() == id))
                .and_then(|window| from_window(&window)),
            Target::WindowName(_) => re.and_then(|re| {
                shareable
                    .windows()
                    .iter()
                    .find(|w| {
                        !excluded.contains(&w.windowID())
                            && w.isOnScreen()
                            && w.title().is_some_and(|t| re.is_match(&t.to_string()))
                    })
                    .and_then(|window| from_window(&window))
            }),
            Target::Pick => return Err(Unsupported::Picker.into()),
        };
        let (filter, points) = filter.ok_or(CaptureError::TargetNotFound)?;
        let (width, height) = match filter_size(&filter)? {
            Some(pixels) => pixels,
            None => points,
        };
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
    jh: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct CaptureVideoDesc {
    size: Arc<AtomicSize>,
}

impl CaptureVideoDesc {
    fn size(&self) -> (u32, u32) {
        self.size.load()
    }
}

#[derive(Debug)]
struct CaptureAudioDesc {
    sample_rate: Arc<AtomicI32>,
}

impl CaptureAudioDesc {
    /// `None` until the first audio buffer settles the format: `0` is the unset state of the
    /// atomic, not a rate, and a caller is meant to tell the two apart.
    fn sample_rate(&self) -> Option<i32> {
        let rate = self.sample_rate.load(Ordering::Relaxed);
        (rate > 0).then_some(rate)
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

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::NSString;
    use objc2_screen_capture_kit::SCContentSharingPickerMode;

    #[test]
    fn adding_exclusions_preserves_the_host_picker_configuration() {
        if AnyClass::get(c"SCContentSharingPickerConfiguration").is_none() {
            return;
        }
        unsafe {
            let host = SCContentSharingPickerConfiguration::new();
            host.setAllowedPickerModes(SCContentSharingPickerMode::SingleWindow);
            host.setAllowsChangingSelectedContent(false);
            host.setExcludedBundleIDs(&NSArray::from_retained_slice(&[NSString::from_str(
                "org.example.Private",
            )]));
            host.setExcludedWindowIDs(&NSArray::from_retained_slice(&[NSNumber::new_u32(11)]));

            let merged = with_excluded_windows(&host, &[22]);
            assert_eq!(merged.allowedPickerModes(), host.allowedPickerModes());
            assert!(!merged.allowsChangingSelectedContent());
            assert_eq!(merged.excludedBundleIDs(), host.excludedBundleIDs());
            let ids = |c: &SCContentSharingPickerConfiguration| {
                c.excludedWindowIDs()
                    .iter()
                    .map(|number| number.as_u32())
                    .collect::<Vec<_>>()
            };
            assert_eq!(ids(&merged), [11, 22]);
            assert_eq!(ids(&host), [11]);
        }
    }

    #[test]
    fn parse_hide_ids_validates_window_ids() {
        assert_eq!(parse_hide_ids(&[]).unwrap(), Vec::<u32>::new());
        assert_eq!(
            parse_hide_ids(&[1, 42, u32::MAX as isize]).unwrap(),
            [1, 42, u32::MAX]
        );
        assert!(matches!(
            parse_hide_ids(&[0]),
            Err(CaptureError::TargetNotFound)
        ));
        assert!(matches!(
            parse_hide_ids(&[-1]),
            Err(CaptureError::TargetNotFound)
        ));
        assert!(matches!(
            parse_hide_ids(&[1, u32::MAX as isize + 1]),
            Err(CaptureError::TargetNotFound)
        ));
    }

    #[test]
    fn positive_scaled_size_rounds_and_validates() {
        assert_eq!(positive_scaled_size(100.4, 50.6, 1.0), Some((100, 51)));
        assert_eq!(positive_scaled_size(100.3, 50.3, 2.0), Some((201, 101)));
        assert_eq!(positive_scaled_size(0.0, 100.0, 2.0), None);
        assert_eq!(positive_scaled_size(100.0, -1.0, 2.0), None);
        assert_eq!(positive_scaled_size(100.0, 100.0, 0.0), None);
        assert_eq!(positive_scaled_size(100.0, 100.0, -2.0), None);
        assert_eq!(positive_scaled_size(f64::NAN, 100.0, 1.0), None);
        assert_eq!(positive_scaled_size(f64::INFINITY, 100.0, 1.0), None);
        assert_eq!(positive_scaled_size(100.0, 100.0, f64::INFINITY), None);
        assert_eq!(
            positive_scaled_size(u32::MAX as f64, 1.0, 1.0),
            Some((u32::MAX as usize, 1))
        );
        assert_eq!(positive_scaled_size(u32::MAX as f64 + 1.0, 1.0, 1.0), None);
    }

    #[test]
    fn sort_displays_puts_primary_first_then_origin_then_id() {
        let mut displays = vec![
            (30u32, 1920.0, 0.0),
            (10u32, -1920.0, 0.0),
            (20u32, 0.0, 0.0),
            (15u32, 1920.0, 0.0),
            (25u32, 1920.0, -1080.0),
        ];
        sort_displays_by_key(&mut displays, 20, |&d| d);
        let ids: Vec<u32> = displays.iter().map(|d| d.0).collect();
        assert_eq!(ids, [20, 10, 25, 15, 30]);

        // Fallback to origin (0, 0) if main_id is absent.
        let mut fallback = vec![
            (30u32, 1920.0, 0.0),
            (10u32, -1920.0, 0.0),
            (20u32, 0.0, 0.0),
        ];
        sort_displays_by_key(&mut fallback, 999, |&d| d);
        let ids: Vec<u32> = fallback.iter().map(|d| d.0).collect();
        assert_eq!(ids, [20, 10, 30]);
    }
}
