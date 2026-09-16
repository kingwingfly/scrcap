//! pipewire related

use std::{
    fmt,
    sync::Arc,
    thread::{self, JoinHandle},
};

use crossbeam_channel::Sender;
use tracing::error;

use parking_lot::Mutex;
use pipewire::{
    channel::Sender as PwSender,
    context::ContextRc,
    keys,
    main_loop::MainLoopRc,
    properties::properties,
    spa::{
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
            SPA_PARAM_Meta, SPA_TYPE_OBJECT_ParamMeta, spa_buffer, spa_chunk, spa_data,
            spa_meta_header,
        },
        utils::{Direction, Fraction, SpaTypes},
    },
    stream::{Stream, StreamFlags, StreamRc, StreamState},
};

use crate::{
    config::{AudioConfig, Target, VideoConfig},
    error::{CaptureError, Result, Unsupported},
    format::{PixFmt, SampleFmt},
    fps::FpsGate,
    frame::{AudioFrame, VideoFrame},
    util::pack_rows,
};

use super::dbus::{DbusScreen, SOURCE_TYPE_MONITOR};

struct Terminate;

struct VideoData {
    tx: Sender<VideoFrame>,
    mainloop: MainLoopRc,
    gate: FpsGate,
    format: VideoInfoRaw,
    /// What the negotiated `format` is called here; only meaningful once it is set.
    pix_fmt: PixFmt,
    size: Arc<Mutex<(u32, u32)>>,
    /// Answered from the format callback, so `new` only returns once the size is real, or
    /// from the state callback if the stream fails before that.
    setup_tx: Option<Sender<Result<()>>>,
}

struct AudioData {
    tx: Sender<AudioFrame>,
    format: AudioInfoRaw,
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
        let max_framerate = fps.filter(|fps| *fps > 0).unwrap_or(1000);
        let size = Arc::new(Mutex::new((0, 0)));
        let sample_rate = audio.as_ref().map(|_audio| Arc::new(Mutex::new(None)));
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        let (setup_tx, setup_rx) = crossbeam_channel::bounded::<Result<()>>(1);

        let jh = thread::spawn({
            let size = size.clone();
            let sample_rate = sample_rate.clone();
            let setup_tx = setup_tx.clone();
            move || {
                let run = || -> Result<()> {
                    let dbus = DbusScreen::new()?;
                    let session = dbus.start(source_types)?;
                    let node_id = session.node_id;

                    let mainloop = MainLoopRc::new(None)?;
                    let context = ContextRc::new(&mainloop, None)?;
                    // The screencast node is only reachable on the remote the portal
                    // granted; under a sandbox the default socket does not have it.
                    let core = context.connect_fd_rc(session.fd, None)?;

                    let _quit_rx = quit_rx.attach(mainloop.loop_(), {
                        let mainloop = mainloop.clone();
                        move |_| mainloop.quit()
                    });

                    let video_stream = StreamRc::new(
                        core.clone(),
                        "scrcap-video",
                        properties! {
                            *keys::MEDIA_TYPE => "Video",
                            *keys::MEDIA_CATEGORY => "Capture",
                            *keys::MEDIA_ROLE => "Screen",
                        },
                    )?;

                    let video_data = VideoData {
                        tx: v_tx,
                        mainloop: mainloop.clone(),
                        gate: FpsGate::new(fps),
                        format: VideoInfoRaw::new(),
                        pix_fmt: PixFmt::Bgra,
                        size,
                        setup_tx: Some(setup_tx.clone()),
                    };

                    let _video_listener = video_stream
                        .add_local_listener_with_user_data(video_data)
                        .param_changed(video_param_change_callback)
                        .process(video_process_callback)
                        .state_changed(video_state_change_callback)
                        .register()?;

                    let obj = object! {
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
                        },
                        property! {
                            FormatProperties::VideoMaxFramerate,
                            Choice, Range, Fraction,
                            Fraction { num: max_framerate, denom: 1 },
                            Fraction { num: 0, denom: 1 },
                            Fraction { num: max_framerate, denom: 1 }
                        }
                    };
                    let values: Vec<u8> = PodSerializer::serialize(
                        std::io::Cursor::new(Vec::new()),
                        &Value::Object(obj),
                    )
                    .map_err(|e| {
                        error!("failed to serialize video format: {e}");
                        CaptureError::SpaParams
                    })?
                    .0
                    .into_inner();
                    let meta = meta_header_param()?;
                    let mut params = [
                        Pod::from_bytes(&values).ok_or_else(|| CaptureError::SpaParams)?,
                        Pod::from_bytes(&meta).ok_or(CaptureError::SpaParams)?,
                    ];
                    video_stream.connect(
                        Direction::Input,
                        Some(node_id),
                        StreamFlags::AUTOCONNECT
                            | StreamFlags::MAP_BUFFERS
                            | StreamFlags::RT_PROCESS,
                        &mut params,
                    )?;

                    let _audio = if let Some(a_tx) = a_tx {
                        // Desktop audio is not a screencast node, so it needs the ordinary
                        // connection rather than the portal's remote.
                        let audio_core = context.connect_rc(None)?;
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
                            tx: a_tx,
                            format: AudioInfoRaw::new(),
                            sample_rate: sample_rate.expect("sample_rate exists when audio is on"),
                        };
                        let audio_listener = audio_stream
                            .add_local_listener_with_user_data(audio_data)
                            .param_changed(audio_param_change_callback)
                            .process(audio_process_callback)
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
                        let values: Vec<u8> = PodSerializer::serialize(
                            std::io::Cursor::new(Vec::new()),
                            &Value::Object(obj),
                        )
                        .map_err(|e| {
                            error!("failed to serialize audio format: {e}");
                            CaptureError::SpaParams
                        })?
                        .0
                        .into_inner();
                        let meta = meta_header_param()?;
                        let mut params = [
                            Pod::from_bytes(&values).ok_or_else(|| CaptureError::SpaParams)?,
                            Pod::from_bytes(&meta).ok_or(CaptureError::SpaParams)?,
                        ];
                        // No target: `node_id` is the portal's *video* node, and an audio
                        // stream aimed at it can never link. `STREAM_CAPTURE_SINK` above is
                        // what tells the session manager to pick the default sink monitor,
                        // and an explicit target would override it.
                        audio_stream.connect(
                            Direction::Input,
                            None,
                            StreamFlags::AUTOCONNECT
                                | StreamFlags::MAP_BUFFERS
                                | StreamFlags::RT_PROCESS,
                            &mut params,
                        )?;
                        // Listener first: tuple fields drop in order, and a listener
                        // dropped after its stream unlinks its hook from freed memory.
                        Some((audio_listener, audio_stream))
                    } else {
                        None
                    };

                    mainloop.run();
                    Ok(())
                };

                if let Err(e) = run() {
                    let _ = setup_tx.try_send(Err(e));
                }
            }
        });
        drop(setup_tx);

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
    Ok(
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
            .map_err(|e| {
                error!("failed to serialize meta param: {e}");
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
unsafe fn buffer_ts(buffer: *const spa_buffer) -> u64 {
    unsafe {
        let header = pipewire::spa::sys::spa_buffer_find_meta_data(
            buffer as *mut _,
            SPA_META_Header,
            size_of::<spa_meta_header>(),
        ) as *const spa_meta_header;
        if !header.is_null()
            && let Ok(pts) = u64::try_from((*header).pts)
            && pts != 0
        {
            return pts;
        }
        let mut now = core::mem::zeroed::<libc::timespec>();
        if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) != 0 {
            return 0;
        }
        now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
    }
}

/// Start of a chunk's valid data and how many bytes of it are actually readable.
///
/// SPA defines `offset` modulo `maxsize` and only guarantees `size` up to the end of the
/// mapped allocation, so a producer may hand over an offset past it. Returns `None` for an
/// unmapped, empty or corrupt chunk.
unsafe fn chunk_slice(plane: spa_data, chunk: &spa_chunk) -> Option<(*const u8, usize)> {
    let maxsize = plane.maxsize as usize;
    if plane.data.is_null() || maxsize == 0 || chunk.size == 0 {
        return None;
    }
    if chunk.flags & SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 {
        return None;
    }
    let offset = chunk.offset as usize % maxsize;
    let avail = (maxsize - offset).min(chunk.size as usize);
    Some((unsafe { (plane.data as *const u8).add(offset) }, avail))
}

fn video_process_callback(stream: &Stream, data: &mut VideoData) {
    let buffer = unsafe { stream.dequeue_raw_buffer() };
    if buffer.is_null() {
        return;
    }
    unsafe {
        if !(*buffer).buffer.is_null() {
            let spa_buffer = &*(*buffer).buffer;
            let ts = buffer_ts(spa_buffer);
            if spa_buffer.n_datas >= 1 {
                let size = data.format.size();
                let plane = spa_buffer.datas; // first elem
                let chunk = &*(*plane).chunk;
                let row_bytes = size.width as usize * 4;
                let stride = if chunk.stride > 0 {
                    chunk.stride as usize
                } else {
                    row_bytes
                };
                let height = size.height as usize;
                let want = stride.saturating_mul(height);
                // Validate before the gate: an unusable buffer must not spend the slot
                // the next real frame needs.
                let usable = chunk_slice(*plane, chunk)
                    .filter(|&(_, avail)| want != 0 && avail >= want && stride >= row_bytes);
                let Some((base, _)) = usable else {
                    stream.queue_raw_buffer(buffer);
                    return;
                };
                if !data.gate.allow(ts) {
                    stream.queue_raw_buffer(buffer);
                    return;
                }
                let Some(vframe) = pack_rows(base, stride, row_bytes, height) else {
                    stream.queue_raw_buffer(buffer);
                    return;
                };
                let _ = data.tx.try_send(VideoFrame {
                    vframe,
                    size: (size.width, size.height),
                    pix_fmt: data.pix_fmt,
                    ts,
                });
            }
        }
        stream.queue_raw_buffer(buffer);
    }
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
    let Some(param) = param else {
        return;
    };
    if id != ParamType::Format.as_raw() {
        return;
    }
    let Ok((MediaType::Video, MediaSubtype::Raw)) = parse_format(param) else {
        return;
    };
    if data.format.parse(param).is_err() {
        return;
    }
    data.pix_fmt = match data.format.format() {
        VideoFormat::BGRx => PixFmt::Bgr0,
        _ => PixFmt::Bgra,
    };
    let size = data.format.size();
    *data.size.lock() = (size.width, size.height);
    if let Some(setup_tx) = data.setup_tx.take() {
        let _ = setup_tx.try_send(Ok(()));
    }
}

fn audio_process_callback(stream: &Stream, data: &mut AudioData) {
    let buffer = unsafe { stream.dequeue_raw_buffer() };
    if buffer.is_null() {
        return;
    }
    unsafe {
        if !(*buffer).buffer.is_null() {
            let spa_buffer = &*(*buffer).buffer;
            if spa_buffer.n_datas >= 1 {
                let sample_rate = data.format.rate();
                let nb_channels = data.format.channels();
                if sample_rate == 0 || nb_channels == 0 {
                    stream.queue_raw_buffer(buffer);
                    return;
                }
                let n_datas = spa_buffer.n_datas as usize;
                // One plane per channel, all the same length, so the shortest readable one
                // sets the frame's sample count rather than misaligning the planes after it.
                let plane_bytes = (0..n_datas)
                    .map(|i| {
                        let plane = spa_buffer.datas.add(i);
                        chunk_slice(*plane, &*(*plane).chunk).map_or(0, |(_, avail)| avail)
                    })
                    .min()
                    .unwrap_or(0);
                let nb_samples = plane_bytes / size_of::<f32>();
                if nb_samples > 0 {
                    let plane_bytes = nb_samples * size_of::<f32>();
                    let mut aframe = Vec::with_capacity(plane_bytes * n_datas);
                    for i in 0..n_datas {
                        let plane = spa_buffer.datas.add(i);
                        if let Some((base, _)) = chunk_slice(*plane, &*(*plane).chunk) {
                            aframe
                                .extend_from_slice(core::slice::from_raw_parts(base, plane_bytes));
                        }
                    }
                    let _ = data.tx.try_send(AudioFrame {
                        aframe,
                        nb_samples: nb_samples as i32,
                        sample_rate: sample_rate as i32,
                        nb_channels: nb_channels as i32,
                        sample_fmt: SampleFmt::F32P,
                        ts: buffer_ts(spa_buffer),
                    });
                }
            }
        }
        stream.queue_raw_buffer(buffer);
    }
}

fn audio_param_change_callback(
    _stream: &Stream,
    data: &mut AudioData,
    id: u32,
    param: Option<&Pod>,
) {
    let Some(param) = param else {
        return;
    };
    if id != ParamType::Format.as_raw() {
        return;
    }
    let Ok((MediaType::Audio, MediaSubtype::Raw)) = parse_format(param) else {
        return;
    };
    if data.format.parse(param).is_err() {
        return;
    }
    let rate = data.format.rate() as i32;
    if rate > 0 {
        *data.sample_rate.lock() = Some(rate);
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
