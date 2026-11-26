//! pipewire related

use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::Rc,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam_channel::Sender;
use tracing::error;

use parking_lot::Mutex;
use pipewire::{
    channel::Sender as PwSender,
    context::ContextRc,
    core::{self as pw_core, PW_ID_CORE},
    keys,
    main_loop::MainLoopRc,
    properties::properties,
    registry,
    spa::{
        buffer::meta::{MetaHeader, MetaHeaderFlags, MetaVideoCrop},
        param::{
            ParamType,
            audio::{AudioFormat, AudioInfoRaw},
            format::{FormatProperties, MediaSubtype, MediaType},
            format_utils::parse_format,
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::{
            Object, Pod, Property, PropertyFlags, Value, object, property, serialize::PodSerializer,
        },
        sys::{
            SPA_CHUNK_FLAG_CORRUPTED, SPA_META_Header, SPA_META_VideoCrop, SPA_PARAM_META_size,
            SPA_PARAM_META_type, SPA_PARAM_Meta, SPA_TYPE_OBJECT_ParamMeta, spa_data,
            spa_meta_header, spa_meta_region,
        },
        utils::{Direction, Fraction, Rectangle, Region, SpaTypes, result::AsyncSeq},
    },
    stream::{Stream, StreamFlags, StreamRc, StreamState},
    types::ObjectType,
};

use crate::{
    config::{AudioConfig, Target, VideoConfig},
    error::{CaptureError, Result, Unsupported},
    format::{PixFmt, SampleFmt},
    fps::FpsGate,
    frame::{AudioFrame, VideoFrame},
    util::{append_rows, pack_rows, pack_rows_bgr0, row_span},
};

use super::dbus::{DbusScreen, SOURCE_TYPE_MONITOR, StreamTarget};

/// From `<spa/buffer/buffer.h>`: chunk data is empty with media-specific neutral data
/// (silence or black). Defined locally because older PipeWire headers (e.g. 0.3.48)
/// predate this constant.
const SPA_CHUNK_FLAG_EMPTY: i32 = 1 << 1;
/// From `<spa/buffer/buffer.h>`: data plane is mapped for reading.
const SPA_DATA_FLAG_READABLE: u32 = 1 << 0;
/// Upper bound on waiting for the PipeWire daemon to answer the initial audio `sync(0)`.
const AUDIO_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

struct Terminate;

struct VideoData {
    tx: Sender<VideoFrame>,
    mainloop: MainLoopRc,
    gate: FpsGate,
    clock: StreamClock,
    format: Option<(Rectangle, PixFmt)>,
    size: Arc<Mutex<(u32, u32)>>,
    /// Last size written to `size`, so unchanged frames avoid taking the lock.
    reported_size: (u32, u32),
    /// Answered from the format callback, so `new` only returns once the size is real, or
    /// from the state callback if the stream fails before that.
    setup_tx: Option<Sender<Result<()>>>,
}

struct AudioData {
    /// Dropped when the stream or its core ends, closing the channel a consumer waits on.
    tx: Rc<RefCell<Option<Sender<AudioFrame>>>>,
    clock: StreamClock,
    format: Option<AudioInfoRaw>,
    sample_rate: Arc<Mutex<Option<i32>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioCoreState {
    Probing,
    Ready,
    Failed,
}

struct AudioCore {
    // Field order is drop order: unlink the core listener before disconnecting the core.
    _listener: pw_core::Listener,
    core: pw_core::CoreRc,
    state: Rc<Cell<AudioCoreState>>,
    tx_slot: Rc<RefCell<Option<Sender<AudioFrame>>>>,
}

pub struct PipewireSession {
    jh: Option<JoinHandle<()>>,
    quit_tx: PwSender<Terminate>,
    pub size: Arc<Mutex<(u32, u32)>>,
    pub sample_rate: Option<Arc<Mutex<Option<i32>>>>,
}

impl fmt::Debug for PipewireSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipewireSession")
            .field("size", &self.size)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl PipewireSession {
    pub fn new(
        v_tx: Sender<VideoFrame>,
        a_tx: Option<Sender<AudioFrame>>,
        video: VideoConfig,
        audio: Option<AudioConfig>,
    ) -> Result<Self> {
        if !video.hide.is_empty() {
            // Neither Wayland nor X11 lets a client opt a window out of a screencast.
            return Err(Unsupported::HideWindows.into());
        }
        let source_types = match video.target {
            Target::Pick => 0, // every available source type
            Target::Primary | Target::Monitor(_) => SOURCE_TYPE_MONITOR,
            Target::Window(_) | Target::WindowName(_) => {
                return Err(Unsupported::WindowTarget.into());
            }
        };

        let fps = video.fps;
        let size = Arc::new(Mutex::new((0, 0)));
        let sample_rate = audio.as_ref().map(|_audio| Arc::new(Mutex::new(None)));
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        let (setup_tx, setup_rx) = crossbeam_channel::bounded::<Result<()>>(1);

        let jh = thread::Builder::new().name("scrcap-linux".into()).spawn({
            let size = size.clone();
            let sample_rate = sample_rate.clone();
            move || {
                let run = || -> Result<()> {
                    let mainloop = MainLoopRc::new(None)?;
                    let context = ContextRc::new(&mainloop, None)?;
                    let audio_core = if a_tx.is_some() {
                        // Desktop audio is not a screencast node, so it needs the ordinary
                        // connection rather than the portal's remote. Verify the server
                        // accepts the connection with a bounded round trip before starting
                        // the portal picker so a sandbox that grants only the screencast
                        // remote reports `Unsupported::Audio` right away instead of showing
                        // the picker a second time on retry.
                        Some(probe_audio_core(&context, &mainloop)?)
                    } else {
                        None
                    };
                    let dbus = DbusScreen::new()?;
                    let session = dbus.start(source_types)?;
                    // The screencast node is only reachable on the remote the portal
                    // granted; under a sandbox the default socket does not have it.
                    let core = context.connect_fd_rc(session.fd, None)?;

                    let _quit_rx = quit_rx.attach(mainloop.loop_(), {
                        let mainloop = mainloop.clone();
                        move |_| mainloop.quit()
                    });

                    let registry = core.get_registry_rc()?;
                    let _listeners = watch_target(&core, &registry, &mainloop, session.target)?;
                    let mut props = properties! {
                        *keys::MEDIA_TYPE => "Video",
                        *keys::MEDIA_CATEGORY => "Capture",
                        *keys::MEDIA_ROLE => "Screen",
                        "node.dont-fallback" => "true",
                        "node.dont-move" => "true",
                    };
                    let node_id = match session.target {
                        StreamTarget::NodeId(id) => Some(id),
                        StreamTarget::Serial(serial) => {
                            props.insert("target.object", serial.to_string());
                            None
                        }
                    };
                    let video_stream = StreamRc::new(core.clone(), "scrcap-video", props)?;

                    let video_data = VideoData {
                        tx: v_tx,
                        mainloop: mainloop.clone(),
                        gate: FpsGate::new(fps),
                        clock: StreamClock::default(),
                        format: None,
                        size,
                        reported_size: (0, 0),
                        setup_tx: Some(setup_tx.clone()),
                    };

                    let _video_listener = video_stream
                        .add_local_listener_with_user_data(video_data)
                        .param_changed(video_param_change_callback)
                        .process(video_process_callback)
                        .state_changed(video_state_change_callback)
                        .register()?;

                    let mut obj = object! {
                        SpaTypes::ObjectParamFormat,
                        ParamType::EnumFormat,
                        property! {
                            FormatProperties::MediaType, Id, MediaType::Video
                        },
                        property! {
                            FormatProperties::MediaSubtype, Id, MediaSubtype::Raw
                        },
                        // BGRx too: wlroots-based portals offer only that for an XRGB8888
                        // output. BGRA stays the default, so a producer with both keeps alpha.
                        property! {
                            FormatProperties::VideoFormat,
                            Choice, Enum, Id,
                            VideoFormat::BGRA,
                            VideoFormat::BGRA,
                            VideoFormat::BGRx
                        }
                    };
                    if let Some(max_framerate) = fps.filter(|fps| *fps > 0) {
                        obj.properties.push(property! {
                            FormatProperties::VideoMaxFramerate,
                            Choice, Range, Fraction,
                            Fraction { num: max_framerate, denom: 1 },
                            Fraction { num: 0, denom: 1 },
                            Fraction { num: max_framerate, denom: 1 }
                        });
                    }
                    let values = serialize_pod(Value::Object(obj))?;
                    let header_meta = meta_param(SPA_META_Header, size_of::<spa_meta_header>())?;
                    let crop_meta = meta_param(SPA_META_VideoCrop, size_of::<spa_meta_region>())?;
                    let mut params = [
                        Pod::from_bytes(&values).ok_or(CaptureError::SpaParams)?,
                        Pod::from_bytes(&header_meta).ok_or(CaptureError::SpaParams)?,
                        Pod::from_bytes(&crop_meta).ok_or(CaptureError::SpaParams)?,
                    ];
                    // The local listeners share mutable format and channel state with their
                    // process callbacks. Run all callbacks on this loop. Copying owned frames
                    // also requires allocations that cannot run on PipeWire's realtime thread.
                    let flags = StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS;
                    video_stream.connect(
                        Direction::Input,
                        node_id,
                        flags | StreamFlags::DONT_RECONNECT,
                        &mut params,
                    )?;

                    let _audio = if let (Some(a_tx), Some(audio_core)) = (a_tx, audio_core) {
                        if audio_core.state.get() != AudioCoreState::Ready {
                            // The audio core errored while the portal picker or target watch
                            // was running; drop `a_tx` so audio closes while video carries on.
                            None
                        } else {
                            *audio_core.tx_slot.borrow_mut() = Some(a_tx);
                            let audio_stream = StreamRc::new(
                                audio_core.core.clone(),
                                "scrcap-audio",
                                properties! {
                                    *keys::MEDIA_TYPE => "Audio",
                                    *keys::MEDIA_CATEGORY => "Capture",
                                    *keys::MEDIA_ROLE => "Music",
                                    *keys::STREAM_CAPTURE_SINK => "true",
                                },
                            )?;
                            let audio_data = AudioData {
                                tx: audio_core.tx_slot.clone(),
                                clock: StreamClock::default(),
                                format: None,
                                sample_rate: sample_rate
                                    .expect("sample_rate exists when audio is on"),
                            };
                            let audio_listener = audio_stream
                                .add_local_listener_with_user_data(audio_data)
                                .param_changed(|_, data, id, param| {
                                    if id == ParamType::Format.as_raw() {
                                        data.set_format(param);
                                    }
                                })
                                .process(audio_process_callback)
                                .state_changed(audio_state_change_callback)
                                .register()?;
                            let obj = object! {
                                SpaTypes::ObjectParamFormat,
                                ParamType::EnumFormat,
                                property! {
                                    FormatProperties::MediaType, Id, MediaType::Audio
                                },
                                property! {
                                    FormatProperties::MediaSubtype, Id, MediaSubtype::Raw
                                },
                                property! {
                                    FormatProperties::AudioFormat, Id, AudioFormat::F32P
                                },
                            };
                            let values = serialize_pod(Value::Object(obj))?;
                            let header_meta =
                                meta_param(SPA_META_Header, size_of::<spa_meta_header>())?;
                            let mut params = [
                                Pod::from_bytes(&values).ok_or(CaptureError::SpaParams)?,
                                Pod::from_bytes(&header_meta).ok_or(CaptureError::SpaParams)?,
                            ];
                            // No target: `node_id` is the portal's *video* node, and an audio
                            // stream aimed at it can never link. `STREAM_CAPTURE_SINK` above is
                            // what tells the session manager to pick the default sink monitor,
                            // and an explicit target would override it.
                            audio_stream.connect(Direction::Input, None, flags, &mut params)?;
                            // Tuple fields drop in order:
                            // 1. `audio_listener` unlinks from `audio_stream`
                            // 2. `audio_stream` destroys the stream and releases its `CoreRc` clone
                            // 3. `audio_core` unlinks its core listener before disconnecting the core
                            Some((audio_listener, audio_stream, audio_core))
                        }
                    } else {
                        None
                    };

                    mainloop.run();
                    // The target or remote can disappear before the first format arrives.
                    let _ = setup_tx.try_send(Err(CaptureError::StreamFailed));
                    Ok(())
                };

                if let Err(e) = run() {
                    let _ = setup_tx.try_send(Err(e));
                }
            }
        })?;

        // Every sender lives on the capture thread, so a session that dies before it
        // negotiates closes the channel instead of reporting a size of (0, 0).
        match setup_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = jh.join();
                return Err(e);
            }
            Err(_) => {
                let _ = jh.join();
                return Err(CaptureError::WorkerGone);
            }
        }

        Ok(PipewireSession {
            jh: Some(jh),
            quit_tx,
            size,
            sample_rate,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        *self.size.lock()
    }

    pub fn sample_rate(&self) -> Option<i32> {
        self.sample_rate.as_ref().and_then(|rate| *rate.lock())
    }

    pub fn terminate(&self) {
        let _ = self.quit_tx.send(Terminate);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetState {
    Waiting(StreamTarget),
    Present(u32),
    Gone,
}

impl TargetState {
    fn observe(self, id: u32, serial: Option<&str>) -> Self {
        match self {
            Self::Waiting(StreamTarget::NodeId(wanted)) if id == wanted => Self::Present(id),
            Self::Waiting(StreamTarget::Serial(wanted))
                if serial.and_then(|s| s.parse::<u64>().ok()) == Some(wanted) =>
            {
                Self::Present(id)
            }
            _ => self,
        }
    }
}

/// Connect to the default PipeWire daemon and complete a bounded `sync(0)` handshake.
///
/// The registered core listener stays alive inside [`AudioCore`] so core errors during
/// subsequent portal setup or streaming continue to be logged and close the audio channel.
fn probe_audio_core(context: &ContextRc, mainloop: &MainLoopRc) -> Result<AudioCore> {
    let core = context.connect_rc(None).map_err(|e| {
        error!("no PipeWire connection for audio: {e}");
        Unsupported::Audio
    })?;
    probe_audio_core_on(core, mainloop, AUDIO_PROBE_TIMEOUT)
}

fn probe_audio_core_on(
    core: pw_core::CoreRc,
    mainloop: &MainLoopRc,
    timeout_duration: Duration,
) -> Result<AudioCore> {
    let state = Rc::new(Cell::new(AudioCoreState::Probing));
    let tx_slot: Rc<RefCell<Option<Sender<AudioFrame>>>> = Rc::new(RefCell::new(None));
    let probe_seq: Rc<Cell<Option<AsyncSeq>>> = Rc::new(Cell::new(None));

    // Attach the listener before `sync(0)` so handshake errors are always observed.
    // Declared after `core`, so on early return it unlinks before `core` disconnects.
    let listener = core
        .add_listener_local()
        .done({
            let mainloop = mainloop.clone();
            let state = state.clone();
            let probe_seq = probe_seq.clone();
            move |id, seq| {
                if state.get() == AudioCoreState::Probing
                    && id == PW_ID_CORE
                    && probe_seq.get() == Some(seq)
                {
                    state.set(AudioCoreState::Ready);
                    mainloop.quit();
                }
            }
        })
        .error({
            let mainloop = mainloop.clone();
            let state = state.clone();
            let tx_slot = tx_slot.clone();
            move |id, seq, res, message| {
                error!("PipeWire audio core error (id={id}, seq={seq}, res={res}): {message}");
                if state.get() == AudioCoreState::Probing {
                    state.set(AudioCoreState::Failed);
                    mainloop.quit();
                } else if id == PW_ID_CORE {
                    state.set(AudioCoreState::Failed);
                    tx_slot.borrow_mut().take();
                }
            }
        })
        .register();

    let seq = core.sync(0).map_err(|e| {
        error!("failed to sync PipeWire audio core: {e}");
        Unsupported::Audio
    })?;
    probe_seq.set(Some(seq));

    if state.get() == AudioCoreState::Probing {
        let timeout = mainloop.loop_().add_timer({
            let mainloop = mainloop.clone();
            let state = state.clone();
            move |_| {
                if state.get() == AudioCoreState::Probing {
                    error!("timed out waiting for PipeWire audio handshake");
                    state.set(AudioCoreState::Failed);
                    mainloop.quit();
                }
            }
        });
        if let Err(e) = timeout
            .update_timer(Some(timeout_duration), None)
            .into_result()
        {
            error!("failed to arm PipeWire audio handshake timer: {e}");
            return Err(Unsupported::Audio.into());
        }
        mainloop.run();
        let _ = timeout.update_timer(None, None);
    }

    match state.get() {
        AudioCoreState::Ready => Ok(AudioCore {
            _listener: listener,
            core,
            state,
            tx_slot,
        }),
        AudioCoreState::Probing | AudioCoreState::Failed => Err(Unsupported::Audio.into()),
    }
}

/// Enumerate the portal remote before connecting, then stop capture if the selected node goes away.
fn watch_target(
    core: &pw_core::Core,
    registry: &registry::Registry,
    mainloop: &MainLoopRc,
    target: StreamTarget,
) -> Result<(registry::Listener, pw_core::Listener)> {
    let node = Rc::new(Cell::new(TargetState::Waiting(target)));
    let globals = registry
        .add_listener_local()
        .global({
            let node = node.clone();
            move |global| {
                if global.type_ == ObjectType::Node {
                    let serial = global.props.and_then(|p| p.get("object.serial"));
                    node.set(node.get().observe(global.id, serial));
                }
            }
        })
        .global_remove({
            let node = node.clone();
            let mainloop = mainloop.clone();
            move |id| {
                if node.get() == TargetState::Present(id) {
                    node.set(TargetState::Gone);
                    mainloop.quit();
                }
            }
        })
        .register();
    let enumeration = core.sync(0)?;
    let connection = core
        .add_listener_local()
        .done({
            let mainloop = mainloop.clone();
            move |id, seq| {
                if id == PW_ID_CORE && seq == enumeration {
                    mainloop.quit();
                }
            }
        })
        .error({
            let node = node.clone();
            let mainloop = mainloop.clone();
            move |id, _, _, message| {
                if id == PW_ID_CORE {
                    error!("PipeWire remote error: {message}");
                    node.set(TargetState::Gone);
                    mainloop.quit();
                }
            }
        })
        .register();
    mainloop.run();
    match node.get() {
        TargetState::Present(_) => Ok((globals, connection)),
        _ => Err(CaptureError::StreamFailed),
    }
}

/// Ask the producer to attach an `SPA_PARAM_Meta` entry (such as `SPA_META_Header` or
/// `SPA_META_VideoCrop`) to every buffer.
fn meta_param(meta_type: u32, size: usize) -> Result<Vec<u8>> {
    let size = i32::try_from(size).map_err(|_| CaptureError::SpaParams)?;
    let obj = Object {
        type_: SPA_TYPE_OBJECT_ParamMeta,
        id: SPA_PARAM_Meta,
        properties: vec![
            Property {
                key: SPA_PARAM_META_type,
                flags: PropertyFlags::empty(),
                value: Value::Id(pipewire::spa::utils::Id(meta_type)),
            },
            Property {
                key: SPA_PARAM_META_size,
                flags: PropertyFlags::empty(),
                value: Value::Int(size),
            },
        ],
    };
    serialize_pod(Value::Object(obj))
}

/// Serialize a stream parameter into the bytes `Pod::from_bytes` reads back.
fn serialize_pod(value: Value) -> Result<Vec<u8>> {
    Ok(
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
            .map_err(|e| {
                error!("failed to serialize a stream parameter: {e}");
                CaptureError::SpaParams
            })?
            .0
            .into_inner(),
    )
}

/// Maximum difference between a buffer's `pts` and `CLOCK_MONOTONIC` to treat
/// the producer's `pts` as already on `CLOCK_MONOTONIC`.
const MONOTONIC_TOLERANCE_NS: u64 = 5_000_000_000;
/// Maximum backward step from a prior `CLOCK_MONOTONIC` fallback still treated as
/// capture latency on `CLOCK_MONOTONIC` rather than a stream-relative `pts` origin.
const MONOTONIC_BACKWARD_LATENCY_NS: u64 = 100_000_000;
/// Maximum forward step accepted without re-checking `CLOCK_MONOTONIC`. Larger steps
/// (for instance, a static-screen pause or a silent clock-domain switch) validate the
/// candidate timestamp against `CLOCK_MONOTONIC` so a rebased stream cannot jump ahead
/// by system uptime.
const MAX_UNCHECKED_STEP_NS: u64 = 500_000_000;
/// Maximum lead ahead of `CLOCK_MONOTONIC` still treated as plausible for a rebased PTS.
const REBASED_MAX_LEAD_NS: u64 = 500_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ClockDomain {
    #[default]
    Unknown,
    Monotonic,
    Rebased {
        base_pts: u64,
        base_mono: u64,
    },
}

/// Normalizes per-stream PipeWire `SPA_META_Header.pts` onto `CLOCK_MONOTONIC`.
///
/// Producers normally stamp `pts` on `CLOCK_MONOTONIC`, but some leave `pts == 0`
/// (or omit the header) and some count stream-relative nanoseconds starting from 0.
/// Rebasing non-monotonic streams onto `CLOCK_MONOTONIC` keeps video and audio on
/// the same clock and avoids a backward jump after an initial `pts == 0` frame.
#[derive(Debug, Default)]
struct StreamClock {
    domain: ClockDomain,
    last: u64,
}

impl StreamClock {
    /// Re-detect the producer's timestamp domain on the next frame while keeping
    /// `last` as a monotonic floor (used on format renegotiation and `DISCONT`).
    fn reset_domain(&mut self) {
        self.domain = ClockDomain::Unknown;
    }

    fn resolve(&mut self, raw_pts: Option<i64>) -> u64 {
        self.resolve_with(raw_pts, monotonic_now)
    }

    fn resolve_unknown(&mut self, pts: u64, mono: u64) -> u64 {
        if mono == 0
            || (mono.abs_diff(pts) <= MONOTONIC_TOLERANCE_NS
                && self.last.saturating_sub(pts) <= MONOTONIC_BACKWARD_LATENCY_NS)
        {
            self.domain = ClockDomain::Monotonic;
            let ts = pts.max(self.last);
            self.last = ts;
            ts
        } else {
            let base_mono = mono.max(self.last);
            self.domain = ClockDomain::Rebased {
                base_pts: pts,
                base_mono,
            };
            self.last = base_mono;
            base_mono
        }
    }

    fn resolve_with(&mut self, raw_pts: Option<i64>, now: impl FnOnce() -> u64) -> u64 {
        let pts = raw_pts
            .and_then(|p| u64::try_from(p).ok())
            .filter(|&p| p != 0);
        match (self.domain, pts) {
            (ClockDomain::Monotonic, Some(pts)) => {
                if pts >= self.last {
                    if pts - self.last <= MAX_UNCHECKED_STEP_NS {
                        self.last = pts;
                        pts
                    } else {
                        let mono = now();
                        if mono == 0 || mono.abs_diff(pts) <= MONOTONIC_TOLERANCE_NS {
                            self.last = pts;
                            pts
                        } else {
                            self.resolve_unknown(pts, mono)
                        }
                    }
                } else if self.last - pts <= MONOTONIC_BACKWARD_LATENCY_NS {
                    self.last
                } else {
                    self.resolve_unknown(pts, now())
                }
            }
            (ClockDomain::Unknown, Some(pts)) => self.resolve_unknown(pts, now()),
            (
                ClockDomain::Rebased {
                    base_pts,
                    base_mono,
                },
                Some(pts),
            ) => {
                if let Some(ts) = pts
                    .checked_sub(base_pts)
                    .and_then(|delta| base_mono.checked_add(delta))
                    .filter(|&ts| ts >= self.last)
                {
                    if ts - self.last <= MAX_UNCHECKED_STEP_NS {
                        self.last = ts;
                        ts
                    } else {
                        let mono = now();
                        if mono == 0
                            || (ts <= mono.saturating_add(REBASED_MAX_LEAD_NS)
                                && mono.saturating_sub(ts) <= MONOTONIC_TOLERANCE_NS)
                        {
                            self.last = ts;
                            ts
                        } else {
                            self.resolve_unknown(pts, mono)
                        }
                    }
                } else {
                    self.resolve_unknown(pts, now())
                }
            }
            (_, None) => {
                let ts = now().max(self.last);
                self.last = ts;
                ts
            }
        }
    }
}

fn monotonic_now() -> u64 {
    unsafe {
        let mut now = core::mem::zeroed::<libc::timespec>();
        if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) != 0 {
            return 0;
        }
        u64::try_from(now.tv_sec)
            .ok()
            .and_then(|s| s.checked_mul(1_000_000_000))
            .and_then(|s| s.checked_add(u64::try_from(now.tv_nsec).ok()?))
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChunkData {
    Bytes(*const u8),
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChunkRegion {
    data: ChunkData,
    avail: usize,
    stride: i32,
}

/// Start, contiguous length and stride of a plane in a dequeued buffer.
///
/// SPA defines `offset` modulo `maxsize` and only guarantees `size` up to the end of the
/// mapped allocation, so a producer may hand over an offset past it. Returns `None` for an
/// unmapped, unreadable, zero-sized or corrupt chunk, and [`ChunkData::Empty`] without
/// touching `plane.data` when `force_empty` (from `MetaHeaderFlags::GAP`) or
/// `SPA_CHUNK_FLAG_EMPTY` is set.
unsafe fn chunk_region(plane: &spa_data, force_empty: bool) -> Option<ChunkRegion> {
    let chunk = unsafe { plane.chunk.as_ref() }?;
    if chunk.flags & SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 {
        return None;
    }
    let maxsize = plane.maxsize as usize;
    if force_empty || chunk.flags & SPA_CHUNK_FLAG_EMPTY != 0 {
        let avail = if maxsize == 0 || chunk.size == 0 {
            0
        } else {
            let offset = chunk.offset as usize % maxsize;
            (maxsize - offset).min(chunk.size as usize)
        };
        return Some(ChunkRegion {
            data: ChunkData::Empty,
            avail,
            stride: chunk.stride,
        });
    }
    if maxsize == 0 || chunk.size == 0 {
        return None;
    }
    let offset = chunk.offset as usize % maxsize;
    let avail = (maxsize - offset).min(chunk.size as usize);
    if avail == 0 {
        return None;
    }
    if plane.data.is_null()
        || plane.flags & SPA_DATA_FLAG_READABLE == 0
        || offset > isize::MAX as usize
    {
        return None;
    }
    Some(ChunkRegion {
        data: ChunkData::Bytes(unsafe { (plane.data as *const u8).add(offset) }),
        avail,
        stride: chunk.stride,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VideoLayout {
    byte_offset: usize,
    pitch: usize,
    row_bytes: usize,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VideoFramePlan {
    Copy {
        base: *const u8,
        layout: VideoLayout,
    },
    Neutral {
        width: u32,
        height: u32,
    },
}

/// Resolve an optional `SPA_META_VideoCrop` region against the negotiated frame size.
///
/// Per `<spa/buffer/meta.h>`, a crop region is absent/invalid only when `width == 0`
/// or `height == 0`, in which case (or when `crop` is `None`) this falls back to the
/// full negotiated frame. A nonempty crop with a negative origin, overflow, or bounds
/// extending outside `frame` returns `None` so the mismatched frame is dropped.
fn resolve_video_crop(frame: Rectangle, crop: Option<Region>) -> Option<(u32, u32, u32, u32)> {
    if frame.width == 0 || frame.height == 0 {
        return None;
    }
    let Some(region) = crop else {
        return Some((0, 0, frame.width, frame.height));
    };
    if region.size.width == 0 || region.size.height == 0 {
        return Some((0, 0, frame.width, frame.height));
    }
    let x = u32::try_from(region.position.x).ok()?;
    let y = u32::try_from(region.position.y).ok()?;
    let right = x.checked_add(region.size.width)?;
    let bottom = y.checked_add(region.size.height)?;
    (right <= frame.width && bottom <= frame.height).then_some((
        x,
        y,
        region.size.width,
        region.size.height,
    ))
}

/// Validate a video plane's geometry and compute the byte offset and row layout to pack.
fn video_layout(
    frame: Rectangle,
    crop: Option<Region>,
    pix_fmt: PixFmt,
    stride: i32,
    avail: usize,
) -> Option<VideoLayout> {
    if frame.width == 0 || frame.height == 0 {
        return None;
    }
    let bpp = pix_fmt.bytes_per_pixel();
    let full_row_bytes = usize::try_from(frame.width).ok()?.checked_mul(bpp)?;
    let stride = usize::try_from(stride).ok()?;
    let pitch = if stride == 0 { full_row_bytes } else { stride };
    if pitch < full_row_bytes {
        return None;
    }
    let (x, y, width, height) = resolve_video_crop(frame, crop)?;
    let x_bytes = usize::try_from(x).ok()?.checked_mul(bpp)?;
    let row_bytes = usize::try_from(width).ok()?.checked_mul(bpp)?;
    let y_bytes = usize::try_from(y).ok()?.checked_mul(pitch)?;
    let byte_offset = y_bytes.checked_add(x_bytes)?;
    let span = row_span(pitch, row_bytes, usize::try_from(height).ok()?)?;
    let end = byte_offset.checked_add(span)?;
    if end > avail || end > isize::MAX as usize {
        return None;
    }
    Some(VideoLayout {
        byte_offset,
        pitch,
        row_bytes,
        width,
        height,
    })
}

fn neutral_video_byte_len(width: u32, height: u32, pix_fmt: PixFmt) -> Option<usize> {
    let pixels = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?;
    let bytes = pixels.checked_mul(pix_fmt.bytes_per_pixel())?;
    (bytes > 0 && bytes <= isize::MAX as usize).then_some(bytes)
}

/// Build an opaque black frame (`[0, 0, 0, 0xFF]` per pixel) for neutral video buffers.
fn neutral_video_frame(width: u32, height: u32, pix_fmt: PixFmt) -> Option<Vec<u8>> {
    let bytes = neutral_video_byte_len(width, height, pix_fmt)?;
    let mut frame = vec![0u8; bytes];
    for px in frame.as_chunks_mut::<4>().0 {
        px[3] = 0xFF;
    }
    Some(frame)
}

/// Decide how to produce a video frame from a dequeued buffer's header flags, crop, and plane.
unsafe fn plan_video_frame(
    frame: Rectangle,
    crop: Option<Region>,
    pix_fmt: PixFmt,
    header_flags: MetaHeaderFlags,
    plane: Option<&spa_data>,
) -> Option<VideoFramePlan> {
    if header_flags.contains(MetaHeaderFlags::CORRUPTED) {
        return None;
    }
    if header_flags.contains(MetaHeaderFlags::GAP) {
        if let Some(plane) = plane
            && let Some(chunk) = unsafe { plane.chunk.as_ref() }
            && chunk.flags & SPA_CHUNK_FLAG_CORRUPTED as i32 != 0
        {
            return None;
        }
        let (_, _, width, height) = resolve_video_crop(frame, crop)?;
        neutral_video_byte_len(width, height, pix_fmt)?;
        return Some(VideoFramePlan::Neutral { width, height });
    }
    let plane = plane?;
    let region = unsafe { chunk_region(plane, false) }?;
    match region.data {
        ChunkData::Empty => {
            let (_, _, width, height) = resolve_video_crop(frame, crop)?;
            neutral_video_byte_len(width, height, pix_fmt)?;
            Some(VideoFramePlan::Neutral { width, height })
        }
        ChunkData::Bytes(base) => {
            let layout = video_layout(frame, crop, pix_fmt, region.stride, region.avail)?;
            Some(VideoFramePlan::Copy { base, layout })
        }
    }
}

unsafe fn materialize_video_frame(
    plan: VideoFramePlan,
    pix_fmt: PixFmt,
) -> Option<(Vec<u8>, (u32, u32))> {
    match plan {
        VideoFramePlan::Copy { base, layout } => {
            let vframe = unsafe {
                let src = base.add(layout.byte_offset);
                match pix_fmt {
                    PixFmt::Bgr0 => pack_rows_bgr0(
                        src,
                        layout.pitch,
                        layout.width as usize,
                        layout.height as usize,
                    )?,
                    _ => pack_rows(src, layout.pitch, layout.row_bytes, layout.height as usize)?,
                }
            };
            Some((vframe, (layout.width, layout.height)))
        }
        VideoFramePlan::Neutral { width, height } => Some((
            neutral_video_frame(width, height, pix_fmt)?,
            (width, height),
        )),
    }
}

fn video_process_callback(stream: &Stream, data: &mut VideoData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let header = buffer.find_meta::<MetaHeader>();
    let header_flags = header.map_or(MetaHeaderFlags::empty(), MetaHeader::flags);
    if header_flags.contains(MetaHeaderFlags::DISCONT) {
        data.clock.reset_domain();
    }
    if header_flags.contains(MetaHeaderFlags::CORRUPTED) {
        return;
    }
    let Some((size, pix_fmt)) = data.format else {
        return;
    };
    if data.tx.is_full() {
        return;
    }
    let raw_pts = header.map(MetaHeader::pts);
    let crop = buffer
        .find_meta::<MetaVideoCrop>()
        .map(|meta| *meta.meta_region().region());
    let plane = buffer.datas_mut().first().map(|p| p.as_raw());
    let Some(plan) = (unsafe { plan_video_frame(size, crop, pix_fmt, header_flags, plane) }) else {
        return;
    };
    // An unusable or corrupt buffer must not advance the clock or spend the next frame's slot.
    let ts = data.clock.resolve(raw_pts);
    if !data.gate.allow(ts) {
        return;
    }
    let Some((vframe, out_size)) = (unsafe { materialize_video_frame(plan, pix_fmt) }) else {
        return;
    };
    if data.reported_size != out_size {
        *data.size.lock() = out_size;
        data.reported_size = out_size;
    }
    let _ = data.tx.try_send(VideoFrame {
        vframe,
        size: out_size,
        pix_fmt,
        ts,
    });
}

/// Leave the loop when the stream ends on its own, so the senders drop and a consumer's
/// `recv` reports that the capture is over instead of blocking forever.
///
/// Before a format is agreed `create` is still waiting, so it is told why rather than left
/// to find the channel closed.
fn video_state_change_callback(
    _stream: &Stream,
    data: &mut VideoData,
    _old: StreamState,
    new: StreamState,
) {
    match new {
        StreamState::Error(e) => error!("pipewire stream error: {e}"),
        StreamState::Unconnected => {}
        _ => return,
    }
    if let Some(setup_tx) = data.setup_tx.take() {
        let _ = setup_tx.try_send(Err(CaptureError::StreamFailed));
    }
    data.mainloop.quit();
}

fn video_param_change_callback(
    _stream: &Stream,
    data: &mut VideoData,
    id: u32,
    param: Option<&Pod>,
) {
    if id != ParamType::Format.as_raw() {
        return;
    }
    data.clock.reset_domain();
    data.format = param.and_then(video_format);
    let Some((size, _)) = data.format else {
        // A null parameter clears the old format during renegotiation.
        if param.is_some() {
            error!("PipeWire negotiated an unusable video format");
            if let Some(tx) = data.setup_tx.take() {
                let _ = tx.try_send(Err(CaptureError::StreamFailed));
            }
            data.mainloop.quit();
        }
        return;
    };
    *data.size.lock() = (size.width, size.height);
    data.reported_size = (size.width, size.height);
    if let Some(setup_tx) = data.setup_tx.take() {
        let _ = setup_tx.try_send(Ok(()));
    }
}

fn video_format(param: &Pod) -> Option<(Rectangle, PixFmt)> {
    if parse_format(param).ok()? != (MediaType::Video, MediaSubtype::Raw) {
        return None;
    }
    let mut format = VideoInfoRaw::new();
    format.parse(param).ok()?;
    let pix_fmt = match format.format() {
        VideoFormat::BGRA => PixFmt::Bgra,
        VideoFormat::BGRx => PixFmt::Bgr0,
        _ => return None,
    };
    let size = format.size();
    (size.width > 0 && size.height > 0).then_some((size, pix_fmt))
}

/// Close the audio channel when its stream ends, so a consumer's `recv` reports that the
/// audio is over. The video capture carries on: only this sender goes away.
fn audio_state_change_callback(
    _stream: &Stream,
    data: &mut AudioData,
    _old: StreamState,
    new: StreamState,
) {
    match new {
        StreamState::Error(e) => error!("pipewire audio stream error: {e}"),
        StreamState::Unconnected => {}
        _ => return,
    }
    data.tx.borrow_mut().take();
}

fn audio_process_callback(stream: &Stream, data: &mut AudioData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let header = buffer.find_meta::<MetaHeader>();
    let header_flags = header.map_or(MetaHeaderFlags::empty(), MetaHeader::flags);
    if header_flags.contains(MetaHeaderFlags::DISCONT) {
        data.clock.reset_domain();
    }
    if header_flags.contains(MetaHeaderFlags::CORRUPTED) {
        return;
    }
    let Some(format) = &data.format else {
        return;
    };
    let tx_guard = data.tx.borrow();
    let Some(tx) = tx_guard.as_ref() else {
        return;
    };
    if tx.is_full() {
        return;
    }
    let raw_pts = header.map(MetaHeader::pts);
    let force_empty = header_flags.contains(MetaHeaderFlags::GAP);
    let channels = format.channels() as usize;
    let Some(planes) = buffer.datas_mut().get(..channels) else {
        return;
    };
    let Some((aframe, nb_samples)) =
        (unsafe { pack_audio_planes(planes.iter().map(|p| p.as_raw()), force_empty) })
    else {
        return;
    };
    let ts = data.clock.resolve(raw_pts);
    let _ = tx.try_send(AudioFrame {
        aframe,
        nb_samples,
        sample_rate: format.rate() as i32,
        nb_channels: format.channels() as i32,
        sample_fmt: SampleFmt::F32P,
        ts,
    });
}

/// Copy planar F32 samples up to the last complete sample shared by all channels.
///
/// Planes marked `SPA_CHUNK_FLAG_EMPTY` (or buffers with `MetaHeaderFlags::GAP`, passed
/// via `force_empty`) emit zeroed silence for their valid sample count without reading
/// `plane.data`. Each non-empty plane and its chunk must remain mapped and readable
/// throughout the copy.
unsafe fn pack_audio_planes<'a>(
    planes: impl ExactSizeIterator<Item = &'a spa_data> + Clone,
    force_empty: bool,
) -> Option<(Vec<u8>, i32)> {
    const SAMPLE_BYTES: usize = size_of::<f32>();
    let channels = planes.len();
    let layout = planes.map(|plane| {
        let region = unsafe { chunk_region(plane, force_empty) }?;
        let pitch = if region.stride == 0 {
            SAMPLE_BYTES
        } else {
            usize::try_from(region.stride).ok()?
        };
        if pitch < SAMPLE_BYTES {
            return None;
        }
        let samples = 1 + region.avail.checked_sub(SAMPLE_BYTES)? / pitch;
        Some((region.data, pitch, samples))
    });
    let samples = layout.clone().map(|p| p.map_or(0, |(_, _, n)| n)).min()?;
    if samples == 0 {
        return None;
    }
    let plane_bytes = samples.checked_mul(SAMPLE_BYTES)?;
    let total = plane_bytes.checked_mul(channels)?;
    if total > isize::MAX as usize {
        return None;
    }
    let mut frame = Vec::with_capacity(total);
    for plane in layout {
        let (data, pitch, _) = plane?;
        match data {
            ChunkData::Bytes(base) => {
                unsafe { append_rows(&mut frame, base, pitch, SAMPLE_BYTES, samples)? };
            }
            ChunkData::Empty => frame.resize(frame.len() + plane_bytes, 0),
        }
    }
    Some((frame, i32::try_from(samples).ok()?))
}

impl AudioData {
    fn set_format(&mut self, param: Option<&Pod>) {
        if self.tx.borrow().is_none() {
            return;
        }
        self.clock.reset_domain();
        self.format = param.and_then(|param| {
            if parse_format(param).ok()? != (MediaType::Audio, MediaSubtype::Raw) {
                return None;
            }
            let mut format = AudioInfoRaw::new();
            format.parse(param).ok()?;
            (format.format() == AudioFormat::F32P
                && format.rate() as i32 > 0
                && format.channels() as i32 > 0)
                .then_some(format)
        });
        *self.sample_rate.lock() = self.format.as_ref().map(|format| format.rate() as i32);
        if param.is_some() && self.format.is_none() {
            // The stream can negotiate a usable format later. Its state callback owns shutdown.
            error!("PipeWire negotiated an unusable audio format");
        }
    }
}

impl Drop for PipewireSession {
    fn drop(&mut self) {
        self.terminate();
        if let Some(jh) = self.jh.take() {
            let _ = jh.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reused_node_id_cannot_replace_the_selected_stream() {
        let waiting = TargetState::Waiting(StreamTarget::Serial(100));
        assert_eq!(waiting.observe(100, Some("101")), waiting);
        assert_eq!(waiting.observe(7, None), waiting);
        assert_eq!(waiting.observe(7, Some("100")), TargetState::Present(7));
        assert_eq!(TargetState::Gone.observe(7, Some("101")), TargetState::Gone);
    }

    #[test]
    fn audio_can_renegotiate_after_an_unusable_format() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut audio = AudioData {
            tx: Rc::new(RefCell::new(Some(tx))),
            clock: StreamClock::default(),
            format: None,
            sample_rate: Arc::default(),
        };
        const SEC: u64 = 1_000_000_000;
        // Put the clock in Rebased domain, then verify format renegotiation resets domain
        // while keeping `last` as the monotonic floor.
        let t0 = audio.clock.resolve_with(Some(SEC as i64), || 100 * SEC);
        assert_eq!(t0, 100 * SEC);
        assert!(matches!(audio.clock.domain, ClockDomain::Rebased { .. }));

        for (format, expected_rate) in [
            (AudioFormat::F32P, Some(48000)),
            (AudioFormat::S16LE, None),
            (AudioFormat::F32P, Some(48000)),
        ] {
            let obj = object! {
                SpaTypes::ObjectParamFormat, ParamType::Format,
                property! { FormatProperties::MediaType, Id, MediaType::Audio },
                property! { FormatProperties::MediaSubtype, Id, MediaSubtype::Raw },
                property! { FormatProperties::AudioFormat, Id, format },
                property! { FormatProperties::AudioRate, Int, 48000 },
                property! { FormatProperties::AudioChannels, Int, 2 }
            };
            let bytes = serialize_pod(Value::Object(obj)).unwrap();
            audio.set_format(Pod::from_bytes(&bytes));
            assert_eq!(audio.clock.domain, ClockDomain::Unknown);
            assert_eq!(audio.clock.last, 100 * SEC);
            assert_eq!(*audio.sample_rate.lock(), expected_rate);
            assert!(matches!(
                rx.try_recv(),
                Err(crossbeam_channel::TryRecvError::Empty)
            ));
        }

        // Switching to CLOCK_MONOTONIC after renegotiation does not jump to 200 s.
        let t1 = audio
            .clock
            .resolve_with(Some((101 * SEC) as i64), || 101 * SEC + 1_000_000);
        assert_eq!(t1, 101 * SEC);
        assert_eq!(audio.clock.domain, ClockDomain::Monotonic);
    }

    #[test]
    fn planar_audio_skips_padding_and_ends_at_the_shortest_channel() {
        use pipewire::spa::sys::spa_chunk;

        let left = [1u32, u32::MAX, 2];
        let right = [3u32, 4, 5];
        let pack = |stride| {
            let mut left_chunk = spa_chunk {
                offset: 0,
                size: 12,
                stride,
                flags: 0,
            };
            let mut right_chunk = spa_chunk {
                stride: 0,
                ..left_chunk
            };
            let planes =
                [(&left, &mut left_chunk), (&right, &mut right_chunk)].map(|(words, chunk)| {
                    spa_data {
                        type_: pipewire::spa::sys::SPA_DATA_MemPtr,
                        flags: SPA_DATA_FLAG_READABLE,
                        fd: -1,
                        mapoffset: 0,
                        maxsize: size_of_val(words) as u32,
                        data: words.as_ptr().cast_mut().cast(),
                        chunk,
                    }
                });
            unsafe { pack_audio_planes(planes.iter(), false) }
        };
        let expected = [1u32, 2, 3, 4]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect();
        assert_eq!(pack(8), Some((expected, 2)));
        assert_eq!(pack(2), None);
        assert_eq!(pack(-4), None);
    }

    #[test]
    fn negotiated_video_requires_a_supported_format_and_nonempty_size() {
        let size = Rectangle {
            width: 16,
            height: 8,
        };
        for (format, dimensions, expected) in [
            (VideoFormat::BGRA, size, Some(PixFmt::Bgra)),
            (VideoFormat::BGRx, size, Some(PixFmt::Bgr0)),
            (VideoFormat::RGB, size, None),
            (VideoFormat::BGRA, Rectangle { width: 0, ..size }, None),
        ] {
            let obj = object! {
                SpaTypes::ObjectParamFormat, ParamType::Format,
                property! { FormatProperties::MediaType, Id, MediaType::Video },
                property! { FormatProperties::MediaSubtype, Id, MediaSubtype::Raw },
                property! { FormatProperties::VideoFormat, Id, format },
                property! { FormatProperties::VideoSize, Rectangle, dimensions }
            };
            let bytes = serialize_pod(Value::Object(obj)).unwrap();
            assert_eq!(
                video_format(Pod::from_bytes(&bytes).unwrap()),
                expected.map(|pix_fmt| (dimensions, pix_fmt))
            );
        }
    }

    #[test]
    fn chunk_region_enforces_readable_flag_and_handles_empty_and_gap_audio() {
        use pipewire::spa::sys::{SPA_DATA_FLAG_WRITABLE, spa_chunk};

        let stale_left = [0xDEAD_BEEFu32, 0xCAFE_BABE, 0x1234_5678];
        let real_right = [3u32, 4, 5];

        let mut empty_chunk = spa_chunk {
            offset: 0,
            size: 12,
            stride: 0,
            flags: SPA_CHUNK_FLAG_EMPTY,
        };
        let mut real_chunk = spa_chunk {
            offset: 0,
            size: 12,
            stride: 0,
            flags: 0,
        };
        let plane_with_flags =
            |data: *mut core::ffi::c_void, maxsize: u32, flags: u32, chunk: &mut spa_chunk| {
                spa_data {
                    type_: pipewire::spa::sys::SPA_DATA_MemPtr,
                    flags,
                    fd: -1,
                    mapoffset: 0,
                    maxsize,
                    data,
                    chunk,
                }
            };
        let plane = |data: *mut core::ffi::c_void, maxsize: u32, chunk: &mut spa_chunk| {
            plane_with_flags(data, maxsize, SPA_DATA_FLAG_READABLE, chunk)
        };

        // Non-empty planes lacking `SPA_DATA_FLAG_READABLE` (e.g. write-only or PROT_NONE) are rejected.
        for bad_flags in [0, SPA_DATA_FLAG_WRITABLE] {
            let unreadable = plane_with_flags(
                real_right.as_ptr().cast_mut().cast(),
                12,
                bad_flags,
                &mut real_chunk,
            );
            assert_eq!(unsafe { chunk_region(&unreadable, false) }, None);
            assert_eq!(
                unsafe { pack_audio_planes([unreadable].iter(), false) },
                None
            );
        }

        // Empty chunk hides `plane.data` (even when unreadable or null) so stale bytes are never read.
        let empty_plane = plane_with_flags(
            stale_left.as_ptr().cast_mut().cast(),
            12,
            0,
            &mut empty_chunk,
        );
        assert_eq!(
            unsafe { chunk_region(&empty_plane, false) },
            Some(ChunkRegion {
                data: ChunkData::Empty,
                avail: 12,
                stride: 0,
            })
        );

        // Mixed planes: left is empty (emits silence), right has real audio samples.
        let right_plane = plane(real_right.as_ptr().cast_mut().cast(), 12, &mut real_chunk);
        let expected: Vec<u8> = [0u32, 0, 0, 3, 4, 5]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect();
        assert_eq!(
            unsafe { pack_audio_planes([empty_plane, right_plane].iter(), false) },
            Some((expected, 3))
        );

        // `MetaHeaderFlags::GAP` (`force_empty = true`) emits silence without reading `plane.data`,
        // even when `SPA_CHUNK_FLAG_EMPTY` is not set and the plane is unreadable or null.
        let gap_unreadable = plane_with_flags(
            stale_left.as_ptr().cast_mut().cast(),
            12,
            0,
            &mut real_chunk,
        );
        let gap_null = plane_with_flags(core::ptr::null_mut(), 12, 0, &mut real_chunk);
        assert_eq!(
            unsafe { pack_audio_planes([gap_unreadable, gap_null].iter(), true) },
            Some((vec![0u8; 24], 3))
        );

        // Empty chunk with null data pointer still safely emits zeroed silence when size > 0.
        let null_empty = plane(core::ptr::null_mut(), 12, &mut empty_chunk);
        assert_eq!(
            unsafe { pack_audio_planes([null_empty].iter(), false) },
            Some((vec![0u8; 12], 3))
        );

        // Non-empty chunk with null data pointer is rejected.
        let null_real = plane(core::ptr::null_mut(), 12, &mut real_chunk);
        assert_eq!(unsafe { chunk_region(&null_real, false) }, None);

        // Corrupted + empty (or corrupted + GAP) is rejected.
        let mut corrupt_empty = spa_chunk {
            flags: SPA_CHUNK_FLAG_EMPTY | SPA_CHUNK_FLAG_CORRUPTED as i32,
            ..empty_chunk
        };
        assert_eq!(
            unsafe { chunk_region(&plane(core::ptr::null_mut(), 12, &mut corrupt_empty), false) },
            None
        );
        assert_eq!(
            unsafe { chunk_region(&plane(core::ptr::null_mut(), 12, &mut corrupt_empty), true) },
            None
        );

        // Zero-sized empty/GAP chunks return `avail: 0` (usable for neutral video, skipped for audio).
        let mut zero_empty = spa_chunk {
            size: 0,
            ..empty_chunk
        };
        let zero_plane = plane(core::ptr::null_mut(), 12, &mut zero_empty);
        assert_eq!(
            unsafe { chunk_region(&zero_plane, false) },
            Some(ChunkRegion {
                data: ChunkData::Empty,
                avail: 0,
                stride: 0,
            })
        );
        assert_eq!(
            unsafe { pack_audio_planes([zero_plane].iter(), false) },
            None
        );
        let mut zero_gap = spa_chunk {
            size: 0,
            ..real_chunk
        };
        let zero_gap_plane = plane(core::ptr::null_mut(), 12, &mut zero_gap);
        assert_eq!(
            unsafe { pack_audio_planes([zero_gap_plane].iter(), true) },
            None
        );

        // `chunk.offset` is taken modulo `maxsize` and clamped to `maxsize - offset`.
        let mut wrapped_chunk = spa_chunk {
            offset: 16, // 16 % 12 == 4
            size: 12,
            stride: 0,
            flags: 0,
        };
        let wrapped_plane = plane(
            real_right.as_ptr().cast_mut().cast(),
            12,
            &mut wrapped_chunk,
        );
        let wrapped = unsafe { chunk_region(&wrapped_plane, false) }.unwrap();
        assert_eq!(wrapped.avail, 8);
        assert_eq!(
            wrapped.data,
            ChunkData::Bytes(unsafe { real_right.as_ptr().cast::<u8>().add(4) })
        );
    }

    #[test]
    fn video_crop_validates_bounds_and_packs_cropped_rows() {
        use core::mem::MaybeUninit;
        use pipewire::spa::utils::Point;

        let frame = Rectangle {
            width: 4,
            height: 3,
        };
        let mk_crop = |x, y, width, height| {
            Some(Region {
                position: Point { x, y },
                size: Rectangle { width, height },
            })
        };

        // Valid interior crop, full-frame crop, absent crop, and empty crop (width==0 || height==0).
        assert_eq!(
            resolve_video_crop(frame, mk_crop(1, 1, 2, 2)),
            Some((1, 1, 2, 2))
        );
        assert_eq!(
            resolve_video_crop(frame, mk_crop(0, 0, 4, 3)),
            Some((0, 0, 4, 3))
        );
        assert_eq!(resolve_video_crop(frame, None), Some((0, 0, 4, 3)));
        for empty in [
            mk_crop(0, 0, 0, 2),
            mk_crop(0, 0, 2, 0),
            mk_crop(-1, -1, 0, 0),
        ] {
            assert_eq!(resolve_video_crop(frame, empty), Some((0, 0, 4, 3)));
        }
        assert_eq!(
            resolve_video_crop(
                Rectangle {
                    width: 0,
                    height: 3
                },
                None
            ),
            None
        );

        // Nonempty negative, out-of-bounds, and overflowing crops are rejected so frames drop.
        for bad in [
            mk_crop(-1, 0, 2, 2),
            mk_crop(0, -1, 2, 2),
            mk_crop(3, 0, 2, 2),
            mk_crop(0, 2, 2, 2),
            mk_crop(i32::MAX, 0, u32::MAX, 1),
            mk_crop(0, i32::MAX, 1, u32::MAX),
        ] {
            assert_eq!(resolve_video_crop(frame, bad), None);
            assert_eq!(video_layout(frame, bad, PixFmt::Bgra, 16, 48), None);
        }

        // Pack a 2x2 crop at (1, 1) from a 4x3 BGRA image with 20-byte stride (4 bytes row padding).
        let stride = 20usize;
        let mut buf = vec![0u8; stride * 3];
        for y in 0..3u8 {
            for x in 0..4u8 {
                let off = y as usize * stride + x as usize * 4;
                buf[off..off + 4].copy_from_slice(&[x, y, 0xAA, 0x80]);
            }
        }
        let layout = video_layout(
            frame,
            mk_crop(1, 1, 2, 2),
            PixFmt::Bgra,
            stride as i32,
            buf.len(),
        )
        .unwrap();
        assert_eq!(
            layout,
            VideoLayout {
                byte_offset: stride + 4,
                pitch: stride,
                row_bytes: 8,
                width: 2,
                height: 2,
            }
        );
        let (packed, out_size) = unsafe {
            materialize_video_frame(
                VideoFramePlan::Copy {
                    base: buf.as_ptr(),
                    layout,
                },
                PixFmt::Bgra,
            )
        }
        .unwrap();
        assert_eq!(out_size, (2, 2));
        assert_eq!(
            packed,
            [
                1, 1, 0xAA, 0x80, 2, 1, 0xAA, 0x80, // row 1, cols 1..3
                1, 2, 0xAA, 0x80, 2, 2, 0xAA, 0x80, // row 2, cols 1..3
            ]
        );

        // BGRx (`PixFmt::Bgr0`) with uninitialized 4th bytes and row padding packs via `pack_rows_bgr0`.
        let mut bgrx = vec![MaybeUninit::<u8>::uninit(); stride * 3];
        for y in 0..3u8 {
            for x in 0..4u8 {
                let off = y as usize * stride + x as usize * 4;
                bgrx[off].write(x);
                bgrx[off + 1].write(y);
                bgrx[off + 2].write(0xBB);
                // 4th byte intentionally left uninitialized.
            }
        }
        let (packed_bgr0, out_size_bgr0) = unsafe {
            materialize_video_frame(
                VideoFramePlan::Copy {
                    base: bgrx.as_ptr().cast(),
                    layout,
                },
                PixFmt::Bgr0,
            )
        }
        .unwrap();
        assert_eq!(out_size_bgr0, (2, 2));
        assert_eq!(
            packed_bgr0,
            [
                1, 1, 0xBB, 0xFF, 2, 1, 0xBB, 0xFF, // row 1, cols 1..3
                1, 2, 0xBB, 0xFF, 2, 2, 0xBB, 0xFF, // row 2, cols 1..3
            ]
        );

        // Stride == 0 uses full negotiated frame row pitch, not cropped row width.
        let tight_layout =
            video_layout(frame, mk_crop(1, 1, 2, 2), PixFmt::Bgra, 0, 4 * 3 * 4).unwrap();
        assert_eq!(tight_layout.pitch, 16);
        assert_eq!(tight_layout.byte_offset, 16 + 4);

        // Reject undersized buffer, too-small stride, and negative stride.
        assert_eq!(
            video_layout(frame, mk_crop(1, 1, 2, 2), PixFmt::Bgra, stride as i32, 40),
            None
        );
        assert_eq!(
            video_layout(frame, mk_crop(0, 0, 2, 2), PixFmt::Bgra, 8, buf.len()),
            None
        );
        assert_eq!(
            video_layout(frame, None, PixFmt::Bgra, -16, buf.len()),
            None
        );

        // Both header and crop meta params serialize to valid Pods.
        let header_pod = meta_param(SPA_META_Header, size_of::<spa_meta_header>()).unwrap();
        let crop_pod = meta_param(SPA_META_VideoCrop, size_of::<spa_meta_region>()).unwrap();
        assert!(Pod::from_bytes(&header_pod).is_some());
        assert!(Pod::from_bytes(&crop_pod).is_some());
    }

    #[test]
    fn video_neutral_frames_and_header_flags() {
        use pipewire::spa::{sys::spa_chunk, utils::Point};

        let frame = Rectangle {
            width: 4,
            height: 3,
        };
        let crop = Some(Region {
            position: Point { x: 1, y: 1 },
            size: Rectangle {
                width: 2,
                height: 2,
            },
        });
        let bad_crop = Some(Region {
            position: Point { x: 3, y: 0 },
            size: Rectangle {
                width: 2,
                height: 2,
            },
        });
        let stale = [0xAAu8; 48];
        let mut normal_chunk = spa_chunk {
            offset: 0,
            size: 48,
            stride: 16,
            flags: 0,
        };
        let mut empty_chunk = spa_chunk {
            offset: 0,
            size: 0,
            stride: 0,
            flags: SPA_CHUNK_FLAG_EMPTY,
        };
        let mut corrupt_chunk = spa_chunk {
            offset: 0,
            size: 48,
            stride: 16,
            flags: SPA_CHUNK_FLAG_CORRUPTED as i32,
        };
        let mk_plane = |data: *mut core::ffi::c_void, flags: u32, chunk: &mut spa_chunk| spa_data {
            type_: pipewire::spa::sys::SPA_DATA_MemPtr,
            flags,
            fd: -1,
            mapoffset: 0,
            maxsize: 48,
            data,
            chunk,
        };

        let readable_plane = mk_plane(
            stale.as_ptr().cast_mut().cast(),
            SPA_DATA_FLAG_READABLE,
            &mut normal_chunk,
        );
        let unreadable_plane = mk_plane(stale.as_ptr().cast_mut().cast(), 0, &mut normal_chunk);
        let empty_null_plane = mk_plane(core::ptr::null_mut(), 0, &mut empty_chunk);
        let corrupt_plane = mk_plane(
            stale.as_ptr().cast_mut().cast(),
            SPA_DATA_FLAG_READABLE,
            &mut corrupt_chunk,
        );

        // `MetaHeaderFlags::CORRUPTED` drops buffers unconditionally (even with `GAP`).
        assert_eq!(
            unsafe {
                plan_video_frame(
                    frame,
                    crop,
                    PixFmt::Bgra,
                    MetaHeaderFlags::CORRUPTED,
                    Some(&readable_plane),
                )
            },
            None
        );
        assert_eq!(
            unsafe {
                plan_video_frame(
                    frame,
                    crop,
                    PixFmt::Bgra,
                    MetaHeaderFlags::CORRUPTED | MetaHeaderFlags::GAP,
                    Some(&empty_null_plane),
                )
            },
            None
        );

        // `MetaHeaderFlags::GAP` produces a neutral black frame respecting crop without reading
        // `plane.data` (works with unreadable plane or even no plane).
        let expected_black: Vec<u8> = [0, 0, 0, 0xFF].repeat(4);
        for (pix_fmt, plane_opt) in [
            (PixFmt::Bgra, Some(&unreadable_plane)),
            (PixFmt::Bgr0, None),
        ] {
            let plan =
                unsafe { plan_video_frame(frame, crop, pix_fmt, MetaHeaderFlags::GAP, plane_opt) };
            assert_eq!(
                plan,
                Some(VideoFramePlan::Neutral {
                    width: 2,
                    height: 2
                })
            );
            let (pixels, out_size) =
                unsafe { materialize_video_frame(plan.unwrap(), pix_fmt) }.unwrap();
            assert_eq!(out_size, (2, 2));
            assert_eq!(pixels, expected_black);
        }

        // `MetaHeaderFlags::GAP` still drops buffers with `SPA_CHUNK_FLAG_CORRUPTED` or bad crop.
        assert_eq!(
            unsafe {
                plan_video_frame(
                    frame,
                    crop,
                    PixFmt::Bgra,
                    MetaHeaderFlags::GAP,
                    Some(&corrupt_plane),
                )
            },
            None
        );
        assert_eq!(
            unsafe {
                plan_video_frame(
                    frame,
                    bad_crop,
                    PixFmt::Bgra,
                    MetaHeaderFlags::GAP,
                    Some(&unreadable_plane),
                )
            },
            None
        );

        // `SPA_CHUNK_FLAG_EMPTY` (even with `chunk.size == 0`, null data, and no READABLE flag)
        // produces a neutral black frame respecting crop and `FpsGate`.
        let empty_plan = unsafe {
            plan_video_frame(
                frame,
                crop,
                PixFmt::Bgr0,
                MetaHeaderFlags::empty(),
                Some(&empty_null_plane),
            )
        };
        assert_eq!(
            empty_plan,
            Some(VideoFramePlan::Neutral {
                width: 2,
                height: 2,
            })
        );
        let (pixels, out_size) =
            unsafe { materialize_video_frame(empty_plan.unwrap(), PixFmt::Bgr0) }.unwrap();
        assert_eq!(out_size, (2, 2));
        assert_eq!(pixels, expected_black);

        // Unreadable non-empty plane is dropped.
        assert_eq!(
            unsafe {
                plan_video_frame(
                    frame,
                    crop,
                    PixFmt::Bgra,
                    MetaHeaderFlags::empty(),
                    Some(&unreadable_plane),
                )
            },
            None
        );

        // `SPA_CHUNK_FLAG_EMPTY` with out-of-bounds nonempty crop is dropped.
        assert_eq!(
            unsafe {
                plan_video_frame(
                    frame,
                    bad_crop,
                    PixFmt::Bgra,
                    MetaHeaderFlags::empty(),
                    Some(&empty_null_plane),
                )
            },
            None
        );

        // Neutral frames still obey `FpsGate`.
        let mut gate = FpsGate::new(Some(60));
        assert!(gate.allow(1_000_000_000));
        assert!(!gate.allow(1_005_000_000));
        assert!(gate.allow(1_016_666_667));
    }

    #[test]
    fn stream_clock_handles_monotonic_missing_and_stream_relative_pts() {
        const SEC: u64 = 1_000_000_000;
        const FRAME: u64 = 16_666_667;

        // 1. Stream-relative producer starting at pts == 0 does not jump backwards.
        let mut clock = StreamClock::default();
        let t0 = clock.resolve_with(Some(0), || 100 * SEC);
        let t1 = clock.resolve_with(Some(FRAME as i64), || 100 * SEC + FRAME);
        let t2 = clock.resolve_with(Some((2 * FRAME) as i64), || unreachable!());
        assert_eq!(t0, 100 * SEC);
        assert_eq!(t1, 100 * SEC + FRAME);
        assert_eq!(t2, 100 * SEC + 2 * FRAME);

        // Stream-relative reset back to 0 re-anchors monotonically.
        let t3 = clock.resolve_with(Some(0), || 100 * SEC + 3 * FRAME);
        let t4 = clock.resolve_with(Some(FRAME as i64), || 100 * SEC + 4 * FRAME);
        let t5 = clock.resolve_with(Some((2 * FRAME) as i64), || unreachable!());
        assert_eq!(t3, 100 * SEC + 3 * FRAME);
        assert_eq!(t4, 100 * SEC + 4 * FRAME);
        assert_eq!(t5, 100 * SEC + 5 * FRAME);

        // 2. Normal CLOCK_MONOTONIC producer passes pts through without calling clock_gettime again.
        let mut clock = StreamClock::default();
        let m0 = clock.resolve_with(Some((50 * SEC) as i64), || 50 * SEC + 2_000_000);
        let m1 = clock.resolve_with(Some((50 * SEC + FRAME) as i64), || unreachable!());
        let m2 = clock.resolve_with(None, || 50 * SEC + 2 * FRAME);
        assert_eq!(m0, 50 * SEC);
        assert_eq!(m1, 50 * SEC + FRAME);
        assert_eq!(m2, 50 * SEC + 2 * FRAME);

        // 3. Producer with always-zero or negative PTS falls back monotonically to CLOCK_MONOTONIC.
        let mut clock = StreamClock::default();
        assert_eq!(clock.resolve_with(Some(0), || 10 * SEC), 10 * SEC);
        assert_eq!(
            clock.resolve_with(Some(i64::MIN), || 10 * SEC + FRAME),
            10 * SEC + FRAME
        );
        assert_eq!(
            clock.resolve_with(Some(0), || 10 * SEC + 2 * FRAME),
            10 * SEC + 2 * FRAME
        );

        // 4. Freshly booted machine (uptime < MONOTONIC_TOLERANCE_NS) still rebases 0-based streams.
        let mut clock = StreamClock::default();
        let b0 = clock.resolve_with(Some(0), || 500_000_000);
        let b1 = clock.resolve_with(Some(FRAME as i64), || 500_000_000 + FRAME);
        assert_eq!(b0, 500_000_000);
        assert_eq!(b1, 500_000_000 + FRAME);

        // 5. Monotonic domain stays monotonic after a missing-PTS fallback whose callback time
        //    is ahead of the next frame's capture PTS, and rebases on large backward resets.
        let mut clock = StreamClock::default();
        let c0 = clock.resolve_with(Some((80 * SEC) as i64), || 80 * SEC + 1_000_000);
        // Callback time is 5 ms after the next frame's capture timestamp.
        let c1 = clock.resolve_with(None, || 80 * SEC + FRAME + 5_000_000);
        let c2 = clock.resolve_with(
            Some((80 * SEC + FRAME + 2_000_000) as i64),
            || unreachable!(),
        );
        let c3 = clock.resolve_with(Some((80 * SEC + 2 * FRAME) as i64), || unreachable!());
        assert_eq!(c0, 80 * SEC);
        assert_eq!(c1, 80 * SEC + FRAME + 5_000_000);
        assert_eq!(c2, c1);
        assert_eq!(c3, 80 * SEC + 2 * FRAME);

        // Large backward jump (e.g. renegotiation resetting PTS to stream-relative) rebases cleanly.
        let c4 = clock.resolve_with(Some(FRAME as i64), || 80 * SEC + 3 * FRAME);
        let c5 = clock.resolve_with(Some((2 * FRAME) as i64), || unreachable!());
        assert_eq!(c4, 80 * SEC + 3 * FRAME);
        assert_eq!(c5, 80 * SEC + 4 * FRAME);
    }

    #[test]
    fn stream_clock_handles_renegotiation_discont_and_forward_leaps() {
        const SEC: u64 = 1_000_000_000;
        const FRAME: u64 = 16_666_667;

        // 1. Renegotiation / DISCONT switching from stream-relative PTS to CLOCK_MONOTONIC
        //    must not leap by system uptime, and keeps `last` as a monotonic floor.
        let mut clock = StreamClock::default();
        assert_eq!(
            clock.resolve_with(Some(SEC as i64), || 100 * SEC),
            100 * SEC
        );
        assert!(matches!(clock.domain, ClockDomain::Rebased { .. }));
        clock.reset_domain();
        assert_eq!(clock.domain, ClockDomain::Unknown);
        // Slightly behind `last` within capture latency still classifies as Monotonic and
        // clamps to `last`.
        let after_discont = clock.resolve_with(Some((100 * SEC - 5_000_000) as i64), || {
            100 * SEC + 5_000_000
        });
        assert_eq!(after_discont, 100 * SEC);
        assert_eq!(clock.domain, ClockDomain::Monotonic);
        assert_eq!(
            clock.resolve_with(Some((100 * SEC + FRAME) as i64), || unreachable!()),
            100 * SEC + FRAME
        );

        // DISCONT switching back from CLOCK_MONOTONIC to stream-relative PTS rebases cleanly.
        clock.reset_domain();
        let rebased_again = clock.resolve_with(Some(FRAME as i64), || 101 * SEC);
        assert_eq!(rebased_again, 101 * SEC);
        assert!(matches!(clock.domain, ClockDomain::Rebased { .. }));

        // 2. Even WITHOUT DISCONT or format renegotiation, a Rebased stream switching to
        //    CLOCK_MONOTONIC cannot leap into the future by system uptime.
        let mut clock = StreamClock::default();
        assert_eq!(
            clock.resolve_with(Some(SEC as i64), || 100 * SEC),
            100 * SEC
        );
        let switched = clock.resolve_with(Some((101 * SEC) as i64), || 101 * SEC + 1_000_000);
        assert_eq!(switched, 101 * SEC);
        assert_eq!(clock.domain, ClockDomain::Monotonic);
        // Subsequent None fallback tracks real time instead of being clamped to 200 s.
        assert_eq!(
            clock.resolve_with(None, || 101 * SEC + FRAME),
            101 * SEC + FRAME
        );

        // 3. Same silent switch on a low-uptime machine (uptime 2 s < MONOTONIC_TOLERANCE_NS).
        let mut clock = StreamClock::default();
        assert_eq!(clock.resolve_with(Some(0), || 2 * SEC), 2 * SEC);
        assert_eq!(
            clock.resolve_with(Some(FRAME as i64), || 2 * SEC + FRAME),
            2 * SEC + FRAME
        );
        assert!(matches!(clock.domain, ClockDomain::Rebased { .. }));
        let switched_low = clock.resolve_with(Some((2 * SEC + 100_000_000) as i64), || {
            2 * SEC + 101_000_000
        });
        assert_eq!(switched_low, 2 * SEC + 100_000_000);
        assert_eq!(clock.domain, ClockDomain::Monotonic);

        // 4. A legitimate 60-second idle pause on a stream-relative producer stays Rebased.
        let mut clock = StreamClock::default();
        assert_eq!(
            clock.resolve_with(Some(SEC as i64), || 100 * SEC),
            100 * SEC
        );
        let after_pause = clock.resolve_with(Some((61 * SEC) as i64), || 160 * SEC + 2_000_000);
        assert_eq!(after_pause, 160 * SEC);
        assert!(matches!(clock.domain, ClockDomain::Rebased { .. }));

        // 5. Overflowing PTS in Rebased re-anchors to `now()` instead of saturating to `u64::MAX`.
        let mut clock = StreamClock::default();
        assert_eq!(
            clock.resolve_with(Some(SEC as i64), || 100 * SEC),
            100 * SEC
        );
        let overflowed = clock.resolve_with(Some(i64::MAX), || 101 * SEC);
        assert_eq!(overflowed, 101 * SEC);
    }

    #[test]
    fn audio_probe_rejects_closed_or_unresponsive_peer() {
        use std::os::{fd::OwnedFd, unix::net::UnixStream};

        let mainloop = MainLoopRc::new(None).unwrap();
        let context = ContextRc::new(&mainloop, None).unwrap();

        // Peer disconnects immediately: server handshake fails with `Unsupported::Audio`.
        let (client, server) = UnixStream::pair().unwrap();
        drop(server);
        let core = context.connect_fd_rc(OwnedFd::from(client), None).unwrap();
        assert!(matches!(
            probe_audio_core_on(core, &mainloop, Duration::from_millis(500)),
            Err(CaptureError::Unsupported(Unsupported::Audio))
        ));

        // Unresponsive peer: bounded timer expires and returns `Unsupported::Audio`.
        let (client, _silent_server) = UnixStream::pair().unwrap();
        let core = context.connect_fd_rc(OwnedFd::from(client), None).unwrap();
        assert!(matches!(
            probe_audio_core_on(core, &mainloop, Duration::from_millis(50)),
            Err(CaptureError::Unsupported(Unsupported::Audio))
        ));
    }
}
