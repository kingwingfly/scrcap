use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};
use parking_lot::Mutex;
use windows::{
    Foundation::{Metadata::ApiInformation, TimeSpan, TypedEventHandler},
    Graphics::{
        Capture::{
            Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCapturePicker,
            GraphicsCaptureSession,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::{FALSE, HMODULE, HWND, LPARAM, POINT, RECT, TRUE},
        Graphics::{
            Direct3D::{
                self, D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
            },
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11Texture2D,
            },
            Dxgi::IDXGIDevice,
            Gdi::{EnumDisplayMonitors, HDC, HMONITOR, MONITOR_DEFAULTTONULL, MonitorFromPoint},
        },
        Media::{
            Audio::{
                AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
                IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
                WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
            },
            KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE},
            Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
        },
        System::{
            Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
                CoUninitialize,
            },
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            Threading::GetCurrentThreadId,
            WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
            },
        },
        UI::Shell::IInitializeWithWindow,
        UI::WindowsAndMessaging::{
            DispatchMessageW, EnumWindows, GetMessageW, GetWindowTextW, IsWindowVisible, MSG,
            PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WINDOW_DISPLAY_AFFINITY, WM_NULL,
            WM_USER,
        },
    },
    core::{BOOL, HSTRING, IInspectable, Interface as _, factory},
};

use regex::Regex;
use tracing::error;

use super::utils::{hide_window_from_capture, restore_window_capture_affinity};
use crate::{
    capture_desc::CaptureDescriptor,
    config::{AudioConfig, CaptureConfig, Target, VideoConfig},
    error::{CaptureError, Result, Unsupported},
    format::{PixFmt, SampleFmt},
    fps::FpsGate,
    frame::{AudioFrame, VideoFrame},
    util::pack_rows,
};

impl CaptureConfig {
    /// Create a new capture configuration.
    pub fn create(self) -> Result<CaptureDesc> {
        self.validate()?;
        let (v_tx, v_rx) = bounded(self.video.channel_capacity);
        let terminate = Arc::new(AtomicBool::new(false));
        let video_desc = self.video.create(v_tx, terminate.clone())?;
        let (audio_desc, a_rx) = match self.audio {
            Some(audio_config) => {
                let (a_tx, a_rx) = bounded(audio_config.channel_capacity);
                (
                    Some(audio_config.create(a_tx, terminate.clone())?),
                    Some(a_rx),
                )
            }
            None => (None, None),
        };

        Ok(CaptureDesc {
            terminate,
            v_rx: Some(v_rx),
            a_rx,
            video_desc,
            audio_desc,
        })
    }
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = unsafe { &mut *(lparam.0 as *mut Vec<HMONITOR>) };
    monitors.push(monitor);
    TRUE
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        if IsWindowVisible(hwnd).as_bool() {
            let mut title = [0u16; 512];
            let len = GetWindowTextW(hwnd, &mut title) as usize;
            if len > 0 {
                let found = &mut *(lparam.0 as *mut (Regex, Option<HWND>));
                if found.0.is_match(&String::from_utf16_lossy(&title[..len])) {
                    found.1 = Some(hwnd);
                    return FALSE;
                }
            }
        }
    }
    TRUE
}

fn window_by_title(pattern: &str) -> Result<HWND> {
    let re = Regex::new(pattern).map_err(CaptureError::BadWindowRegex)?;
    let mut found = (re, None);
    unsafe {
        let _ = EnumWindows(Some(collect_window), LPARAM(&mut found as *mut _ as isize));
    }
    found.1.ok_or(CaptureError::TargetNotFound)
}

fn monitor_from_index(index: isize) -> Result<HMONITOR> {
    if index < 0 {
        return Err(CaptureError::TargetNotFound);
    }
    let mut monitors: Vec<HMONITOR> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut monitors as *mut _ as isize),
        )
        .ok()?;
    }
    monitors
        .get(index as usize)
        .copied()
        .ok_or(CaptureError::TargetNotFound)
}

fn capture_item(target: Target) -> Result<GraphicsCaptureItem> {
    let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    Ok(match target {
        Target::Primary => {
            let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTONULL) };
            if monitor.is_invalid() {
                return Err(CaptureError::TargetNotFound);
            }
            unsafe { interop.CreateForMonitor(monitor)? }
        }
        Target::Monitor(index) => {
            let monitor = monitor_from_index(index)?;
            unsafe { interop.CreateForMonitor(monitor)? }
        }
        Target::Window(hwnd) => unsafe { interop.CreateForWindow(HWND(hwnd as _))? },
        Target::WindowName(pattern) => {
            let hwnd = window_by_title(&pattern)?;
            unsafe { interop.CreateForWindow(hwnd)? }
        }
        Target::Pick(parent) => {
            let picker = GraphicsCapturePicker::new()?;
            let init: IInitializeWithWindow = picker.cast()?;
            unsafe { init.Initialize(HWND(parent as _))? };
            // Dismissing yields a null item, which windows-rs reports as a success-coded error.
            match picker.PickSingleItemAsync()?.join() {
                Ok(item) => item,
                Err(e) if e.code().is_ok() => return Err(CaptureError::Cancelled),
                Err(e) => return Err(CaptureError::Win(e)),
            }
        }
    })
}

/// The WinRT wrapper `Direct3D11CaptureFramePool` takes around a D3D11 device. Built again
/// where needed rather than shared, because it is not `Send`.
fn winrt_device(d3d_device: &ID3D11Device) -> windows::core::Result<IDirect3DDevice> {
    let dxgi_device: IDXGIDevice = d3d_device.cast()?;
    unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)? }.cast()
}

fn restore_hidden(hidden: &[(isize, WINDOW_DISPLAY_AFFINITY)]) {
    for (hwnd, previous) in hidden {
        restore_window_capture_affinity(HWND(*hwnd as _), *previous);
    }
}

impl VideoConfig {
    /// Create a new capture configuration.
    fn create(
        self,
        tx: Sender<VideoFrame>,
        terminate: Arc<AtomicBool>,
    ) -> Result<CaptureVideoDesc> {
        if !GraphicsCaptureSession::IsSupported()? {
            return Err(Unsupported::ScreenCapture.into());
        }
        let fps = self.fps;

        // Before resolving the target: `Pick` would otherwise offer the hidden windows.
        let mut hidden = Vec::with_capacity(self.hide.len());
        for hide in &self.hide {
            let hwnd = HWND(*hide as _);
            match hide_window_from_capture(hwnd) {
                Ok(previous) => hidden.push((*hide, previous)),
                Err(e) => {
                    restore_hidden(&hidden);
                    return Err(e);
                }
            }
        }

        let item = match capture_item(self.target) {
            Ok(item) => item,
            Err(e) => {
                restore_hidden(&hidden);
                return Err(e);
            }
        };

        let item_size = match item.Size() {
            Ok(size) => size,
            Err(e) => {
                restore_hidden(&hidden);
                return Err(e.into());
            }
        };
        let size = Arc::new(Mutex::new((
            item_size.Width as u32,
            item_size.Height as u32,
        )));

        let (setup_tx, setup_rx) = bounded::<Result<u32>>(1);

        let desc_terminate = terminate.clone();
        let jh = thread::spawn(unsafe {
            let size = size.clone();
            let setup_tx = setup_tx.clone();
            move || -> Result<()> {
                let setup =
                    (|| -> Result<(Direct3D11CaptureFramePool, GraphicsCaptureSession, i64)> {
                        const FEATURE_FLAGS: [Direct3D::D3D_FEATURE_LEVEL; 2] =
                            [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
                        let mut d3d_device = None;
                        let mut d3d_device_context = None;
                        D3D11CreateDevice(
                            None,
                            D3D_DRIVER_TYPE_HARDWARE,
                            HMODULE::default(),
                            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                            Some(&FEATURE_FLAGS),
                            D3D11_SDK_VERSION,
                            Some(&mut d3d_device),
                            None,
                            Some(&mut d3d_device_context),
                        )?;
                        let d3d_device = d3d_device.ok_or(Unsupported::Direct3D)?;
                        let d3d_device_context = d3d_device_context.ok_or(Unsupported::Direct3D)?;
                        let device = winrt_device(&d3d_device)?;

                        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                            &device,
                            DirectXPixelFormat::B8G8R8A8UIntNormalized,
                            3,
                            item_size,
                        )?;

                        let session = pool.CreateCaptureSession(&item)?;
                        if let Some(fps) = fps.filter(|fps| *fps > 0)
                            && ApiInformation::IsPropertyPresent(
                                &HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession"),
                                &HSTRING::from("MinUpdateInterval"),
                            )?
                        {
                            session.SetMinUpdateInterval(TimeSpan {
                                Duration: 10_000_000 / fps as i64,
                            })?;
                        }
                        let terminate_c = terminate.clone();
                        let gate = Mutex::new(FpsGate::new(fps));
                        let staging: Mutex<Option<(ID3D11Texture2D, u32, u32)>> = Mutex::new(None);
                        let pool_size = Mutex::new(item_size);
                        let token = pool.FrameArrived(&TypedEventHandler::<
                            Direct3D11CaptureFramePool,
                            IInspectable,
                        >::new(
                            move |pool, _| {
                                if terminate_c.load(Ordering::Relaxed) {
                                    return Ok(());
                                }
                                let Some(pool) = pool.as_ref() else {
                                    return Ok(());
                                };
                                let frame = pool.TryGetNextFrame()?;
                                // The pool keeps handing out textures of the size it was made
                                // with, clipping or padding a source that has since resized.
                                let content = frame.ContentSize()?;
                                let mut pool_size = pool_size.lock();
                                if content.Width > 0 && content.Height > 0 && content != *pool_size
                                {
                                    pool.Recreate(
                                        &winrt_device(&d3d_device)?,
                                        DirectXPixelFormat::B8G8R8A8UIntNormalized,
                                        3,
                                        content,
                                    )?;
                                    *pool_size = content;
                                }
                                drop(pool_size);
                                let ts = frame
                                    .SystemRelativeTime()
                                    .ok()
                                    .and_then(|t| u64::try_from(t.Duration).ok())
                                    .map_or_else(qpc_now, |t| t * 100);
                                if !gate.lock().allow(ts) {
                                    return Ok(());
                                }
                                let surface = frame.Surface()?;
                                let interface = surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
                                let texture = interface.GetInterface::<ID3D11Texture2D>()?;
                                let mut desc = D3D11_TEXTURE2D_DESC::default();
                                texture.GetDesc(&mut desc);

                                let mut staging = staging.lock();
                                let cpu_texture = match staging.as_ref() {
                                    Some((texture, w, h))
                                        if *w == desc.Width && *h == desc.Height =>
                                    {
                                        texture.clone()
                                    }
                                    _ => {
                                        let cpu_desc = D3D11_TEXTURE2D_DESC {
                                            Usage: D3D11_USAGE_STAGING,
                                            BindFlags: 0,
                                            CPUAccessFlags: (D3D11_CPU_ACCESS_READ
                                                | D3D11_CPU_ACCESS_WRITE)
                                                .0
                                                as u32,
                                            MiscFlags: 0,
                                            ..desc
                                        };
                                        let mut cpu_texture = None;
                                        d3d_device.CreateTexture2D(
                                            &cpu_desc,
                                            None,
                                            Some(&mut cpu_texture),
                                        )?;
                                        let cpu_texture = cpu_texture.ok_or_else(|| {
                                            windows::core::Error::from_hresult(
                                                windows::Win32::Foundation::E_FAIL,
                                            )
                                        })?;
                                        *staging =
                                            Some((cpu_texture.clone(), desc.Width, desc.Height));
                                        *size.lock() = (desc.Width, desc.Height);
                                        cpu_texture
                                    }
                                };
                                d3d_device_context.CopyResource(&cpu_texture, &texture);

                                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                                d3d_device_context.Map(
                                    &cpu_texture,
                                    0,
                                    D3D11_MAP_READ,
                                    0,
                                    Some(&mut mapped),
                                )?;
                                let row_bytes = desc.Width as usize * 4;
                                let pitch = mapped.RowPitch as usize;
                                let height = desc.Height as usize;
                                let vframe =
                                    pack_rows(mapped.pData as *const u8, pitch, row_bytes, height);
                                d3d_device_context.Unmap(&cpu_texture, 0);
                                drop(staging);
                                let Some(vframe) = vframe else {
                                    return Ok(());
                                };
                                let _ = tx.try_send(VideoFrame {
                                    vframe,
                                    size: (desc.Width, desc.Height),
                                    pix_fmt: PixFmt::Bgra,
                                    ts,
                                });
                                Ok(())
                            },
                        ))?;
                        let terminate_closed = terminate.clone();
                        let pump = GetCurrentThreadId();
                        item.Closed(
                            &TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(
                                move |_, _| {
                                    terminate_closed.store(true, Ordering::Relaxed);
                                    let _ = PostThreadMessageW(
                                        pump,
                                        WM_NULL,
                                        Default::default(),
                                        LPARAM(0),
                                    );
                                    Ok(())
                                },
                            ),
                        )?;
                        session.StartCapture()?;
                        Ok((pool, session, token))
                    })();

                let mut message = MSG::default();
                let (pool, session, token) = match setup {
                    Ok(v) => {
                        let _ = PeekMessageW(&mut message, None, WM_USER, WM_USER, PM_NOREMOVE);
                        let id = GetCurrentThreadId();
                        let _ = setup_tx.send(Ok(id));
                        v
                    }
                    Err(e) => {
                        let _ = setup_tx.send(Err(e));
                        return Ok(());
                    }
                };

                loop {
                    if terminate.load(Ordering::Relaxed) {
                        break;
                    }
                    let ret = GetMessageW(&mut message, None, 0, 0);
                    if ret.0 == -1 || !ret.as_bool() {
                        break;
                    }
                    if terminate.load(Ordering::Relaxed) {
                        break;
                    }
                    DispatchMessageW(&message);
                }
                let _ = pool.RemoveFrameArrived(token);
                let _ = session.Close();
                Ok(())
            }
        });
        drop(setup_tx);

        let thread_id = match setup_rx.recv() {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => {
                let _ = jh.join();
                restore_hidden(&hidden);
                return Err(e);
            }
            Err(_) => {
                let _ = jh.join();
                restore_hidden(&hidden);
                return Err(CaptureError::WorkerGone);
            }
        };

        Ok(CaptureVideoDesc {
            size,
            jh: Some(jh),
            thread_id,
            hidden,
            terminate: desc_terminate,
        })
    }
}

/// Now, in nanoseconds on the same QPC timeline WASAPI and WGC report their timestamps on.
fn qpc_now() -> u64 {
    // Fixed at boot, and this sits on a per-frame path.
    static FREQUENCY: OnceLock<i64> = OnceLock::new();
    unsafe {
        let frequency = *FREQUENCY.get_or_init(|| {
            let mut frequency = 0i64;
            match QueryPerformanceFrequency(&mut frequency) {
                Ok(()) if frequency > 0 => frequency,
                _ => 0,
            }
        });
        let mut counter = 0i64;
        if frequency <= 0 || QueryPerformanceCounter(&mut counter).is_err() || counter < 0 {
            return 0;
        }
        (counter as u128 * 1_000_000_000 / frequency as u128) as u64
    }
}

unsafe fn wasapi_sample_fmt(format: *const WAVEFORMATEX) -> Option<SampleFmt> {
    let (tag, bits, cb_size) = unsafe {
        (
            (*format).wFormatTag as u32,
            (*format).wBitsPerSample,
            (*format).cbSize,
        )
    };
    let (is_float, is_pcm) = match tag {
        WAVE_FORMAT_IEEE_FLOAT => (true, false),
        WAVE_FORMAT_PCM => (false, true),
        WAVE_FORMAT_EXTENSIBLE if cb_size >= 22 => {
            let sub = unsafe { (*(format as *const WAVEFORMATEXTENSIBLE)).SubFormat };
            (
                sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
                sub == KSDATAFORMAT_SUBTYPE_PCM,
            )
        }
        _ => (false, false),
    };
    match (is_float, is_pcm, bits) {
        (true, _, 32) => Some(SampleFmt::F32),
        (true, _, 64) => Some(SampleFmt::F64),
        (_, true, 8) => Some(SampleFmt::U8),
        (_, true, 16) => Some(SampleFmt::S16),
        (_, true, 32) => Some(SampleFmt::S32),
        _ => None,
    }
}

/// This thread's COM apartment, left on drop.
struct ComApartment;

impl ComApartment {
    fn enter() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// A started WASAPI loopback capture of the default render device.
struct Loopback {
    audio_client: IAudioClient,
    capture_client: IAudioCaptureClient,
    sample_rate: i32,
    nb_channels: i32,
    sample_fmt: SampleFmt,
    /// One sample of every channel.
    frame_bytes: usize,
    buffer_frames: u32,
    /// The endpoint's default period in nanoseconds: loopback hands packets over one period
    /// after the QPC time they are stamped with.
    device_period: u64,
    // Must stay last: fields drop in order, and the interfaces above have to be released
    // while their apartment still exists.
    _com: ComApartment,
}

impl Loopback {
    fn start() -> Result<Self> {
        let com = ComApartment::enter()?;
        unsafe {
            let device_enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator as *const _, None, CLSCTX_ALL)?;
            let device = device_enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let audio_client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = audio_client.GetMixFormat()?;
            let sample_fmt = wasapi_sample_fmt(format);
            let (sample_rate, nb_channels, bits_per_sample) = (
                (*format).nSamplesPerSec as i32,
                (*format).nChannels as i32,
                (*format).wBitsPerSample as usize,
            );
            let init = audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                10_000_000, // 10_000_000 * 100ns = 1s
                0,
                format,
                None,
            );
            CoTaskMemFree(Some(format as *const _));
            init?;
            let sample_fmt = sample_fmt.ok_or(Unsupported::SampleFormat)?;
            let buffer_frames = audio_client.GetBufferSize()?;
            let mut device_period = 0i64;
            audio_client.GetDevicePeriod(Some(&mut device_period), None)?;
            let capture_client: IAudioCaptureClient = audio_client.GetService()?;
            audio_client.Start()?;
            Ok(Self {
                audio_client,
                capture_client,
                sample_rate,
                nb_channels,
                sample_fmt,
                frame_bytes: nb_channels as usize * (bits_per_sample / 8),
                buffer_frames,
                // 100 ns units; 10 ms is the shared-mode default if the driver reports none.
                device_period: u64::try_from(device_period)
                    .ok()
                    .filter(|period| *period > 0)
                    .map_or(10_000_000, |period| period * 100),
                _com: com,
            })
        }
    }

    /// Deliver audio until `terminate` is set, the consumer hangs up, or WASAPI fails.
    fn pump(
        &self,
        tx: &Sender<AudioFrame>,
        wake_rx: &Receiver<()>,
        terminate: &AtomicBool,
    ) -> Result<()> {
        let poll = Duration::from_nanos(
            1_000_000_000 * self.buffer_frames as u64 / self.sample_rate as u64 / 2,
        )
        .min(Duration::from_millis(10));
        // U8 PCM is offset binary: its zero point is 128, not 0.
        let silence = match self.sample_fmt {
            SampleFmt::U8 => 128u8,
            _ => 0,
        };
        // A packet is readable only a period (plus scheduling latency) after the time it is
        // stamped with, so an empty poll proves nothing about the most recent periods. Filling
        // silence right up to now would cover time the next real packet is stamped in, and
        // that packet would then have to be pushed later than it was captured.
        let settled = 3 * self.device_period;
        let poll_bytes = (self.sample_rate as u64 * poll.as_nanos() as u64 / 1_000_000_000 + 1)
            as usize
            * self.frame_bytes;
        let mut data: *mut u8 = core::ptr::null_mut();
        let mut nb_frames = 0u32;
        let mut flags = 0u32;
        let mut qpc = 0u64;
        let mut next_ts = qpc_now();
        while !terminate.load(Ordering::Relaxed) {
            match wake_rx.recv_timeout(poll) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => break,
            }
            let mut aframe = Vec::with_capacity(poll_bytes);
            let mut nb_samples = 0u32;
            let mut ts = None;
            while unsafe { self.capture_client.GetNextPacketSize()? } != 0 {
                unsafe {
                    self.capture_client.GetBuffer(
                        &mut data as *mut _,
                        &mut nb_frames as *mut _,
                        &mut flags as *mut _,
                        None,
                        Some(&mut qpc),
                    )?;
                }
                ts.get_or_insert(qpc * 100);
                let packet_bytes = nb_frames as usize * self.frame_bytes;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 {
                    aframe.extend_from_slice(unsafe {
                        core::slice::from_raw_parts(data, packet_bytes)
                    });
                } else {
                    aframe.resize(aframe.len() + packet_bytes, silence);
                }
                nb_samples += nb_frames;
                unsafe { self.capture_client.ReleaseBuffer(nb_frames)? };
            }
            if aframe.is_empty() {
                let gap = qpc_now().saturating_sub(settled).saturating_sub(next_ts);
                nb_samples = (gap as u128 * self.sample_rate as u128 / 1_000_000_000)
                    .min(self.sample_rate as u128) as u32;
                if nb_samples == 0 {
                    continue;
                }
                aframe.resize(nb_samples as usize * self.frame_bytes, silence);
                ts = Some(next_ts);
            }
            let ts = ts.map_or(next_ts, |ts| ts.max(next_ts));
            next_ts = ts + (nb_samples as u128 * 1_000_000_000 / self.sample_rate as u128) as u64;
            match tx.try_send(AudioFrame {
                aframe,
                nb_samples: nb_samples as i32,
                sample_rate: self.sample_rate,
                nb_channels: self.nb_channels,
                sample_fmt: self.sample_fmt,
                ts,
            }) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => terminate.store(true, Ordering::Relaxed),
            }
        }
        Ok(())
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        let _ = unsafe { self.audio_client.Stop() };
    }
}

impl AudioConfig {
    fn create(
        self,
        tx: Sender<AudioFrame>,
        terminate: Arc<AtomicBool>,
    ) -> Result<CaptureAudioDesc> {
        let sample_rate = Arc::new(Mutex::new(0));
        let (wake_tx, wake_rx) = bounded::<()>(1);
        let (setup_tx, setup_rx) = bounded::<Result<()>>(1);
        let jh = thread::spawn({
            let sample_rate = sample_rate.clone();
            let terminate = terminate.clone();
            move || {
                let loopback = match Loopback::start() {
                    Ok(loopback) => loopback,
                    Err(e) => {
                        let _ = setup_tx.send(Err(e));
                        return;
                    }
                };
                *sample_rate.lock() = loopback.sample_rate;
                let _ = setup_tx.send(Ok(()));
                if let Err(e) = loopback.pump(&tx, &wake_rx, &terminate) {
                    error!("audio capture stopped: {e}");
                }
            }
        });

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
        Ok(CaptureAudioDesc {
            sample_rate,
            jh: Some(jh),
            wake_tx,
            terminate,
        })
    }
}

/// A description of the capture, including control and size.
///
/// Drop this descriptor to terminate the capture and clean up resources.
#[derive(Debug)]
pub struct CaptureDesc {
    terminate: Arc<AtomicBool>,
    v_rx: Option<Receiver<VideoFrame>>,
    a_rx: Option<Receiver<AudioFrame>>,
    video_desc: CaptureVideoDesc,
    audio_desc: Option<CaptureAudioDesc>,
}

#[derive(Debug)]
struct CaptureVideoDesc {
    size: Arc<Mutex<(u32, u32)>>,
    jh: Option<JoinHandle<Result<()>>>,
    thread_id: u32,
    /// `HWND`s, and the affinity each had before being hidden.
    hidden: Vec<(isize, WINDOW_DISPLAY_AFFINITY)>,
    terminate: Arc<AtomicBool>,
}

impl CaptureVideoDesc {
    pub fn size(&self) -> (u32, u32) {
        *self.size.lock()
    }

    fn wake(&self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_NULL, Default::default(), LPARAM(0));
        }
    }
}

impl Drop for CaptureVideoDesc {
    fn drop(&mut self) {
        self.terminate.store(true, Ordering::Relaxed);
        self.wake();
        if let Some(jh) = self.jh.take() {
            let _ = jh.join();
        }
        restore_hidden(&self.hidden);
    }
}

#[derive(Debug)]
struct CaptureAudioDesc {
    sample_rate: Arc<Mutex<i32>>,
    jh: Option<JoinHandle<()>>,
    wake_tx: Sender<()>,
    terminate: Arc<AtomicBool>,
}

impl CaptureAudioDesc {
    pub fn sample_rate(&self) -> i32 {
        *self.sample_rate.lock()
    }

    fn wake(&self) {
        let _ = self.wake_tx.try_send(());
    }
}

impl Drop for CaptureAudioDesc {
    fn drop(&mut self) {
        self.terminate.store(true, Ordering::Relaxed);
        self.wake();
        if let Some(jh) = self.jh.take() {
            let _ = jh.join();
        }
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
        self.terminate.store(true, Ordering::Relaxed);
        self.video_desc.wake();
        if let Some(audio_desc) = self.audio_desc.as_ref() {
            audio_desc.wake();
        }
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
        self.audio_desc.as_ref().map(|desc| desc.sample_rate())
    }
}

impl Drop for CaptureDesc {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.v_rx.take(); // drop the receivers to terminate the channels
        let _ = self.a_rx.take();
    }
}
