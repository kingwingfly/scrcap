//! pipewire related

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use crossbeam_channel::Sender;

use parking_lot::{ArcMutexGuard, Mutex, RawMutex};
use pipewire::{
    context::ContextRc,
    keys,
    main_loop::MainLoopRc,
    properties::properties,
    spa::{
        param::{
            ParamType,
            audio::AudioInfoRaw,
            format::{FormatProperties, MediaSubtype, MediaType},
            format_utils::parse_format,
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::{Pod, Value, object, property, serialize::PodSerializer},
        sys::SPA_VIDEO_FORMAT_BGRA,
        utils::{Direction, Fraction, SpaTypes},
    },
    stream::{Stream, StreamFlags, StreamRc},
};

use crate::{
    config::{AudioConfig, VideoConfig},
    frame::Frame,
};

use super::dbus::DbusScreen;

struct VideoData {
    tx: Sender<Frame>,
    format: VideoInfoRaw,
    size_guard: Option<ArcMutexGuard<RawMutex, (u32, u32)>>,
    size: Arc<Mutex<(u32, u32)>>,
    terminate: Arc<AtomicBool>,
    mainloop: MainLoopRc,
}

struct AudioData {
    tx: Sender<Frame>,
    format: AudioInfoRaw,
    sample_rate: Arc<Mutex<i32>>,
}

#[derive(Debug)]
pub struct PipewireSession {
    jh: Option<JoinHandle<()>>,
    pub terminate: Arc<AtomicBool>,
    pub size: Arc<Mutex<(u32, u32)>>,
    pub sample_rate: Option<Arc<Mutex<i32>>>,
}

impl PipewireSession {
    pub fn new(tx: Sender<Frame>, _video: VideoConfig, audio: Option<AudioConfig>) -> Self {
        let size = Arc::new(Mutex::new((0, 0)));
        let sample_rate = audio.as_ref().map(|_audio| Arc::new(Mutex::new(0)));
        let terminate = Arc::new(AtomicBool::new(false));

        let jh = thread::spawn({
            let terminate = terminate.clone();
            let size = size.clone();
            let sample_rate = sample_rate.clone();
            let size_guard = size.lock_arc();
            move || {
                let dbus = DbusScreen::new();
                let node_id = dbus.start();

                let mainloop = MainLoopRc::new(None).unwrap();
                let context = ContextRc::new(&mainloop, None).unwrap();
                let core = context.connect_rc(None).unwrap();

                let video_stream = StreamRc::new(
                    core.clone(),
                    "scrcap-video",
                    properties! {
                        *keys::MEDIA_TYPE => "Video",
                        *keys::MEDIA_CATEGORY => "Capture",
                        *keys::MEDIA_ROLE => "Screen",
                    },
                )
                .unwrap();

                let video_data = VideoData {
                    tx: tx.clone(),
                    format: VideoInfoRaw::new(),
                    size_guard: Some(size_guard),
                    size,
                    terminate,
                    mainloop: mainloop.clone(),
                };

                let _video_listener = video_stream
                    .add_local_listener_with_user_data(video_data)
                    .param_changed(video_param_change_callback)
                    .process(video_process_callback)
                    .register()
                    .unwrap();

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
                        FormatProperties::VideoMaxFramerate, Fraction, Fraction { num: 60, denom: 1 }
                    }
                };
                let values: Vec<u8> =
                    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
                        .unwrap()
                        .0
                        .into_inner();
                let mut params = [Pod::from_bytes(&values).unwrap()];
                video_stream
                    .connect(
                        Direction::Input,
                        Some(node_id),
                        StreamFlags::AUTOCONNECT
                            | StreamFlags::MAP_BUFFERS
                            | StreamFlags::RT_PROCESS,
                        &mut params,
                    )
                    .unwrap();

                if let Some(_audio) = audio {
                    let audio_stream = StreamRc::new(
                        core.clone(),
                        "scrcap-audio",
                        properties! {
                            *keys::MEDIA_TYPE => "Audio",
                            *keys::MEDIA_CATEGORY => "Capture",
                            *keys::MEDIA_ROLE => "Music",
                            *keys::STREAM_CAPTURE_SINK => "true",
                        },
                    )
                    .unwrap();
                    let audio_data = AudioData {
                        tx,
                        format: AudioInfoRaw::new(),
                        sample_rate: sample_rate.unwrap(),
                    };
                    let _audio_listener = audio_stream
                        .add_local_listener_with_user_data(audio_data)
                        .param_changed(audio_param_change_callback)
                        .process(audio_process_callback)
                        .register()
                        .unwrap();
                    let obj = object! {
                        SpaTypes::ObjectParamFormat,
                        ParamType::EnumFormat,
                        property! {
                            FormatProperties::MediaType, Id, MediaType::Audio
                        },
                        property! {
                            FormatProperties::MediaSubtype, Id, MediaSubtype::Raw
                        },
                    };
                    let values: Vec<u8> = PodSerializer::serialize(
                        std::io::Cursor::new(Vec::new()),
                        &Value::Object(obj),
                    )
                    .unwrap()
                    .0
                    .into_inner();
                    let mut params = [Pod::from_bytes(&values).unwrap()];
                    audio_stream
                        .connect(
                            Direction::Input,
                            Some(node_id),
                            StreamFlags::AUTOCONNECT
                                | StreamFlags::MAP_BUFFERS
                                | StreamFlags::RT_PROCESS,
                            &mut params,
                        )
                        .unwrap();
                    mainloop.run();
                } else {
                    mainloop.run();
                }
            }
        });

        let _ = *size.lock();

        PipewireSession {
            jh: Some(jh),
            size,
            terminate,
            sample_rate,
        }
    }

    pub fn size(&self) -> (u32, u32) {
        *self.size.lock()
    }

    pub fn sample_rate(&self) -> Option<i32> {
        self.sample_rate
            .as_ref()
            .map(|sample_rate| *sample_rate.lock())
    }
}

fn video_process_callback(stream: &Stream, data: &mut VideoData) {
    if data.terminate.load(Ordering::Relaxed) {
        data.mainloop.quit();
        return;
    }
    let buffer = unsafe { stream.dequeue_raw_buffer() };
    if buffer.is_null() || unsafe { (*buffer).buffer.is_null() } {
        return;
    }
    unsafe {
        let buffer = &*(*buffer).buffer;
        if buffer.n_datas < 1 {
            return;
        }
        let vframe = {
            let data = buffer.datas; // first elem
            std::slice::from_raw_parts((*data).data as *const u8, (*(*data).chunk).size as usize)
                .to_vec()
        };
        let size = data.format.size();
        let _ = data.tx.try_send(Frame::Video {
            vframe,
            size: (size.width, size.height),
            pix_fmt: 28, // AV_PIX_FMT_BGRA
        });
    }
    unsafe { stream.queue_raw_buffer(buffer) };
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
    data.format.parse(param).unwrap();
    let size = data.format.size();
    if let Some(mut size_guard) = data.size_guard.take() {
        *size_guard = (size.width, size.height);
    } else {
        *data.size.lock() = (size.width, size.height);
    }
}

fn audio_process_callback(stream: &Stream, data: &mut AudioData) {
    let buffer = unsafe { stream.dequeue_raw_buffer() };
    if buffer.is_null() || unsafe { (*buffer).buffer.is_null() } {
        return;
    }
    unsafe {
        let buffer = &*(*buffer).buffer;
        if buffer.n_datas < 1 {
            return;
        }
        let sample_rate = data.format.rate();
        let nb_channels = data.format.channels();
        let nb_samples = (*(*buffer.datas).chunk).size as usize / size_of::<f32>();
        let mut aframe = Vec::with_capacity(nb_samples * nb_channels as usize * size_of::<f32>());
        for i in 0..buffer.n_datas as usize {
            let data = buffer.datas.add(i);
            aframe.extend_from_slice(core::slice::from_raw_parts(
                (*data).data as *const u8,
                (*(*data).chunk).size as usize,
            ));
        }
        let _ = data.tx.send(Frame::Audio {
            aframe,
            nb_samples: nb_samples as i32,
            sample_rate: sample_rate as i32,
            nb_channels: nb_channels as i32,
            sample_fmt: 8, // F32P AV_SAMPLE_FMT_FLTP
        });
    }
    unsafe { stream.queue_raw_buffer(buffer) };
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
    data.format.parse(param).unwrap();
    *data.sample_rate.lock() = data.format.rate() as i32;
}

impl Drop for PipewireSession {
    fn drop(&mut self) {
        self.terminate.store(true, Ordering::Relaxed);
        if let Some(jh) = self.jh.take() {
            let _ = jh.join();
        }
    }
}
