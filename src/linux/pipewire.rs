//! pipewire related

use std::{
    cell::Cell,
    fmt,
    rc::Rc,
    sync::Arc,
    thread::{self, JoinHandle},
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
        buffer::meta::{MetaHeader, MetaVideoCrop},
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
            SPA_CHUNK_FLAG_CORRUPTED, SPA_CHUNK_FLAG_EMPTY, SPA_META_Header, SPA_META_VideoCrop,
            SPA_PARAM_META_size, SPA_PARAM_META_type, SPA_PARAM_Meta, SPA_TYPE_OBJECT_ParamMeta,
            spa_data, spa_meta_header, spa_meta_region,
        },
        utils::{Direction, Fraction, Rectangle, Region, SpaTypes},
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
    util::{append_rows, pack_rows, row_span},
};

use super::dbus::{DbusScreen, SOURCE_TYPE_MONITOR, StreamTarget};

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
    /// Dropped when the stream ends, which closes the channel a consumer is waiting on.
    tx: Option<Sender<AudioFrame>>,
    clock: StreamClock,
    format: Option<AudioInfoRaw>,
    sample_rate: Arc<Mutex<Option<i32>>>,
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
                        // connection rather than the portal's remote. Verify this before
                        // starting the portal picker so a sandbox that grants only the
                        // screencast remote reports `Unsupported::Audio` right away instead
                        // of showing the picker a second time on retry.
                        Some(context.connect_rc(None).map_err(|e| {
                            error!("no PipeWire connection for audio: {e}");
                            Unsupported::Audio
                        })?)
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
                        let audio_stream = StreamRc::new(
                            audio_core,
                            "scrcap-audio",
                            properties! {
                                *keys::MEDIA_TYPE => "Audio",
                                *keys::MEDIA_CATEGORY => "Capture",
                                *keys::MEDIA_ROLE => "Music",
                                *keys::STREAM_CAPTURE_SINK => "true",
                            },
                        )?;
                        let audio_data = AudioData {
                            tx: Some(a_tx),
                            clock: StreamClock::default(),
                            format: None,
                            sample_rate: sample_rate.expect("sample_rate exists when audio is on"),
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
                        // Listener first: tuple fields drop in order, and a listener
                        // dropped after its stream unlinks its hook from freed memory.
                        Some((audio_listener, audio_stream))
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
    fn resolve(&mut self, raw_pts: Option<i64>) -> u64 {
        self.resolve_with(raw_pts, monotonic_now)
    }

    fn resolve_with(&mut self, raw_pts: Option<i64>, now: impl FnOnce() -> u64) -> u64 {
        let pts = raw_pts
            .and_then(|p| u64::try_from(p).ok())
            .filter(|&p| p != 0);
        match (self.domain, pts) {
            (ClockDomain::Monotonic, Some(pts)) => {
                if pts >= self.last {
                    self.last = pts;
                    pts
                } else if self.last - pts <= MONOTONIC_BACKWARD_LATENCY_NS {
                    self.last
                } else {
                    let base_mono = now().max(self.last);
                    self.domain = ClockDomain::Rebased {
                        base_pts: pts,
                        base_mono,
                    };
                    self.last = base_mono;
                    base_mono
                }
            }
            (ClockDomain::Unknown, Some(pts)) => {
                let mono = now();
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
            (
                ClockDomain::Rebased {
                    base_pts,
                    base_mono,
                },
                Some(pts),
            ) => {
                if let Some(ts) = pts
                    .checked_sub(base_pts)
                    .map(|delta| base_mono.saturating_add(delta))
                    .filter(|&ts| ts >= self.last)
                {
                    self.last = ts;
                    ts
                } else {
                    let base_mono = now().max(self.last);
                    self.domain = ClockDomain::Rebased {
                        base_pts: pts,
                        base_mono,
                    };
                    self.last = base_mono;
                    base_mono
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
/// unmapped, zero-sized or corrupt chunk, and [`ChunkData::Empty`] without exposing
/// `plane.data` when `SPA_CHUNK_FLAG_EMPTY` is set.
unsafe fn chunk_region(plane: &spa_data) -> Option<ChunkRegion> {
    let chunk = unsafe { plane.chunk.as_ref() }?;
    let maxsize = plane.maxsize as usize;
    if maxsize == 0 || chunk.size == 0 {
        return None;
    }
    if chunk.flags & SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 {
        return None;
    }
    let offset = chunk.offset as usize % maxsize;
    let avail = (maxsize - offset).min(chunk.size as usize);
    if avail == 0 {
        return None;
    }
    let data = if chunk.flags & SPA_CHUNK_FLAG_EMPTY as i32 != 0 {
        ChunkData::Empty
    } else {
        if plane.data.is_null() {
            return None;
        }
        ChunkData::Bytes(unsafe { (plane.data as *const u8).add(offset) })
    };
    Some(ChunkRegion {
        data,
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

/// Resolve an optional `SPA_META_VideoCrop` region against the negotiated frame size.
///
/// Falls back cleanly to the full negotiated frame when `crop` is absent, empty,
/// negative, overflows, or extends outside `frame`.
fn resolve_video_crop(frame: Rectangle, crop: Option<Region>) -> (u32, u32, u32, u32) {
    if let Some(region) = crop
        && let Ok(x) = u32::try_from(region.position.x)
        && let Ok(y) = u32::try_from(region.position.y)
        && region.size.width > 0
        && region.size.height > 0
        && x.checked_add(region.size.width)
            .is_some_and(|right| right <= frame.width)
        && y.checked_add(region.size.height)
            .is_some_and(|bottom| bottom <= frame.height)
    {
        return (x, y, region.size.width, region.size.height);
    }
    (0, 0, frame.width, frame.height)
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
    let (x, y, width, height) = resolve_video_crop(frame, crop);
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

fn video_process_callback(stream: &Stream, data: &mut VideoData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let Some((size, pix_fmt)) = data.format else {
        return;
    };
    if data.tx.is_full() {
        return;
    }
    let raw_pts = buffer.find_meta::<MetaHeader>().map(MetaHeader::pts);
    let crop = buffer
        .find_meta::<MetaVideoCrop>()
        .map(|meta| *meta.meta_region().region());
    let Some(plane) = buffer.datas_mut().first() else {
        return;
    };
    let Some(region) = (unsafe { chunk_region(plane.as_raw()) }) else {
        return;
    };
    let ChunkData::Bytes(base) = region.data else {
        return;
    };
    let Some(layout) = video_layout(size, crop, pix_fmt, region.stride, region.avail) else {
        return;
    };
    // An unusable or empty buffer must not advance the clock or spend the next frame's slot.
    let ts = data.clock.resolve(raw_pts);
    if !data.gate.allow(ts) {
        return;
    }
    let Some(vframe) = (unsafe {
        pack_rows(
            base.add(layout.byte_offset),
            layout.pitch,
            layout.row_bytes,
            layout.height as usize,
        )
    }) else {
        return;
    };
    let out_size = (layout.width, layout.height);
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
    data.tx.take();
}

fn audio_process_callback(stream: &Stream, data: &mut AudioData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let (Some(tx), Some(format)) = (&data.tx, &data.format) else {
        return;
    };
    if tx.is_full() {
        return;
    }
    let raw_pts = buffer.find_meta::<MetaHeader>().map(MetaHeader::pts);
    let channels = format.channels() as usize;
    let Some(planes) = buffer.datas_mut().get(..channels) else {
        return;
    };
    let Some((aframe, nb_samples)) =
        (unsafe { pack_audio_planes(planes.iter().map(|p| p.as_raw())) })
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
/// Planes marked `SPA_CHUNK_FLAG_EMPTY` emit zeroed silence for their valid sample count
/// without reading `plane.data`. Each non-empty plane and its chunk must remain mapped and
/// readable throughout the copy.
unsafe fn pack_audio_planes<'a>(
    planes: impl ExactSizeIterator<Item = &'a spa_data> + Clone,
) -> Option<(Vec<u8>, i32)> {
    const SAMPLE_BYTES: usize = size_of::<f32>();
    let channels = planes.len();
    let layout = planes.map(|plane| {
        let region = unsafe { chunk_region(plane) }?;
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
        if self.tx.is_none() {
            return;
        }
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
            tx: Some(tx),
            clock: StreamClock::default(),
            format: None,
            sample_rate: Arc::default(),
        };
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
            assert_eq!(*audio.sample_rate.lock(), expected_rate);
            assert!(matches!(
                rx.try_recv(),
                Err(crossbeam_channel::TryRecvError::Empty)
            ));
        }
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
                        flags: pipewire::spa::sys::SPA_DATA_FLAG_READABLE,
                        fd: -1,
                        mapoffset: 0,
                        maxsize: size_of_val(words) as u32,
                        data: words.as_ptr().cast_mut().cast(),
                        chunk,
                    }
                });
            unsafe { pack_audio_planes(planes.iter()) }
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
    fn empty_chunks_return_empty_variant_and_emit_audio_silence() {
        use pipewire::spa::sys::spa_chunk;

        let stale_left = [0xDEAD_BEEFu32, 0xCAFE_BABE, 0x1234_5678];
        let real_right = [3u32, 4, 5];

        let mut empty_chunk = spa_chunk {
            offset: 0,
            size: 12,
            stride: 0,
            flags: SPA_CHUNK_FLAG_EMPTY as i32,
        };
        let mut real_chunk = spa_chunk {
            offset: 0,
            size: 12,
            stride: 0,
            flags: 0,
        };
        let plane = |data: *mut core::ffi::c_void, maxsize: u32, chunk: &mut spa_chunk| spa_data {
            type_: pipewire::spa::sys::SPA_DATA_MemPtr,
            flags: pipewire::spa::sys::SPA_DATA_FLAG_READABLE,
            fd: -1,
            mapoffset: 0,
            maxsize,
            data,
            chunk,
        };

        // Empty chunk hides `plane.data` (so video drops it and audio cannot read stale bytes).
        let empty_plane = plane(stale_left.as_ptr().cast_mut().cast(), 12, &mut empty_chunk);
        assert_eq!(
            unsafe { chunk_region(&empty_plane) },
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
            unsafe { pack_audio_planes([empty_plane, right_plane].iter()) },
            Some((expected, 3))
        );

        // Empty chunk with null data pointer still safely emits zeroed silence when size > 0.
        let null_empty = plane(core::ptr::null_mut(), 12, &mut empty_chunk);
        assert_eq!(
            unsafe { pack_audio_planes([null_empty].iter()) },
            Some((vec![0u8; 12], 3))
        );

        // Non-empty chunk with null data pointer is rejected.
        let null_real = plane(core::ptr::null_mut(), 12, &mut real_chunk);
        assert_eq!(unsafe { chunk_region(&null_real) }, None);

        // Corrupted + empty or zero-sized empty chunks are rejected.
        let mut corrupt_empty = spa_chunk {
            flags: (SPA_CHUNK_FLAG_EMPTY | SPA_CHUNK_FLAG_CORRUPTED) as i32,
            ..empty_chunk
        };
        assert_eq!(
            unsafe { chunk_region(&plane(core::ptr::null_mut(), 12, &mut corrupt_empty)) },
            None
        );
        let mut zero_empty = spa_chunk {
            size: 0,
            ..empty_chunk
        };
        let zero_plane = plane(core::ptr::null_mut(), 12, &mut zero_empty);
        assert_eq!(unsafe { pack_audio_planes([zero_plane].iter()) }, None);
    }

    #[test]
    fn video_crop_validates_bounds_and_packs_cropped_rows() {
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

        // Valid interior crop.
        assert_eq!(resolve_video_crop(frame, mk_crop(1, 1, 2, 2)), (1, 1, 2, 2));
        // Full-frame crop and absent crop both keep full dimensions.
        assert_eq!(resolve_video_crop(frame, mk_crop(0, 0, 4, 3)), (0, 0, 4, 3));
        assert_eq!(resolve_video_crop(frame, None), (0, 0, 4, 3));

        // Malformed, empty, negative, out-of-bounds, and overflowing crops fall back to full frame.
        for bad in [
            mk_crop(0, 0, 0, 2),
            mk_crop(0, 0, 2, 0),
            mk_crop(-1, 0, 2, 2),
            mk_crop(0, -1, 2, 2),
            mk_crop(3, 0, 2, 2),
            mk_crop(0, 2, 2, 2),
            mk_crop(i32::MAX, 0, u32::MAX, 1),
            mk_crop(0, i32::MAX, 1, u32::MAX),
        ] {
            assert_eq!(resolve_video_crop(frame, bad), (0, 0, 4, 3));
        }

        // Pack a 2x2 crop at (1, 1) from a 4x3 BGRA image with 20-byte stride (4 bytes row padding).
        let stride = 20usize;
        let mut buf = vec![0u8; stride * 3];
        for y in 0..3u8 {
            for x in 0..4u8 {
                let off = y as usize * stride + x as usize * 4;
                buf[off..off + 4].copy_from_slice(&[x, y, 0xAA, 0xFF]);
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
        let packed = unsafe {
            pack_rows(
                buf.as_ptr().add(layout.byte_offset),
                layout.pitch,
                layout.row_bytes,
                layout.height as usize,
            )
        }
        .unwrap();
        assert_eq!(
            packed,
            [
                1, 1, 0xAA, 0xFF, 2, 1, 0xAA, 0xFF, // row 1, cols 1..3
                1, 2, 0xAA, 0xFF, 2, 2, 0xAA, 0xFF, // row 2, cols 1..3
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
}
