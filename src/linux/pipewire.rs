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
    buffer::Buffer,
    channel::Sender as PwSender,
    context::ContextRc,
    core::{self as pw_core, PW_ID_CORE},
    keys,
    main_loop::MainLoopRc,
    properties::properties,
    registry,
    spa::{
        buffer::meta::MetaHeader,
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
            SPA_CHUNK_FLAG_CORRUPTED, SPA_META_Header, SPA_PARAM_META_size, SPA_PARAM_META_type,
            SPA_PARAM_Meta, SPA_TYPE_OBJECT_ParamMeta, spa_data, spa_meta_header,
        },
        utils::{Direction, Fraction, Rectangle, SpaTypes},
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
    format: Option<(Rectangle, PixFmt)>,
    size: Arc<Mutex<(u32, u32)>>,
    /// Answered from the format callback, so `new` only returns once the size is real, or
    /// from the state callback if the stream fails before that.
    setup_tx: Option<Sender<Result<()>>>,
}

struct AudioData {
    /// Dropped when the stream ends, which closes the channel a consumer is waiting on.
    tx: Option<Sender<AudioFrame>>,
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

        let jh = thread::spawn({
            let size = size.clone();
            let sample_rate = sample_rate.clone();
            move || {
                let run = || -> Result<()> {
                    let dbus = DbusScreen::new()?;
                    let session = dbus.start(source_types)?;
                    let mainloop = MainLoopRc::new(None)?;
                    let context = ContextRc::new(&mainloop, None)?;
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
                        format: None,
                        size,
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
                    let meta = meta_header_param()?;
                    let mut params = [
                        Pod::from_bytes(&values).ok_or(CaptureError::SpaParams)?,
                        Pod::from_bytes(&meta).ok_or(CaptureError::SpaParams)?,
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

                    let _audio = if let Some(a_tx) = a_tx {
                        // Desktop audio is not a screencast node, so it needs the ordinary
                        // connection rather than the portal's remote. A sandbox may grant only
                        // the latter; that is audio being unavailable, not the capture failing,
                        // so say which so the caller can retry without it.
                        let audio_core = context.connect_rc(None).map_err(|e| {
                            error!("no PipeWire connection for audio: {e}");
                            Unsupported::Audio
                        })?;
                        let audio_stream = StreamRc::new(
                            audio_core.clone(),
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
                        let meta = meta_header_param()?;
                        let mut params = [
                            Pod::from_bytes(&values).ok_or(CaptureError::SpaParams)?,
                            Pod::from_bytes(&meta).ok_or(CaptureError::SpaParams)?,
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
        });

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

/// Ask the producer to attach an `SPA_META_Header` to every buffer, which is where the
/// capture timestamp lives.
fn meta_header_param() -> Result<Vec<u8>> {
    let obj = Object {
        type_: SPA_TYPE_OBJECT_ParamMeta,
        id: SPA_PARAM_Meta,
        properties: vec![
            Property {
                key: SPA_PARAM_META_type,
                flags: PropertyFlags::empty(),
                value: Value::Id(pipewire::spa::utils::Id(SPA_META_Header)),
            },
            Property {
                key: SPA_PARAM_META_size,
                flags: PropertyFlags::empty(),
                value: Value::Int(size_of::<spa_meta_header>() as i32),
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

/// The buffer's presentation timestamp in nanoseconds.
///
/// Falls back to `CLOCK_MONOTONIC` -- the domain spa's `pts` is on -- if the producer
/// attached no header despite [`meta_header_param`] asking for one.
fn buffer_ts(buffer: &Buffer<'_>) -> u64 {
    if let Some(header) = buffer.find_meta::<MetaHeader>()
        && let Ok(pts) = u64::try_from(header.pts())
        && pts != 0
    {
        return pts;
    }
    unsafe {
        let mut now = core::mem::zeroed::<libc::timespec>();
        if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) != 0 {
            return 0;
        }
        now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
    }
}

/// Start, contiguous length and stride of a plane in a dequeued buffer.
///
/// SPA defines `offset` modulo `maxsize` and only guarantees `size` up to the end of the
/// mapped allocation, so a producer may hand over an offset past it. Returns `None` for an
/// unmapped, empty or corrupt chunk.
unsafe fn chunk_region(plane: &spa_data) -> Option<(*const u8, usize, i32)> {
    let chunk = unsafe { plane.chunk.as_ref() }?;
    let maxsize = plane.maxsize as usize;
    if plane.data.is_null() || maxsize == 0 || chunk.size == 0 {
        return None;
    }
    if chunk.flags & SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 {
        return None;
    }
    let offset = chunk.offset as usize % maxsize;
    let avail = (maxsize - offset).min(chunk.size as usize);
    Some((
        unsafe { (plane.data as *const u8).add(offset) },
        avail,
        chunk.stride,
    ))
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
    let ts = buffer_ts(&buffer);
    let Some(plane) = buffer.datas_mut().first() else {
        return;
    };
    let Some((base, avail, stride)) = (unsafe { chunk_region(plane.as_raw()) }) else {
        return;
    };
    let Some(row_bytes) = (size.width as usize).checked_mul(pix_fmt.bytes_per_pixel()) else {
        return;
    };
    let Ok(stride) = usize::try_from(stride) else {
        return;
    };
    let pitch = if stride == 0 { row_bytes } else { stride };
    let height = size.height as usize;
    // An unusable buffer must not spend the next frame's slot.
    if row_span(pitch, row_bytes, height).is_none_or(|span| span > avail) || !data.gate.allow(ts) {
        return;
    }
    let Some(vframe) = (unsafe { pack_rows(base, pitch, row_bytes, height) }) else {
        return;
    };
    let _ = data.tx.try_send(VideoFrame {
        vframe,
        size: (size.width, size.height),
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
    let ts = buffer_ts(&buffer);
    let channels = format.channels() as usize;
    let Some(planes) = buffer.datas_mut().get(..channels) else {
        return;
    };
    let Some((aframe, nb_samples)) =
        (unsafe { pack_audio_planes(planes.iter().map(|p| p.as_raw())) })
    else {
        return;
    };
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
/// Each plane and its chunk must remain mapped and readable throughout the copy.
unsafe fn pack_audio_planes<'a>(
    planes: impl ExactSizeIterator<Item = &'a spa_data> + Clone,
) -> Option<(Vec<u8>, i32)> {
    const SAMPLE_BYTES: usize = size_of::<f32>();
    let channels = planes.len();
    let layout = planes.map(|plane| {
        let (base, avail, stride) = unsafe { chunk_region(plane) }?;
        let pitch = if stride == 0 {
            SAMPLE_BYTES
        } else {
            usize::try_from(stride).ok()?
        };
        if pitch < SAMPLE_BYTES {
            return None;
        }
        let samples = 1 + avail.checked_sub(SAMPLE_BYTES)? / pitch;
        Some((base, pitch, samples))
    });
    let samples = layout.clone().map(|p| p.map_or(0, |(_, _, n)| n)).min()?;
    if samples == 0 {
        return None;
    }
    let total = samples.checked_mul(SAMPLE_BYTES)?.checked_mul(channels)?;
    if total > isize::MAX as usize {
        return None;
    }
    let mut frame = Vec::with_capacity(total);
    for plane in layout {
        let (base, pitch, _) = plane?;
        unsafe { append_rows(&mut frame, base, pitch, SAMPLE_BYTES, samples)? };
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
}
