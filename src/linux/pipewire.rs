//! pipewire related

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use crossbeam_channel::Sender;

use parking_lot::{ArcMutexGuard, Mutex, RawMutex};
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
            SPA_META_Header, SPA_PARAM_META_size, SPA_PARAM_META_type, SPA_PARAM_Meta,
            SPA_TYPE_OBJECT_ParamMeta, SPA_VIDEO_FORMAT_BGRA, spa_buffer, spa_meta_header,
        },
        utils::{Direction, Fraction, SpaTypes},
    },
    stream::{Stream, StreamFlags, StreamRc},
};

use crate::{
    config::{AudioConfig, Target, VideoConfig},
    error::{CaptureError, Result},
    format::{PixFmt, SampleFmt},
    fps::FpsGate,
    frame::{AudioFrame, VideoFrame},
};

use super::dbus::{DbusScreen, SOURCE_TYPE_MONITOR};

struct Terminate;

struct VideoData {
    tx: Sender<VideoFrame>,
    gate: FpsGate,
    format: VideoInfoRaw,
    size_guard: Option<ArcMutexGuard<RawMutex, (u32, u32)>>,
    size: Arc<Mutex<(u32, u32)>>,
}

struct AudioData {
    tx: Sender<AudioFrame>,
    format: AudioInfoRaw,
    sample_rate: Arc<Mutex<i32>>,
}

pub struct PipewireSession {
    jh: Option<JoinHandle<()>>,
    quit_tx: PwSender<Terminate>,
    pub terminate: Arc<AtomicBool>,
    pub size: Arc<Mutex<(u32, u32)>>,
    pub sample_rate: Option<Arc<Mutex<i32>>>,
}

impl fmt::Debug for PipewireSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipewireSession")
            .field("terminate", &self.terminate)
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
        let source_types = match video.target {
            Target::Pick => 0, // every available source type
            Target::Primary | Target::Monitor(_) => SOURCE_TYPE_MONITOR,
            Target::Window(_) | Target::WindowName(_) => return Err(CaptureError::Unsupported),
        };

        let fps = video.fps;
        let max_framerate = fps.filter(|fps| *fps > 0).unwrap_or(1000);
        let size = Arc::new(Mutex::new((0, 0)));
        let sample_rate = audio.as_ref().map(|_audio| Arc::new(Mutex::new(0)));
        let terminate = Arc::new(AtomicBool::new(false));
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        let (setup_tx, setup_rx) = crossbeam_channel::bounded::<Result<()>>(1);

        let jh = thread::spawn({
            let terminate = terminate.clone();
            let size = size.clone();
            let sample_rate = sample_rate.clone();
            let size_guard = size.lock_arc();
            let setup_tx = setup_tx.clone();
            move || {
                let run = || -> Result<()> {
                    let dbus = DbusScreen::new()?;
                    let node_id = dbus.start(source_types)?;

                    let mainloop = MainLoopRc::new(None)?;
                    let context = ContextRc::new(&mainloop, None)?;
                    let core = context.connect_rc(None)?;

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
                        gate: FpsGate::new(fps),
                        format: VideoInfoRaw::new(),
                        size_guard: Some(size_guard),
                        size,
                    };

                    let _video_listener = video_stream
                        .add_local_listener_with_user_data(video_data)
                        .param_changed(video_param_change_callback)
                        .process(video_process_callback)
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
                        property! {
                            FormatProperties::VideoFormat, Id, VideoFormat(SPA_VIDEO_FORMAT_BGRA)
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
                        CaptureError::Portal(format!("failed to serialize video format: {e}"))
                    })?
                    .0
                    .into_inner();
                    let meta = meta_header_param()?;
                    let mut params = [
                        Pod::from_bytes(&values).ok_or_else(|| {
                            CaptureError::Portal("invalid video format pod".into())
                        })?,
                        Pod::from_bytes(&meta)
                            .ok_or_else(|| CaptureError::Portal("invalid meta pod".into()))?,
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
                        let audio_stream = StreamRc::new(
                            core.clone(),
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
                            CaptureError::Portal(format!("failed to serialize audio format: {e}"))
                        })?
                        .0
                        .into_inner();
                        let meta = meta_header_param()?;
                        let mut params = [
                            Pod::from_bytes(&values).ok_or_else(|| {
                                CaptureError::Portal("invalid audio format pod".into())
                            })?,
                            Pod::from_bytes(&meta)
                                .ok_or_else(|| CaptureError::Portal("invalid meta pod".into()))?,
                        ];
                        audio_stream.connect(
                            Direction::Input,
                            Some(node_id),
                            StreamFlags::AUTOCONNECT
                                | StreamFlags::MAP_BUFFERS
                                | StreamFlags::RT_PROCESS,
                            &mut params,
                        )?;
                        Some((audio_stream, audio_listener))
                    } else {
                        None
                    };

                    let _ = setup_tx.send(Ok(()));
                    mainloop.run();
                    Ok(())
                };

                if let Err(e) = run() {
                    let _ = setup_tx.send(Err(e));
                }
                terminate.store(true, Ordering::Relaxed);
            }
        });
        drop(setup_tx);

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

        let _ = *size.lock();

        Ok(PipewireSession {
            jh: Some(jh),
            quit_tx,
            size,
            terminate,
            sample_rate,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        *self.size.lock()
    }

    pub fn sample_rate(&self) -> Option<i32> {
        self.sample_rate
            .as_ref()
            .map(|sample_rate| *sample_rate.lock())
    }

    pub fn terminate(&self) {
        self.terminate.store(true, Ordering::Relaxed);
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
            .map_err(|e| CaptureError::Portal(format!("failed to serialize meta param: {e}")))?
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

fn video_process_callback(stream: &Stream, data: &mut VideoData) {
    let buffer = unsafe { stream.dequeue_raw_buffer() };
    if buffer.is_null() {
        return;
    }
    unsafe {
        if !(*buffer).buffer.is_null() {
            let spa_buffer = &*(*buffer).buffer;
            let ts = buffer_ts(spa_buffer);
            if spa_buffer.n_datas >= 1 && data.gate.allow(ts) {
                let vframe = {
                    let data = spa_buffer.datas; // first elem
                    std::slice::from_raw_parts(
                        (*data).data as *const u8,
                        (*(*data).chunk).size as usize,
                    )
                    .to_vec()
                };
                let size = data.format.size();
                let _ = data.tx.try_send(VideoFrame {
                    vframe,
                    size: (size.width, size.height),
                    pix_fmt: PixFmt::Bgra,
                    ts,
                });
            }
        }
        stream.queue_raw_buffer(buffer);
    }
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
    let size = data.format.size();
    if let Some(mut size_guard) = data.size_guard.take() {
        *size_guard = (size.width, size.height);
    } else {
        *data.size.lock() = (size.width, size.height);
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
                let nb_samples = (*(*spa_buffer.datas).chunk).size as usize / size_of::<f32>();
                let mut aframe =
                    Vec::with_capacity(nb_samples * nb_channels as usize * size_of::<f32>());
                for i in 0..spa_buffer.n_datas as usize {
                    let plane = spa_buffer.datas.add(i);
                    aframe.extend_from_slice(core::slice::from_raw_parts(
                        (*plane).data as *const u8,
                        (*(*plane).chunk).size as usize,
                    ));
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
    *data.sample_rate.lock() = data.format.rate() as i32;
}

impl Drop for PipewireSession {
    fn drop(&mut self) {
        self.terminate();
        if let Some(jh) = self.jh.take() {
            let _ = jh.join();
        }
    }
}
