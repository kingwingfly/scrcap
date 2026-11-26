use std::{
    ffi::c_void,
    fmt,
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
    AnyThread as _, ClassType as _, MainThreadMarker, msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, ProtocolObject},
    sel,
};
use objc2_core_media::{CMTime, kCMTimeZero};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSObjectProtocol as _, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCContentSharingPicker, SCContentSharingPickerConfiguration, SCDisplay,
    SCRunningApplication, SCShareableContent, SCShareableContentStyle, SCStream,
    SCStreamConfiguration, SCStreamOutputType, SCWindow,
};
use parking_lot::{Mutex, MutexGuard};
use regex::Regex;
use tracing::error;

/// An exclusively-owned `SCStream` and its unretained delegate.
struct BuiltStream {
    // Field order is the drop order: drop `stream` before `_stream_delegate`, which
    // ScreenCaptureKit holds unretained.
    stream: Retained<SCStream>,
    _stream_delegate: Retained<StreamDelegate>,
    width: u32,
    height: u32,
}

impl fmt::Debug for BuiltStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltStream")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

// SAFETY: `SCStream` and `StreamDelegate` are created and exclusively owned by us (never shared
// with host picker observers). Unique ownership is handed sequentially across threads (picker
// callback -> worker -> main-queue picker teardown); no two threads access the stream
// concurrently, and `StreamDelegate`'s ivars (`Unparker`, `Arc<AtomicBool>`, `Sender`) are all
// `Send + Sync`.
unsafe impl Send for BuiltStream {}

struct RunningStream {
    // Field order is the drop order: drop the output before the stream, and the stream
    // before the delegate, which ScreenCaptureKit holds unretained.
    output_delegate: Retained<VideoStreamOutput>,
    built: BuiltStream,
    has_audio: bool,
    picker_configured: bool,
}

/// Clears a per-stream picker configuration on the main queue before `SCStream` drops.
///
/// While `async_slot` is `None` (during `Target::Pick` setup before `create` returns), `Drop`
/// clears synchronously via `exec_sync`. Once armed with `Some(slot)` for the running stream,
/// `Drop` enqueues async cleanup on the main queue so `CaptureDesc::drop` running on the main
/// thread can never deadlock joining the worker thread.
struct PickerStreamGuard {
    built: Option<BuiltStream>,
    configured: bool,
    async_slot: Option<Arc<Mutex<Option<BuiltStream>>>>,
}

impl Drop for PickerStreamGuard {
    fn drop(&mut self) {
        let Some(built) = self.built.take() else {
            return;
        };
        if !self.configured {
            drop(built);
            return;
        }
        if let Some(slot) = self.async_slot.take() {
            *slot.lock() = Some(built);
            let slot_for_main = slot.clone();
            DispatchQueue::main().exec_async(move || clear_picker_stream_slot(&slot_for_main));
        } else {
            // Only runs during `Target::Pick` setup, which must not be called from the main
            // thread, so clearing synchronously before `create` returns cannot deadlock.
            let slot = Mutex::new(Some(built));
            DispatchQueue::main().exec_sync(|| clear_picker_stream_slot(&slot));
        }
    }
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
        let picker_teardown = Arc::new(Mutex::new(None::<BuiltStream>));

        let (setup_tx, setup_rx) = bounded::<Result<()>>(1);

        let jh = thread::Builder::new().name("scrcap-macos".into()).spawn({
            let size = size.clone();
            let sample_rate = audio_desc.as_ref().map(|desc| desc.sample_rate.clone());
            let target = self.video.target.clone();
            let picker_teardown = picker_teardown.clone();
            move || unsafe {
                let stopped = Arc::new(AtomicBool::new(false));
                // The delegate answers this too, so the wait below never needs a timeout.
                let (stop_tx, stop_rx) = bounded::<Option<CaptureError>>(1);
                let run = || -> Result<RunningStream> {
                    let has_audio = self.audio.is_some();
                    let mut built = if target == Target::Pick {
                        present_picker(
                            &excluded,
                            has_audio,
                            self.video.fps,
                            parker.unparker().clone(),
                            stopped.clone(),
                            stop_tx.clone(),
                        )?
                    } else {
                        let content = shareable_content()?;
                        let (filter, width, height) =
                            content_filter(&content, &target, re.as_ref(), &excluded)?;
                        build_stream_from_filter(
                            &filter,
                            (width, height),
                            has_audio,
                            self.video.fps,
                            parker.unparker().clone(),
                            stopped.clone(),
                            stop_tx.clone(),
                        )?
                    };

                    let mut picker_configured = false;
                    if target == Target::Pick && !excluded.is_empty() {
                        let built_ref = &mut built;
                        DispatchQueue::main().exec_sync(|| {
                            // Force capture of the whole `&mut BuiltStream` (which is `Send`)
                            // rather than disjointly borrowing `&Retained<SCStream>`.
                            let built_ref = built_ref;
                            picker_configured =
                                configure_picker_for_stream(&built_ref.stream, &excluded);
                        });
                    }
                    // Declared before `output_delegate` so early returns and unwinds drop
                    // `output_delegate` first, then clear the per-stream picker configuration
                    // before dropping `stream` and `stream_delegate`.
                    let mut stream_guard = PickerStreamGuard {
                        built: Some(built),
                        configured: picker_configured,
                        async_slot: None,
                    };
                    let built = stream_guard.built.as_ref().expect("stream still armed");

                    // Before the output exists, so a real frame's size always lands after it.
                    size.store(built.width, built.height);
                    let output_delegate =
                        VideoStreamOutput::new(v_tx, a_tx, size, sample_rate, self.video.fps);
                    let video_queue =
                        DispatchQueue::new("dev.scrcap.video", DispatchQueueAttr::SERIAL);
                    built
                        .stream
                        .addStreamOutput_type_sampleHandlerQueue_error(
                            ProtocolObject::from_ref(&*output_delegate),
                            SCStreamOutputType::Screen,
                            Some(&video_queue),
                        )
                        .map_err(|_| ScreenCaptureKitError::OutputAttach)?;
                    if has_audio {
                        let queue =
                            DispatchQueue::new("dev.scrcap.audio", DispatchQueueAttr::SERIAL);
                        built
                            .stream
                            .addStreamOutput_type_sampleHandlerQueue_error(
                                ProtocolObject::from_ref(&*output_delegate),
                                SCStreamOutputType::Audio,
                                Some(&queue),
                            )
                            .map_err(|_| ScreenCaptureKitError::OutputAttach)?;
                    }

                    let (start_tx, start_rx) = bounded(1);
                    built
                        .stream
                        .startCaptureWithCompletionHandler(Some(&RcBlock::new(
                            move |e: *mut NSError| {
                                let _ = start_tx.try_send((!e.is_null()).then(|| ns_error(&*e)));
                            },
                        )));
                    match start_rx.recv() {
                        Ok(Some(e)) => return Err(e),
                        Ok(None) => {}
                        Err(_) => return Err(CaptureError::WorkerGone),
                    }
                    let built = stream_guard.built.take().expect("stream still armed");
                    let picker_configured = stream_guard.configured;
                    Ok(RunningStream {
                        output_delegate,
                        built,
                        has_audio,
                        picker_configured,
                    })
                };

                // `stream_guard` is bound first so `output_delegate` drops before `stream`,
                // `stream` drops before `_stream_delegate`, and any per-stream picker config
                // is cleared on the main queue even on unwind.
                let (stream_guard, output_delegate, has_audio) = match run() {
                    Ok(RunningStream {
                        output_delegate,
                        built,
                        has_audio,
                        picker_configured,
                    }) => {
                        let _ = setup_tx.send(Ok(()));
                        (
                            PickerStreamGuard {
                                built: Some(built),
                                configured: picker_configured,
                                async_slot: Some(picker_teardown),
                            },
                            output_delegate,
                            has_audio,
                        )
                    }
                    Err(e) => {
                        let _ = setup_tx.send(Err(e));
                        return;
                    }
                };
                let built = stream_guard.built.as_ref().expect("stream still armed");

                parker.park();
                {
                    let output = ProtocolObject::from_ref(&*output_delegate);
                    let _ = built
                        .stream
                        .removeStreamOutput_type_error(output, SCStreamOutputType::Screen);
                    if has_audio {
                        let _ = built
                            .stream
                            .removeStreamOutput_type_error(output, SCStreamOutputType::Audio);
                    }
                }
                // These own the senders; a consumer sees the end only once they drop.
                drop(output_delegate);
                // Asking an already-stopped stream to stop can leave the completion handler
                // never firing, and it can stop itself right after this check -- which is why
                // the delegate answers `stop_rx` too.
                if !stopped.load(Ordering::Acquire) {
                    let stop_tx = stop_tx.clone();
                    built
                        .stream
                        .stopCaptureWithCompletionHandler(Some(&RcBlock::new(
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
                drop(stream_guard);
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
            picker_teardown,
        })
    }
}

unsafe fn build_stream_from_filter(
    filter: &SCContentFilter,
    size_px: (usize, usize),
    has_audio: bool,
    fps: Option<u32>,
    unparker: Unparker,
    stopped: Arc<AtomicBool>,
    stop_tx: Sender<Option<CaptureError>>,
) -> Result<BuiltStream> {
    unsafe {
        let width = u32::try_from(size_px.0)
            .ok()
            .filter(|&w| w > 0)
            .ok_or(ScreenCaptureKitError::FailedToStart)?;
        let height = u32::try_from(size_px.1)
            .ok()
            .filter(|&h| h > 0)
            .ok_or(ScreenCaptureKitError::FailedToStart)?;

        // Deliberately no preset: the HDR ones change the dynamic range and colour
        // space, which 8-bit BGRA cannot carry, and only exist on macOS 15.
        let stream_config = SCStreamConfiguration::new();
        stream_config.setWidth(size_px.0);
        stream_config.setHeight(size_px.1);
        // macOS 13+; on 12.3 the selector is missing and sending it aborts.
        if stream_config.respondsToSelector(sel!(setCapturesAudio:)) {
            stream_config.setCapturesAudio(has_audio);
        } else if has_audio {
            return Err(Unsupported::Audio.into());
        }
        if stream_config.respondsToSelector(sel!(setCaptureMicrophone:)) {
            stream_config.setCaptureMicrophone(false);
        }
        stream_config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));
        let interval = match fps {
            None | Some(0) => kCMTimeZero,
            Some(fps) => CMTime::new(1, i32::try_from(fps).unwrap_or(i32::MAX)),
        };
        stream_config.setMinimumFrameInterval(interval);

        // Declared before `stream` so early returns and unwinds drop `stream` first.
        let stream_delegate = StreamDelegate::new(unparker, stopped, stop_tx);
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            filter,
            &stream_config,
            Some(ProtocolObject::from_ref(&*stream_delegate)),
        );
        Ok(BuiltStream {
            stream,
            _stream_delegate: stream_delegate,
            width,
            height,
        })
    }
}

/// Clear any per-stream picker configuration stored in `slot` and drop the `SCStream` on the
/// main queue.
fn clear_picker_stream_slot(slot: &Mutex<Option<BuiltStream>>) {
    let Some(built) = slot.lock().take() else {
        return;
    };
    unsafe {
        if AnyClass::get(c"SCContentSharingPicker").is_some() {
            let picker = SCContentSharingPicker::sharedPicker();
            if picker.respondsToSelector(sel!(setConfiguration:forStream:)) {
                picker.setConfiguration_forStream(None, &built.stream);
            }
        }
    }
    drop(built);
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

/// Snapshot of the picker's `defaultConfiguration` captured before [`PickerGuard`] overrides it.
struct SavedPickerConfig {
    previous: Retained<SCContentSharingPickerConfiguration>,
    /// Unshared copy of the configuration we installed, used to detect in-place mutations and
    /// compare by value if `defaultConfiguration()` copies on read.
    expected: Retained<SCContentSharingPickerConfiguration>,
    /// The picker's stored configuration when `defaultConfiguration()` returns a stable object
    /// identity, so replacing it with a distinct object is detected even if known fields match.
    installed: Option<Retained<SCContentSharingPickerConfiguration>>,
}

/// Main-thread state owned by [`PickerGuard`] while `SCContentSharingPicker` is presented.
struct PickerState {
    observer: Option<Retained<PickerObserver>>,
    saved_config: Option<SavedPickerConfig>,
    deactivate: bool,
}

// SAFETY: `PickerState` is only ever constructed, accessed, and dropped on the main queue
// via `DispatchQueue::main().exec_sync` (when unarmed, all ObjC fields are `None`). While
// stored inside `PickerGuard` on the worker thread, it is never dereferenced or mutated.
unsafe impl Send for PickerState {}

impl PickerState {
    fn is_armed(&self) -> bool {
        self.observer.is_some() || self.saved_config.is_some() || self.deactivate
    }

    unsafe fn restore(mut self) {
        unsafe {
            let picker = SCContentSharingPicker::sharedPicker();
            if let Some(observer) = self.observer.take() {
                picker.removeObserver(ProtocolObject::from_ref(&*observer));
                drop(observer);
            }
            if let Some(saved) = self.saved_config.take() {
                let current = picker.defaultConfiguration();
                if should_restore_picker_config(&current, &saved) {
                    picker.setDefaultConfiguration(&saved.previous);
                }
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

/// Immutable ScreenCaptureKit targets snapshotted synchronously from a picker-owned
/// `SCContentFilter` so exclusions can be rebuilt off-main without retaining or sending the
/// interior-mutable `SCContentFilter` across threads.
struct PickedExclusionBase {
    style: SCShareableContentStyle,
    displays: Vec<Retained<SCDisplay>>,
    apps: Vec<Retained<SCRunningApplication>>,
}

// SAFETY: `SCDisplay` and `SCRunningApplication` are immutable ScreenCaptureKit snapshot value
// objects (exposing only read-only getters) with atomic ObjC refcounting.
unsafe impl Send for PickedExclusionBase {}

struct PickedSelection {
    /// Unstarted stream built synchronously from the picker-owned `SCContentFilter` inside the
    /// observer callback, used directly when no exclusions need to be rebuilt.
    stream: BuiltStream,
    /// Present when `!excluded.is_empty()` and the pick is not a single window, so exclusions
    /// can be resolved asynchronously on the worker thread without blocking the main queue.
    exclusion_base: Option<PickedExclusionBase>,
}

unsafe fn present_picker(
    excluded: &[u32],
    has_audio: bool,
    fps: Option<u32>,
    unparker: Unparker,
    stopped: Arc<AtomicBool>,
    stop_tx: Sender<Option<CaptureError>>,
) -> Result<BuiltStream> {
    // New-stream selections have no stream ID. Only one of our observers can own one.
    static PRESENTING: Mutex<()> = Mutex::new(());
    unsafe {
        if AnyClass::get(c"SCContentSharingPicker").is_none() {
            return Err(Unsupported::Picker.into());
        }
        let (tx, rx) = bounded::<Result<PickedSelection>>(1);
        let excluded_for_pick = excluded.to_vec();
        let unparker_for_pick = unparker.clone();
        let stopped_for_pick = stopped.clone();
        let stop_tx_for_pick = stop_tx.clone();
        let mut guard = PickerGuard {
            state: Some(PickerState {
                observer: None,
                saved_config: None,
                deactivate: false,
            }),
            _presenting: PRESENTING.lock(),
        };
        let state_slot = &mut guard.state;
        // `SCContentSharingPicker` drives system UI on the main queue; `present()` returns
        // asynchronously, so we wait on `rx` back on the worker thread. The observer consumes
        // `&SCContentFilter` synchronously in place rather than sending it across threads.
        DispatchQueue::main().exec_sync(|| {
            let state = state_slot.as_mut().expect("picker state initialized");
            let picker = SCContentSharingPicker::sharedPicker();
            // The host may already restrict this shared picker. Layer our exclusions onto its
            // configuration only while we present it, arming restoration before each mutation.
            if !excluded.is_empty() {
                let previous = picker.defaultConfiguration();
                let updated = with_excluded_windows(&previous, excluded);
                let expected: Retained<SCContentSharingPickerConfiguration> =
                    msg_send![&*updated, copy];
                state.saved_config = Some(SavedPickerConfig {
                    previous,
                    expected,
                    installed: None,
                });
                picker.setDefaultConfiguration(&updated);
                let installed_a = picker.defaultConfiguration();
                let installed_b = picker.defaultConfiguration();
                if Retained::as_ptr(&installed_a) == Retained::as_ptr(&installed_b)
                    && let Some(saved) = state.saved_config.as_mut()
                {
                    saved.installed = Some(installed_a);
                }
            }
            let observer = state.observer.insert(PickerObserver::new(move |picked| {
                let outcome = picked.and_then(|filter| {
                    let exclusion_base = if !excluded_for_pick.is_empty()
                        && filter.style() != SCShareableContentStyle::Window
                    {
                        // `includedDisplays` / `includedApplications` are macOS 15.2+, the picker
                        // only 14.0+: sending them to an older filter raises an uncaught
                        // unrecognized-selector exception.
                        if !filter.respondsToSelector(sel!(includedDisplays))
                            || !filter.respondsToSelector(sel!(includedApplications))
                        {
                            return Err(Unsupported::HideWindows.into());
                        }
                        let style = filter.style();
                        let displays: Vec<_> = filter.includedDisplays().iter().collect();
                        let apps: Vec<_> = if style == SCShareableContentStyle::Application {
                            filter.includedApplications().iter().collect()
                        } else {
                            Vec::new()
                        };
                        Some(PickedExclusionBase {
                            style,
                            displays,
                            apps,
                        })
                    } else {
                        None
                    };
                    let size_px = filter_size(filter)?.ok_or(Unsupported::Picker)?;
                    let stream = build_stream_from_filter(
                        filter,
                        size_px,
                        has_audio,
                        fps,
                        unparker_for_pick,
                        stopped_for_pick,
                        stop_tx_for_pick,
                    )?;
                    Ok(PickedSelection {
                        stream,
                        exclusion_base,
                    })
                });
                let _ = tx.try_send(outcome);
            }));
            picker.addObserver(ProtocolObject::from_ref(&**observer));
            if !picker.isActive() {
                state.deactivate = true;
                picker.setActive(true);
            }
            picker.present();
        });
        let selection = rx.recv().map_err(|_| CaptureError::WorkerGone);
        drop(guard);

        let PickedSelection {
            stream,
            exclusion_base,
        } = selection??;
        match exclusion_base {
            None => Ok(stream),
            Some(base) => rebuild_stream_with_exclusions(
                stream, base, excluded, has_audio, fps, unparker, stopped, stop_tx,
            ),
        }
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

/// Build a per-stream picker configuration that carries `excluded` and disables live content
/// switching so Control Center cannot redirect the stream to a hidden window.
unsafe fn locked_stream_picker_config(
    base: &SCContentSharingPickerConfiguration,
    excluded: &[u32],
) -> Retained<SCContentSharingPickerConfiguration> {
    unsafe {
        let config = with_excluded_windows(base, excluded);
        if config.respondsToSelector(sel!(setAllowsChangingSelectedContent:)) {
            config.setAllowsChangingSelectedContent(false);
        }
        config
    }
}

/// Install a per-stream picker configuration on the shared picker (must run on the main queue).
unsafe fn configure_picker_for_stream(stream: &SCStream, excluded: &[u32]) -> bool {
    unsafe {
        if AnyClass::get(c"SCContentSharingPicker").is_none() {
            return false;
        }
        let picker = SCContentSharingPicker::sharedPicker();
        if !picker.respondsToSelector(sel!(setConfiguration:forStream:)) {
            return false;
        }
        let base = picker.defaultConfiguration();
        let config = locked_stream_picker_config(&base, excluded);
        picker.setConfiguration_forStream(Some(&config), stream);
        true
    }
}

unsafe fn picker_config_eq(
    a: &SCContentSharingPickerConfiguration,
    b: &SCContentSharingPickerConfiguration,
) -> bool {
    unsafe {
        a.allowedPickerModes() == b.allowedPickerModes()
            && a.allowsChangingSelectedContent() == b.allowsChangingSelectedContent()
            && a.excludedWindowIDs() == b.excludedWindowIDs()
            && a.excludedBundleIDs() == b.excludedBundleIDs()
    }
}

unsafe fn should_restore_picker_config(
    current: &SCContentSharingPickerConfiguration,
    saved: &SavedPickerConfig,
) -> bool {
    unsafe {
        let same_instance = saved.installed.as_ref().is_none_or(|installed| {
            core::ptr::eq(current, &**installed) || current == &**installed
        });
        same_instance && picker_config_eq(current, &saved.expected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobalExclusionNeed {
    /// Missing excluded windows are closed, already covered by current-process content, or
    /// belong to applications outside an `Application` pick.
    NotNeeded,
    /// At least one live window requiring exclusion needs global `SCShareableContent`.
    NeedGlobal,
}

/// Decide whether `missing` excluded window IDs require a global `SCShareableContent` query.
///
/// - `own_content_ok`: whether `getCurrentProcessShareableContent` succeeded (if so, any
///   remaining window owned by `current_pid` is not shareable and needs no global lookup).
/// - `included_pids`: `Some(pids)` for an `Application` pick (only windows owned by one of
///   those PIDs can affect the capture), `None` for a `Display` pick.
/// - `live_owners`: WindowServer lookup result for `missing` (`None` if the query failed).
fn global_exclusion_need(
    missing: &[u32],
    current_pid: i32,
    own_content_ok: bool,
    included_pids: Option<&[i32]>,
    live_owners: Option<&[(u32, Option<i32>)]>,
) -> GlobalExclusionNeed {
    if missing.is_empty() {
        return GlobalExclusionNeed::NotNeeded;
    }
    let Some(live_owners) = live_owners else {
        // WindowServer query failed; cannot prove the missing windows are closed.
        return GlobalExclusionNeed::NeedGlobal;
    };
    let needed = missing.iter().any(|&id| {
        let Some(&(_, owner_pid)) = live_owners.iter().find(|&&(win_id, _)| win_id == id) else {
            // Window no longer exists (closed while the picker was open).
            return false;
        };
        match owner_pid {
            Some(pid) if own_content_ok && pid == current_pid => false,
            Some(pid) => included_pids.is_none_or(|pids| pids.contains(&pid)),
            // Live window with unknown owner: conservatively require global lookup.
            None => true,
        }
    });
    if needed {
        GlobalExclusionNeed::NeedGlobal
    } else {
        GlobalExclusionNeed::NotNeeded
    }
}

const K_CG_WINDOW_LIST_OPTION_ALL: u32 = 0;

/// Look up the owning PID of every currently live window in `ids` via CoreGraphics,
/// which never prompts for Screen Recording permission.
unsafe fn live_window_owners(ids: &[u32]) -> Option<Vec<(u32, Option<i32>)>> {
    struct CfArrayGuard(*mut NSArray<AnyObject>);
    impl Drop for CfArrayGuard {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0.cast()) };
        }
    }
    unsafe {
        let raw = CGWindowListCopyWindowInfo(K_CG_WINDOW_LIST_OPTION_ALL, 0);
        if raw.is_null() {
            return None;
        }
        let _guard = CfArrayGuard(raw);
        let array: &NSArray<AnyObject> = &*raw;
        let num_key: &AnyObject = &*(kCGWindowNumber as *const NSString).cast();
        let pid_key: &AnyObject = &*(kCGWindowOwnerPID as *const NSString).cast();
        let mut live = Vec::new();
        for entry in array.iter() {
            let Some(dict) = entry.downcast_ref::<NSDictionary>() else {
                continue;
            };
            let Some(id) = dict.objectForKey(num_key).and_then(|v| {
                v.downcast_ref::<NSNumber>()
                    .and_then(|n| u32::try_from(n.as_i64()).ok())
            }) else {
                continue;
            };
            if !ids.contains(&id) {
                continue;
            }
            let pid = dict.objectForKey(pid_key).and_then(|v| {
                v.downcast_ref::<NSNumber>()
                    .and_then(|n| i32::try_from(n.as_i64()).ok())
            });
            live.push((id, pid));
        }
        Some(live)
    }
}

/// Resolve `excluded` window IDs to `SCWindow`s without triggering an unsolicited global
/// Screen Recording TCC prompt on a picker-consented capture.
unsafe fn resolve_excluded_windows(
    excluded: &[u32],
    included_pids: Option<&[i32]>,
) -> Result<Vec<Retained<SCWindow>>> {
    unsafe {
        // Try current-process content first so hiding our own windows never demands global
        // Screen Recording TCC when the user only granted one-time picker consent.
        let has_current_process_api: bool = msg_send![
            SCShareableContent::class(),
            respondsToSelector: sel!(getCurrentProcessShareableContentWithCompletionHandler:)
        ];
        let (own_found, own_content_ok) = if has_current_process_api {
            match current_process_shareable_content() {
                Ok(content) => (windows_by_id(&content, excluded), true),
                Err(_) => (Vec::new(), false),
            }
        } else {
            (Vec::new(), false)
        };
        let missing: Vec<u32> = excluded
            .iter()
            .copied()
            .filter(|&id| !own_found.iter().any(|w| w.windowID() == id))
            .collect();
        if missing.is_empty() {
            return Ok(own_found);
        }
        let live_owners = live_window_owners(&missing);
        let current_pid = i32::try_from(std::process::id()).unwrap_or(-1);
        match global_exclusion_need(
            &missing,
            current_pid,
            own_content_ok,
            included_pids,
            live_owners.as_deref(),
        ) {
            GlobalExclusionNeed::NotNeeded => Ok(own_found),
            GlobalExclusionNeed::NeedGlobal => {
                if !CGPreflightScreenCaptureAccess() {
                    return Err(Unsupported::HideWindows.into());
                }
                let content = shareable_content()?;
                Ok(windows_by_id(&content, excluded))
            }
        }
    }
}

/// Carry `hide` into the picked display or application stream.
///
/// `excludedWindowIDs` only keeps a window out of the *picker*; the filter it returns for a
/// display or application can still contain it. Rebuild those streams with exclusions.
#[allow(clippy::too_many_arguments)]
unsafe fn rebuild_stream_with_exclusions(
    stream: BuiltStream,
    base: PickedExclusionBase,
    excluded: &[u32],
    has_audio: bool,
    fps: Option<u32>,
    unparker: Unparker,
    stopped: Arc<AtomicBool>,
    stop_tx: Sender<Option<CaptureError>>,
) -> Result<BuiltStream> {
    unsafe {
        let included_pids: Option<Vec<i32>> = (base.style == SCShareableContentStyle::Application
            && !base.apps.is_empty())
        .then(|| base.apps.iter().map(|app| app.processID()).collect());
        let hidden = resolve_excluded_windows(excluded, included_pids.as_deref())?;
        if hidden.is_empty() {
            return Ok(stream);
        }
        let new_filter = match base.style {
            SCShareableContentStyle::Display if base.displays.len() == 1 => {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &base.displays[0],
                    &NSArray::from_retained_slice(&hidden),
                )
            }
            SCShareableContentStyle::Application => {
                if base.apps.is_empty() {
                    return Err(Unsupported::HideWindows.into());
                }
                // An exception owned by an unselected app would add that window to the share.
                let mut exceptions = Vec::new();
                for window in hidden {
                    let owner = window.owningApplication().ok_or(Unsupported::HideWindows)?;
                    if base
                        .apps
                        .iter()
                        .any(|app| app.processID() == owner.processID())
                    {
                        exceptions.push(window);
                    }
                }
                if exceptions.is_empty() {
                    return Ok(stream);
                }
                if base.displays.len() != 1 {
                    return Err(Unsupported::HideWindows.into());
                }
                SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                    SCContentFilter::alloc(),
                    &base.displays[0],
                    &NSArray::from_retained_slice(&base.apps),
                    &NSArray::from_retained_slice(&exceptions),
                )
            }
            _ => return Err(Unsupported::HideWindows.into()),
        };
        let size_px = filter_size(&new_filter)?.ok_or(Unsupported::Picker)?;
        // Drop the unstarted initial stream before constructing its replacement.
        drop(stream);
        build_stream_from_filter(
            &new_filter,
            size_px,
            has_audio,
            fps,
            unparker,
            stopped,
            stop_tx,
        )
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

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// The `CGDirectDisplayID` of the display with the menu bar, the one `SCDisplay::displayID`
    /// matches for `Target::Primary`.
    safe fn CGMainDisplayID() -> u32;
    /// Whether this process already has global Screen Recording permission (never prompts).
    safe fn CGPreflightScreenCaptureAccess() -> bool;
    /// Snapshot of WindowServer window metadata (`CFArrayRef` of `CFDictionaryRef`).
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> *mut NSArray<AnyObject>;
    static kCGWindowNumber: &'static NSString;
    static kCGWindowOwnerPID: &'static NSString;
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
    picker_teardown: Arc<Mutex<Option<BuiltStream>>>,
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
        // If `Drop` ran on the main thread, drain any pending per-stream picker cleanup now so
        // `setConfiguration:forStream:` is cleared before `Drop` returns.
        if MainThreadMarker::new().is_some() {
            clear_picker_stream_slot(&self.picker_teardown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn locked_stream_picker_config_disables_changing_content_and_keeps_host_settings() {
        if AnyClass::get(c"SCContentSharingPickerConfiguration").is_none() {
            return;
        }
        unsafe {
            let host = SCContentSharingPickerConfiguration::new();
            host.setAllowedPickerModes(SCContentSharingPickerMode::SingleDisplay);
            host.setAllowsChangingSelectedContent(true);
            host.setExcludedBundleIDs(&NSArray::from_retained_slice(&[NSString::from_str(
                "org.example.Private",
            )]));
            host.setExcludedWindowIDs(&NSArray::from_retained_slice(&[NSNumber::new_u32(7)]));

            let locked = locked_stream_picker_config(&host, &[42]);
            assert!(!locked.allowsChangingSelectedContent());
            assert!(host.allowsChangingSelectedContent());
            assert_eq!(locked.allowedPickerModes(), host.allowedPickerModes());
            assert_eq!(locked.excludedBundleIDs(), host.excludedBundleIDs());
            let ids: Vec<u32> = locked
                .excludedWindowIDs()
                .iter()
                .map(|number| number.as_u32())
                .collect();
            assert_eq!(ids, [7, 42]);
        }
    }

    #[test]
    fn picker_restore_skips_host_modifications() {
        if AnyClass::get(c"SCContentSharingPickerConfiguration").is_none() {
            return;
        }
        unsafe {
            let previous = SCContentSharingPickerConfiguration::new();
            previous.setAllowedPickerModes(SCContentSharingPickerMode::SingleDisplay);
            let installed = with_excluded_windows(&previous, &[42]);
            let expected: Retained<SCContentSharingPickerConfiguration> =
                msg_send![&*installed, copy];
            let saved = SavedPickerConfig {
                previous,
                expected,
                installed: Some(installed.clone()),
            };

            // Unchanged instance restores `previous`.
            assert!(should_restore_picker_config(&installed, &saved));

            // Replacing with a distinct config object preserves the host's update.
            let host_replaced = SCContentSharingPickerConfiguration::new();
            host_replaced.setAllowedPickerModes(SCContentSharingPickerMode::SingleWindow);
            assert!(!should_restore_picker_config(&host_replaced, &saved));

            // Mutating the installed instance in place also preserves the host's update.
            installed.setAllowsChangingSelectedContent(false);
            assert!(!should_restore_picker_config(&installed, &saved));
        }
    }

    #[test]
    fn global_exclusion_need_avoids_unsolicited_tcc_for_closed_own_and_unrelated_windows() {
        let me = 100;
        let other = 200;
        let unrelated = 300;

        // Nothing missing -> no global lookup needed.
        assert_eq!(
            global_exclusion_need(&[], me, true, None, Some(&[])),
            GlobalExclusionNeed::NotNeeded
        );
        // Caller window closed while the picker was open -> no global lookup needed.
        assert_eq!(
            global_exclusion_need(&[10], me, true, None, Some(&[])),
            GlobalExclusionNeed::NotNeeded
        );
        // Live own-process window already omitted by a successful current-process query -> no
        // global lookup needed.
        assert_eq!(
            global_exclusion_need(&[10], me, true, None, Some(&[(10, Some(me))])),
            GlobalExclusionNeed::NotNeeded
        );
        // If current-process query failed and our window is still live -> need global lookup.
        assert_eq!(
            global_exclusion_need(&[10], me, false, None, Some(&[(10, Some(me))])),
            GlobalExclusionNeed::NeedGlobal
        );
        // Live window owned by another process on a Display pick -> need global lookup.
        assert_eq!(
            global_exclusion_need(&[20], me, true, None, Some(&[(20, Some(other))])),
            GlobalExclusionNeed::NeedGlobal
        );
        // Application pick: live window owned by an unselected app -> no global lookup needed.
        assert_eq!(
            global_exclusion_need(
                &[30],
                me,
                true,
                Some(&[other]),
                Some(&[(30, Some(unrelated))]),
            ),
            GlobalExclusionNeed::NotNeeded
        );
        // Application pick: live window owned by a selected app -> need global lookup.
        assert_eq!(
            global_exclusion_need(&[20], me, true, Some(&[other]), Some(&[(20, Some(other))])),
            GlobalExclusionNeed::NeedGlobal
        );
        // Failed WindowServer lookup or missing owner PID -> conservatively need global lookup.
        assert_eq!(
            global_exclusion_need(&[20], me, true, None, None),
            GlobalExclusionNeed::NeedGlobal
        );
        assert_eq!(
            global_exclusion_need(&[20], me, true, Some(&[other]), Some(&[(20, None)])),
            GlobalExclusionNeed::NeedGlobal
        );
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
