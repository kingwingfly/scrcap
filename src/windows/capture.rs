use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};
use parking_lot::Mutex;
use windows::{
    Foundation::{Metadata::ApiInformation, TimeSpan, TypedEventHandler},
    Graphics::{
        Capture::{
            Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
            GraphicsCapturePicker, GraphicsCaptureSession,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Foundation::{ERROR_NOT_FOUND, FALSE, HMODULE, HWND, LPARAM, POINT, RECT, TRUE},
        Graphics::{
            Direct3D::{
                self, D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
            },
            Direct3D11::{
                D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                ID3D11Texture2D,
            },
            Dxgi::{
                DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
                IDXGIDevice,
            },
            Gdi::{
                EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONULL,
                MONITORINFO, MonitorFromPoint,
            },
        },
        Media::{
            Audio::{
                AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
                AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_ENDPOINT_CREATE_FAILED,
                AUDCLNT_E_SERVICE_NOT_RUNNING, AUDCLNT_S_BUFFER_EMPTY, AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient, IAudioClient, IMMDevice,
                IMMDeviceEnumerator, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX,
                WAVEFORMATEXTENSIBLE, eConsole, eRender,
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
            WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
            },
        },
        UI::{
            Shell::IInitializeWithWindow,
            WindowsAndMessaging::{
                EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsWindow, IsWindowVisible,
                MONITORINFOF_PRIMARY,
            },
        },
    },
    core::{BOOL, HRESULT, HSTRING, IInspectable, Interface, factory},
};

use regex::Regex;
use tracing::{error, warn};

use super::utils::{HideToken, hide_window_from_capture, unhide_window};
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

struct WindowSearch<'a> {
    re: Regex,
    hide: &'a [isize],
    found: Option<HWND>,
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut WindowSearch<'_>) };
    if search.hide.contains(&(hwnd.0 as isize)) {
        return TRUE;
    }
    if unsafe { IsWindowVisible(hwnd) }.as_bool()
        && let Some(title) = window_title(hwnd)
        && search.re.is_match(&title)
    {
        search.found = Some(hwnd);
        return FALSE;
    }
    TRUE
}

fn window_title(hwnd: HWND) -> Option<String> {
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return None;
    }
    // Leave room for the terminator and one spare unit so a complete read is distinguishable
    // from a title that grew between the two Win32 calls.
    let mut capacity = length.checked_add(2)?;
    let mut title = Vec::new();
    loop {
        title.try_reserve(capacity as usize - title.len()).ok()?;
        title.resize(capacity as usize, 0);
        let written = unsafe { GetWindowTextW(hwnd, &mut title) };
        if written <= 0 {
            return None;
        }
        if written < capacity - 1 {
            return Some(String::from_utf16_lossy(&title[..written as usize]));
        }
        capacity = capacity.checked_mul(2)?;
    }
}

fn window_by_title(pattern: &str, hide: &[isize]) -> Result<HWND> {
    let re = Regex::new(pattern).map_err(CaptureError::BadWindowRegex)?;
    let mut search = WindowSearch {
        re,
        hide,
        found: None,
    };
    unsafe {
        let _ = EnumWindows(
            Some(collect_window),
            LPARAM(&mut search as *mut WindowSearch<'_> as isize),
        );
    }
    search.found.ok_or(CaptureError::TargetNotFound)
}

fn monitor_sort_key(monitor: HMONITOR) -> (bool, i32, i32, usize) {
    let mut info = MONITORINFO {
        cbSize: core::mem::size_of::<MONITORINFO>() as u32,
        rcMonitor: RECT::default(),
        rcWork: RECT::default(),
        dwFlags: 0,
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        let is_primary = info.dwFlags & MONITORINFOF_PRIMARY != 0;
        (
            !is_primary,
            info.rcMonitor.left,
            info.rcMonitor.top,
            monitor.0 as usize,
        )
    } else {
        (true, i32::MAX, i32::MAX, monitor.0 as usize)
    }
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
    monitors.sort_by_cached_key(|&monitor| monitor_sort_key(monitor));
    monitors
        .get(index as usize)
        .copied()
        .ok_or(CaptureError::TargetNotFound)
}

fn capture_item(target: Target, hide: &[isize]) -> Result<GraphicsCaptureItem> {
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
        Target::Window(hwnd) => {
            let handle = HWND(hwnd as _);
            if hwnd == 0 || hide.contains(&hwnd) || !unsafe { IsWindow(Some(handle)) }.as_bool() {
                return Err(CaptureError::TargetNotFound);
            }
            unsafe { interop.CreateForWindow(handle)? }
        }
        Target::WindowName(pattern) => {
            let hwnd = window_by_title(&pattern, hide)?;
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

fn restore_hidden(hidden: &[HideToken]) {
    for token in hidden {
        unhide_window(*token);
    }
}

fn is_device_lost(err: &windows::core::Error) -> bool {
    matches!(
        err.code(),
        DXGI_ERROR_DEVICE_REMOVED | DXGI_ERROR_DEVICE_RESET | DXGI_ERROR_DEVICE_HUNG
    )
}

/// State owned by the `FrameArrived` callback while the capture is live.
///
/// Wrapped in `Arc<Mutex<Option<FrameState>>>` so teardown can take it under the lock,
/// waiting for any in-flight `CreateFreeThreaded` callback to finish and preventing any
/// late callback from touching the pool, D3D11 context, or video channel after teardown.
struct FrameState {
    tx: Sender<VideoFrame>,
    gate: FpsGate,
    d3d_device: ID3D11Device,
    d3d_device_context: ID3D11DeviceContext,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    pool_size: SizeInt32,
    size: Arc<Mutex<(u32, u32)>>,
}

impl FrameState {
    /// Copy one frame out, cropped to `content`.
    fn copy_frame(
        &mut self,
        frame: &Direct3D11CaptureFrame,
        content: SizeInt32,
    ) -> windows::core::Result<()> {
        // Only ContentSize pixels are defined, including on the resize frame still backed
        // by the old pool texture.
        if content.Width <= 0 || content.Height <= 0 {
            return Ok(());
        }
        let ts = frame
            .SystemRelativeTime()
            .ok()
            .and_then(|t| u64::try_from(t.Duration).ok())
            .filter(|&t| t > 0)
            .and_then(|t| t.checked_mul(100))
            .unwrap_or_else(qpc_now);
        // A full channel would drop the frame anyway; skip the readback.
        if self.tx.is_full() || !self.gate.allow(ts) {
            return Ok(());
        }
        let surface = frame.Surface()?;
        let interface = surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
        let texture = unsafe { interface.GetInterface::<ID3D11Texture2D>()? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        // This frame was allocated at the pool's *old* size, and only the top-left
        // `ContentSize` of it is defined, so a source that has shrunk is cropped rather
        // than copied whole. Recreating the pool only fixes the frames after this one.
        let width = desc.Width.min(content.Width as u32);
        let height = desc.Height.min(content.Height as u32);
        if width == 0 || height == 0 {
            return Ok(());
        }
        let Some(row_bytes) = (width as usize).checked_mul(4) else {
            return Ok(());
        };

        let cpu_texture = match self.staging.as_ref() {
            Some((texture, w, h)) if *w == width && *h == height => texture.clone(),
            _ => {
                let cpu_desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..desc
                };
                let mut cpu_texture = None;
                unsafe {
                    self.d3d_device
                        .CreateTexture2D(&cpu_desc, None, Some(&mut cpu_texture))?;
                }
                let cpu_texture = cpu_texture.ok_or_else(|| {
                    windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL)
                })?;
                self.staging = Some((cpu_texture.clone(), width, height));
                *self.size.lock() = (width, height);
                cpu_texture
            }
        };
        unsafe {
            self.d3d_device_context.CopySubresourceRegion(
                &cpu_texture,
                0,
                0,
                0,
                0,
                &texture,
                0,
                Some(&D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: width,
                    bottom: height,
                    back: 1,
                }),
            );
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.d3d_device_context
                .Map(&cpu_texture, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let pitch = mapped.RowPitch as usize;
        let rows = height as usize;
        let vframe = unsafe { pack_rows(mapped.pData as *const u8, pitch, row_bytes, rows) };
        unsafe { self.d3d_device_context.Unmap(&cpu_texture, 0) };
        let Some(vframe) = vframe else {
            return Ok(());
        };
        let _ = self.tx.try_send(VideoFrame {
            vframe,
            size: (width, height),
            pix_fmt: PixFmt::Bgra,
            ts,
        });
        Ok(())
    }

    fn on_frame(&mut self, pool: &Direct3D11CaptureFramePool) -> windows::core::Result<()> {
        let frame = match pool.TryGetNextFrame() {
            Ok(frame) => frame,
            // An empty pool yields a null frame, which windows-rs surfaces as a success-coded error.
            Err(e) if e.code().is_ok() => return Ok(()),
            Err(e) => return Err(e),
        };
        // The pool keeps handing out textures of the size it was made with, clipping or
        // padding a source that has since resized, so a change means recreating it -- but
        // not before this frame, which belongs to the generation being replaced, has gone
        // back to the pool. The copy's outcome is carried past that, so a resize is
        // followed up whichever way the copy went, including the early return on a full
        // channel.
        let content = match frame.ContentSize() {
            Ok(content) => content,
            Err(e) => {
                let _ = frame.Close();
                return Err(e);
            }
        };
        let copied = self.copy_frame(&frame, content);
        let _ = frame.Close();
        drop(frame);
        if let Err(ref e) = copied
            && is_device_lost(e)
        {
            return copied;
        }
        if content.Width > 0 && content.Height > 0 && content != self.pool_size {
            pool.Recreate(
                &winrt_device(&self.d3d_device)?,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                3,
                content,
            )?;
            self.pool_size = content;
        }
        copied
    }
}

/// Owns the live WGC objects on the video worker thread and tears them down in a safe,
/// synchronized order on both normal exit and partial setup failure.
struct CaptureTeardown {
    session: Option<GraphicsCaptureSession>,
    pool: Direct3D11CaptureFramePool,
    item: GraphicsCaptureItem,
    state: Arc<Mutex<Option<FrameState>>>,
    frame_token: Option<i64>,
    closed_token: Option<i64>,
    terminate: Arc<AtomicBool>,
}

impl Drop for CaptureTeardown {
    fn drop(&mut self) {
        self.terminate.store(true, Ordering::Release);
        if let Some(token) = self.closed_token.take() {
            let _ = self.item.RemoveClosed(token);
        }
        if let Some(token) = self.frame_token.take() {
            let _ = self.pool.RemoveFrameArrived(token);
        }
        // Wait for any in-flight `FrameArrived` callback to leave `on_frame` and prevent
        // late callbacks from touching `pool`, `d3d_device_context`, or `tx`.
        let taken = self.state.lock().take();
        if let Some(session) = self.session.as_ref() {
            let _ = session.Close();
        }
        let _ = self.pool.Close();
        drop(taken);
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
                Ok(token) => hidden.push(token),
                Err(e) => {
                    restore_hidden(&hidden);
                    return Err(e);
                }
            }
        }

        let item = match capture_item(self.target, &self.hide) {
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
            item_size.Width.max(0) as u32,
            item_size.Height.max(0) as u32,
        )));

        let (wake_tx, wake_rx) = bounded::<()>(1);
        let (setup_tx, setup_rx) = bounded::<Result<()>>(1);

        let desc_terminate = terminate.clone();
        let jh = match thread::Builder::new()
            .name("scrcap-video".to_string())
            .spawn({
                let size = size.clone();
                let setup_tx = setup_tx.clone();
                let wake_tx = wake_tx.clone();
                move || {
                    let setup = (|| -> Result<CaptureTeardown> {
                        const FEATURE_FLAGS: [Direct3D::D3D_FEATURE_LEVEL; 2] =
                            [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
                        let mut d3d_device = None;
                        let mut d3d_device_context = None;
                        unsafe {
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
                        }
                        let d3d_device = d3d_device.ok_or(Unsupported::Direct3D)?;
                        let d3d_device_context = d3d_device_context.ok_or(Unsupported::Direct3D)?;
                        let device = winrt_device(&d3d_device)?;

                        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                            &device,
                            DirectXPixelFormat::B8G8R8A8UIntNormalized,
                            3,
                            item_size,
                        )?;
                        let state = Arc::new(Mutex::new(Some(FrameState {
                            tx,
                            gate: FpsGate::new(fps),
                            d3d_device,
                            d3d_device_context,
                            staging: None,
                            pool_size: item_size,
                            size,
                        })));
                        let mut teardown = CaptureTeardown {
                            session: None,
                            pool,
                            item,
                            state: state.clone(),
                            frame_token: None,
                            closed_token: None,
                            terminate: terminate.clone(),
                        };

                        let session = teardown.pool.CreateCaptureSession(&teardown.item)?;
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
                        teardown.session = Some(session);

                        let terminate_frame = terminate.clone();
                        let wake_frame = wake_tx.clone();
                        let state_frame = state;
                        teardown.frame_token =
                            Some(teardown.pool.FrameArrived(&TypedEventHandler::<
                                Direct3D11CaptureFramePool,
                                IInspectable,
                            >::new(
                                move |pool, _| {
                                    let mut guard = state_frame.lock();
                                    if terminate_frame.load(Ordering::Acquire) {
                                        return Ok(());
                                    }
                                    let Some(state) = guard.as_mut() else {
                                        return Ok(());
                                    };
                                    let Some(pool) = pool.as_ref() else {
                                        return Ok(());
                                    };
                                    // Logged rather than handed back to WinRT, which discards it
                                    // silently: a persistent failure here -- a lost D3D11 device,
                                    // say -- otherwise just stops frames arriving, with nothing in
                                    // the log and the channel still open, so a consumer waits in
                                    // `recv` with no way to tell why.
                                    if let Err(e) = state.on_frame(pool) {
                                        error!("capture frame failed: {e}");
                                        // A device that was removed, reset or hung never comes back
                                        // for this capture, so end it: every later frame would fail
                                        // the same way, leaving a consumer in `recv` with no sign
                                        // that the frames have stopped for good.
                                        if is_device_lost(&e) {
                                            terminate_frame.store(true, Ordering::Release);
                                            let _ = wake_frame.try_send(());
                                        }
                                    }
                                    Ok(())
                                },
                            ))?);

                        let terminate_closed = terminate.clone();
                        let wake_closed = wake_tx;
                        teardown.closed_token =
                            Some(teardown.item.Closed(&TypedEventHandler::<
                                GraphicsCaptureItem,
                                IInspectable,
                            >::new(
                                move |_, _| {
                                    terminate_closed.store(true, Ordering::Release);
                                    let _ = wake_closed.try_send(());
                                    Ok(())
                                },
                            ))?);

                        if let Some(session) = teardown.session.as_ref() {
                            session.StartCapture()?;
                        }
                        Ok(teardown)
                    })();

                    let _teardown = match setup {
                        Ok(teardown) => {
                            let _ = setup_tx.send(Ok(()));
                            teardown
                        }
                        Err(e) => {
                            let _ = setup_tx.send(Err(e));
                            return;
                        }
                    };

                    while !terminate.load(Ordering::Acquire) {
                        if wake_rx.recv().is_err() {
                            break;
                        }
                    }
                }
            }) {
            Ok(jh) => jh,
            Err(e) => {
                restore_hidden(&hidden);
                return Err(e.into());
            }
        };
        drop(setup_tx);

        match setup_rx.recv() {
            Ok(Ok(())) => {}
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
        }

        Ok(CaptureVideoDesc {
            size,
            jh: Some(jh),
            wake_tx,
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

fn is_device_invalidated(err: &CaptureError) -> bool {
    matches!(err, CaptureError::Win(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED)
}

fn is_transient_audio_error(err: &CaptureError) -> bool {
    const E_NOTFOUND: HRESULT = HRESULT::from_win32(ERROR_NOT_FOUND.0);
    matches!(
        err,
        CaptureError::Win(e)
            if e.code() == E_NOTFOUND
                || e.code() == AUDCLNT_E_DEVICE_INVALIDATED
                || e.code() == AUDCLNT_E_ENDPOINT_CREATE_FAILED
                || e.code() == AUDCLNT_E_SERVICE_NOT_RUNNING
    )
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

/// Raw `IAudioCaptureClient::GetBuffer` preserving the exact `HRESULT`.
///
/// The high-level wrapper maps any success `HRESULT` (including `AUDCLNT_S_BUFFER_EMPTY`) to
/// `Ok(())`, hiding whether a packet was actually acquired; calling `ReleaseBuffer(0)` after
/// `AUDCLNT_S_BUFFER_EMPTY` fails with `AUDCLNT_E_OUT_OF_ORDER`.
unsafe fn get_buffer_raw(
    capture_client: &IAudioCaptureClient,
    data: &mut *mut u8,
    nb_frames: &mut u32,
    flags: &mut u32,
    qpc: &mut u64,
) -> HRESULT {
    unsafe {
        (Interface::vtable(capture_client).GetBuffer)(
            Interface::as_raw(capture_client),
            data,
            nb_frames,
            flags,
            core::ptr::null_mut(),
            qpc,
        )
    }
}

/// Owns a COM task-memory allocation and releases it with `CoTaskMemFree` on drop.
struct CoTaskMem<T>(*mut T);

impl<T> Drop for CoTaskMem<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0.cast_const().cast())) };
        }
    }
}

fn device_id(device: &IMMDevice) -> Option<Vec<u16>> {
    let raw = unsafe { device.GetId() }.ok()?;
    let guard = CoTaskMem(raw.0);
    if guard.0.is_null() {
        return None;
    }
    // SAFETY: `IMMDevice::GetId` returns a NUL-terminated UTF-16 string allocated with
    // `CoTaskMemAlloc`, which stays live until `guard` drops.
    let id = unsafe { raw.as_wide() }.to_vec();
    drop(guard);
    Some(id)
}

/// How often `Loopback::pump` probes whether the default output device changed.
const DEFAULT_DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(250);

enum PumpStop {
    /// Capture terminated or the consumer hung up.
    Stopped,
    /// The default output device changed and loopback should re-bind.
    Rebind,
}

/// A started WASAPI loopback capture of the default render device.
///
/// Must be dropped while the [`ComApartment`] passed to [`Loopback::start`] is still live.
struct Loopback {
    audio_client: IAudioClient,
    capture_client: IAudioCaptureClient,
    device_id: Option<Vec<u16>>,
    sample_rate: i32,
    nb_channels: i32,
    sample_fmt: SampleFmt,
    /// One sample of every channel (`nBlockAlign`).
    frame_bytes: usize,
    buffer_frames: u32,
    /// The endpoint's default period in nanoseconds: loopback hands packets over one period
    /// after the QPC time they are stamped with.
    device_period: u64,
}

impl Loopback {
    fn start(_com: &ComApartment, enumerator: &IMMDeviceEnumerator) -> Result<Self> {
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
        let device_id = device_id(&device);
        let audio_client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
        let format = CoTaskMem(unsafe { audio_client.GetMixFormat()? });
        if format.0.is_null() {
            return Err(Unsupported::SampleFormat.into());
        }
        let sample_fmt = unsafe { wasapi_sample_fmt(format.0) };
        let (sample_rate, nb_channels, bits_per_sample, frame_bytes) = unsafe {
            (
                i32::try_from((*format.0).nSamplesPerSec).unwrap_or(0),
                (*format.0).nChannels as i32,
                (*format.0).wBitsPerSample as usize,
                (*format.0).nBlockAlign as usize,
            )
        };
        let init = unsafe {
            audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                10_000_000, // 10_000_000 * 100ns = 1s
                0,
                format.0,
                None,
            )
        };
        drop(format);
        init?;
        let sample_fmt = sample_fmt.ok_or(Unsupported::SampleFormat)?;
        let expected_frame_bytes = (nb_channels as usize).checked_mul(bits_per_sample / 8);
        // Both `sample_rate` and `nb_channels` are divisors in the poll and timeline maths
        // below, and `nBlockAlign` is the authoritative byte size of one frame.
        if sample_rate <= 0
            || nb_channels <= 0
            || frame_bytes == 0
            || expected_frame_bytes != Some(frame_bytes)
        {
            return Err(Unsupported::SampleFormat.into());
        }
        let buffer_frames = unsafe { audio_client.GetBufferSize()? };
        let mut device_period = 0i64;
        unsafe { audio_client.GetDevicePeriod(Some(&mut device_period), None)? };
        let capture_client: IAudioCaptureClient = unsafe { audio_client.GetService()? };
        unsafe { audio_client.Start()? };
        Ok(Self {
            audio_client,
            capture_client,
            device_id,
            sample_rate,
            nb_channels,
            sample_fmt,
            frame_bytes,
            buffer_frames,
            // 100 ns units; 10 ms is the shared-mode default if the driver reports none.
            device_period: u64::try_from(device_period)
                .ok()
                .filter(|&period| period > 0)
                .and_then(|period| period.checked_mul(100))
                .unwrap_or(10_000_000),
        })
    }

    /// Returns `true` if Windows' default render console endpoint now has a different ID.
    ///
    /// Transient probe failures (e.g. `E_NOTFOUND`) are ignored so `pump` and
    /// `AUDCLNT_E_DEVICE_INVALIDATED` handling decide whether to re-bind.
    fn default_device_changed(&self, enumerator: &IMMDeviceEnumerator) -> bool {
        let Some(active_id) = self.device_id.as_deref() else {
            return false;
        };
        let Ok(default) = (unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }) else {
            return false;
        };
        let Some(default_id) = device_id(&default) else {
            return false;
        };
        default_id.as_slice() != active_id
    }

    /// Deliver audio until `terminate` is set, the consumer hangs up, the default output device
    /// changes, or WASAPI fails.
    fn pump(
        &self,
        enumerator: &IMMDeviceEnumerator,
        tx: &Sender<AudioFrame>,
        wake_rx: &Receiver<()>,
        terminate: &AtomicBool,
        line: &mut Timeline,
    ) -> Result<PumpStop> {
        let poll = Duration::from_nanos(
            (1_000_000_000u128 * self.buffer_frames as u128 / self.sample_rate as u128 / 2)
                .min(10_000_000) as u64,
        )
        .max(Duration::from_millis(1));
        // U8 PCM is offset binary: its zero point is 128, not 0.
        let silence = match self.sample_fmt {
            SampleFmt::U8 => 128u8,
            _ => 0,
        };
        // A packet is readable only a period (plus scheduling latency) after the time it is
        // stamped with, so an empty poll proves nothing about the most recent periods. Filling
        // silence right up to now would cover time the next real packet is stamped in, and
        // that packet would then have to be pushed later than it was captured.
        let settled = self.device_period.saturating_mul(3);
        let mut last_device_check = Instant::now();
        while !terminate.load(Ordering::Acquire) {
            match wake_rx.recv_timeout(poll) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => return Ok(PumpStop::Stopped),
            }
            let mut delivered = false;
            while unsafe { self.capture_client.GetNextPacketSize()? } != 0 {
                let mut data: *mut u8 = core::ptr::null_mut();
                let mut nb_frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                let hr = unsafe {
                    get_buffer_raw(
                        &self.capture_client,
                        &mut data,
                        &mut nb_frames,
                        &mut flags,
                        &mut qpc,
                    )
                };
                if hr == AUDCLNT_S_BUFFER_EMPTY {
                    break;
                }
                hr.ok()?;
                // Only release when `S_OK` genuinely acquired a zero-frame packet;
                // `AUDCLNT_S_BUFFER_EMPTY` above acquired nothing and must not call `ReleaseBuffer`.
                if nb_frames == 0 {
                    unsafe { self.capture_client.ReleaseBuffer(0)? };
                    break;
                }
                // One frame per packet, each with its own capture time: two packets drained
                // in the same poll may be a pause apart, and merging them would place the
                // second one early. `TIMESTAMP_ERROR` means the position is not usable, and
                // a frame with no usable time of its own simply continues the timeline --
                // which is the same QPC domain, not a foreign clock.
                let ts = qpc.checked_mul(100).filter(|&ts| {
                    ts != 0 && flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0
                });
                let Some(packet_bytes) = (nb_frames as usize)
                    .checked_mul(self.frame_bytes)
                    .filter(|&bytes| bytes <= isize::MAX as usize)
                else {
                    unsafe { self.capture_client.ReleaseBuffer(nb_frames)? };
                    continue;
                };
                if i32::try_from(nb_frames).is_err() {
                    unsafe { self.capture_client.ReleaseBuffer(nb_frames)? };
                    continue;
                }
                if tx.is_full() {
                    unsafe { self.capture_client.ReleaseBuffer(nb_frames)? };
                    let _ = self.advance_timeline(line, nb_frames, ts, false);
                    delivered = true;
                    continue;
                }
                let is_silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let aframe = if !is_silent && !data.is_null() {
                    let mut aframe = Vec::with_capacity(packet_bytes);
                    aframe.extend_from_slice(unsafe {
                        core::slice::from_raw_parts(data, packet_bytes)
                    });
                    unsafe { self.capture_client.ReleaseBuffer(nb_frames)? };
                    aframe
                } else {
                    unsafe { self.capture_client.ReleaseBuffer(nb_frames)? };
                    vec![silence; packet_bytes]
                };
                self.emit(tx, terminate, line, aframe, nb_frames, ts, false);
                delivered = true;
            }
            if terminate.load(Ordering::Acquire) {
                return Ok(PumpStop::Stopped);
            }
            if last_device_check.elapsed() >= DEFAULT_DEVICE_POLL_INTERVAL {
                last_device_check = Instant::now();
                if self.default_device_changed(enumerator) {
                    return Ok(PumpStop::Rebind);
                }
            }
            if delivered {
                continue;
            }
            // Loopback delivers nothing while no app is playing; the gap is made up here.
            let gap = qpc_now().saturating_sub(settled).saturating_sub(line.next);
            let nb_samples = (gap as u128 * self.sample_rate as u128 / 1_000_000_000)
                .min(self.sample_rate as u128) as u32;
            if nb_samples == 0 {
                continue;
            }
            if tx.is_full() {
                let _ = self.advance_timeline(line, nb_samples, None, true);
                continue;
            }
            let Some(silence_bytes) = (nb_samples as usize)
                .checked_mul(self.frame_bytes)
                .filter(|&bytes| bytes <= isize::MAX as usize)
            else {
                continue;
            };
            let aframe = vec![silence; silence_bytes];
            self.emit(tx, terminate, line, aframe, nb_samples, None, true);
        }
        Ok(PumpStop::Stopped)
    }
}

/// How far the emitted audio has reached, in the QPC domain, and how it got there.
struct Timeline {
    next: u64,
    /// Whether the samples up to `next` end in silence [`Loopback::pump`] made up rather
    /// than in samples WASAPI delivered.
    invented: bool,
}

impl Loopback {
    /// Advance `line` for a packet of `nb_samples` starting at `ts` (or `line.next`).
    ///
    /// Returns `Some((emitted_ts, remaining_samples, trimmed_leading_samples))`, or `None` if
    /// the whole packet falls inside already-emitted silence.
    fn advance_timeline(
        &self,
        line: &mut Timeline,
        nb_samples: u32,
        ts: Option<u64>,
        invented: bool,
    ) -> Option<(u64, u32, usize)> {
        let mut ts = ts.unwrap_or(line.next);
        let mut trimmed = 0usize;
        let mut remaining = nb_samples;
        if ts < line.next {
            if line.invented {
                // The frame reaches back into time already sent as made-up silence, or into
                // time before the capture started. That cannot be taken back, and stamping
                // the frame late instead would make every contiguous frame after it late
                // too, since each continues from the one before. Drop the overlap;
                // interleaved samples trim from the front one sample of every channel at a
                // time.
                let overlap = ((line.next - ts) as u128 * self.sample_rate as u128 + 500_000_000)
                    / 1_000_000_000;
                if overlap >= remaining as u128 {
                    return None;
                }
                trimmed = overlap as usize;
                remaining -= overlap as u32;
            }
            // After real samples the overlap is only timestamp jitter: the stream is
            // contiguous, so it continues exactly where the last frame ended.
            ts = line.next;
        }
        line.invented = invented;
        line.next = ts
            .saturating_add((remaining as u128 * 1_000_000_000 / self.sample_rate as u128) as u64);
        Some((ts, remaining, trimmed))
    }

    /// Send one frame, keeping the emitted timeline continuous.
    ///
    /// `ts` is the frame's own capture time where there is a usable one; without it the
    /// frame continues where the last one ended.
    #[allow(clippy::too_many_arguments)]
    fn emit(
        &self,
        tx: &Sender<AudioFrame>,
        terminate: &AtomicBool,
        line: &mut Timeline,
        mut aframe: Vec<u8>,
        nb_samples: u32,
        ts: Option<u64>,
        invented: bool,
    ) {
        let Some((ts, remaining, trimmed)) = self.advance_timeline(line, nb_samples, ts, invented)
        else {
            return;
        };
        let Ok(nb_samples) = i32::try_from(remaining) else {
            return;
        };
        if trimmed > 0 {
            aframe.drain(..trimmed * self.frame_bytes);
        }
        match tx.try_send(AudioFrame {
            aframe,
            nb_samples,
            sample_rate: self.sample_rate,
            nb_channels: self.nb_channels,
            sample_fmt: self.sample_fmt,
            ts,
        }) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => terminate.store(true, Ordering::Release),
        }
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
        let (wake_tx, wake_rx) = bounded::<()>(1);
        let (setup_tx, setup_rx) = bounded::<Result<i32>>(1);
        let sample_rate = Arc::new(AtomicI32::new(0));
        let jh = thread::Builder::new()
            .name("scrcap-audio".to_string())
            .spawn({
                let terminate = terminate.clone();
                let sample_rate = sample_rate.clone();
                move || {
                    let com = match ComApartment::enter() {
                        Ok(com) => com,
                        Err(e) => {
                            let _ = setup_tx.send(Err(e));
                            return;
                        }
                    };
                    let enumerator: IMMDeviceEnumerator = match unsafe {
                        CoCreateInstance(&MMDeviceEnumerator as *const _, None, CLSCTX_ALL)
                    } {
                        Ok(enumerator) => enumerator,
                        Err(e) => {
                            let _ = setup_tx.send(Err(e.into()));
                            return;
                        }
                    };
                    let mut loopback = match Loopback::start(&com, &enumerator) {
                        Ok(loopback) => loopback,
                        Err(e) => {
                            let _ = setup_tx.send(Err(e));
                            return;
                        }
                    };
                    sample_rate.store(loopback.sample_rate, Ordering::Release);
                    let _ = setup_tx.send(Ok(loopback.sample_rate));

                    let mut line = Timeline {
                        next: qpc_now(),
                        // Nothing has been emitted, so nothing before `next` is a real sample either.
                        invented: true,
                    };
                    'capture: loop {
                        match loopback.pump(&enumerator, &tx, &wake_rx, &terminate, &mut line) {
                            Ok(PumpStop::Stopped) => break,
                            Ok(PumpStop::Rebind) => {}
                            Err(e) if is_device_invalidated(&e) => {}
                            Err(e) => {
                                error!("audio capture stopped: {e}");
                                break;
                            }
                        }
                        drop(loopback);
                        let mut backoff = Duration::from_millis(50);
                        let mut logged = false;
                        loop {
                            if terminate.load(Ordering::Acquire) {
                                break 'capture;
                            }
                            match Loopback::start(&com, &enumerator) {
                                Ok(next) => {
                                    sample_rate.store(next.sample_rate, Ordering::Release);
                                    loopback = next;
                                    continue 'capture;
                                }
                                Err(err) if is_transient_audio_error(&err) => {
                                    if !logged {
                                        warn!(
                                            "audio device unavailable, waiting to reconnect: {err}"
                                        );
                                        logged = true;
                                    }
                                    match wake_rx.recv_timeout(backoff) {
                                        Err(RecvTimeoutError::Timeout) => {
                                            backoff = (backoff * 2).min(Duration::from_millis(500));
                                        }
                                        _ => break 'capture,
                                    }
                                }
                                Err(err) => {
                                    error!("audio capture stopped: {err}");
                                    break 'capture;
                                }
                            }
                        }
                    }
                }
            })?;

        match setup_rx.recv() {
            Ok(Ok(_)) => {}
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
    jh: Option<JoinHandle<()>>,
    wake_tx: Sender<()>,
    /// What this capture hid, each to be given back once.
    hidden: Vec<HideToken>,
    terminate: Arc<AtomicBool>,
}

impl CaptureVideoDesc {
    pub fn size(&self) -> (u32, u32) {
        *self.size.lock()
    }

    fn wake(&self) {
        let _ = self.wake_tx.try_send(());
    }
}

impl Drop for CaptureVideoDesc {
    fn drop(&mut self) {
        self.terminate.store(true, Ordering::Release);
        self.wake();
        if let Some(jh) = self.jh.take() {
            let _ = jh.join();
        }
        restore_hidden(&self.hidden);
    }
}

#[derive(Debug)]
struct CaptureAudioDesc {
    sample_rate: Arc<AtomicI32>,
    jh: Option<JoinHandle<()>>,
    wake_tx: Sender<()>,
    terminate: Arc<AtomicBool>,
}

impl CaptureAudioDesc {
    pub fn sample_rate(&self) -> i32 {
        self.sample_rate.load(Ordering::Acquire)
    }

    fn wake(&self) {
        let _ = self.wake_tx.try_send(());
    }
}

impl Drop for CaptureAudioDesc {
    fn drop(&mut self) {
        self.terminate.store(true, Ordering::Release);
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
        self.terminate.store(true, Ordering::Release);
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
