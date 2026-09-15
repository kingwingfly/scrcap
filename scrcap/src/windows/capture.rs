use std::{
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender, bounded};
use parking_lot::Mutex;
use windows::{
    Foundation::{Metadata::ApiInformation, TypedEventHandler},
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::{HMODULE, HWND, POINT},
        Graphics::{
            Direct3D::{
                self, D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
            },
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_MAP_READ_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
                D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Texture2D,
            },
            Dxgi::IDXGIDevice,
            Gdi::{HMONITOR, MONITOR_DEFAULTTONULL, MonitorFromPoint},
        },
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
            IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole,
            eRender,
        },
        System::{
            Com::{CLSCTX_ALL, CoCreateInstance},
            WinRT::{
                Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
                Graphics::Capture::IGraphicsCaptureItemInterop,
            },
        },
        UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG},
    },
    core::{HSTRING, IInspectable, Interface as _, factory},
};

use super::utils::hide_window_from_capture;
use crate::{
    capture_desc::CaptureDescriptor,
    config::{AudioConfig, CaptureConfig, Target, VideoConfig},
    error::{CaptureError, Result},
    frame::Frame,
};

impl CaptureConfig {
    /// Create a new capture configuration.
    pub fn create(self) -> Result<CaptureDesc> {
        let (tx, rx) = bounded(self.channel_capacity);
        let terminate = Arc::new(AtomicBool::new(false));
        let video_desc = self.video.create(tx.clone(), terminate.clone())?;
        let audio_desc = if let Some(audio_config) = self.audio {
            Some(audio_config.create(tx, terminate.clone())?)
        } else {
            None
        };

        Ok(CaptureDesc {
            terminate,
            rx: Some(rx),
            video_desc,
            audio_desc,
        })
    }
}

impl VideoConfig {
    /// Create a new capture configuration.
    fn create(self, tx: Sender<Frame>, terminate: Arc<AtomicBool>) -> Result<CaptureVideoDesc> {
        if !GraphicsCaptureSession::IsSupported()? {
            return Err(CaptureError::Unsupported);
        }
        for hide in self.hide {
            hide_window_from_capture(HWND(hide as _))?;
        }

        let item: GraphicsCaptureItem = match self.target {
            Target::Primary => {
                let monitor =
                    unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTONULL) };
                assert!(!monitor.is_invalid());
                let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
                unsafe { interop.CreateForMonitor(monitor)? }
            }
            Target::Monitor(hmonitor) => {
                let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
                unsafe { interop.CreateForMonitor(HMONITOR(hmonitor as _))? }
            }
            Target::Window(hwnd) => {
                let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
                unsafe { interop.CreateForWindow(HWND(hwnd as _))? }
            }
            Target::WindowName(_) => todo!(),
            Target::Pick => unimplemented!("GraphicsCapturePicker needs a UI thread"),
        };
        let item_size = item.Size()?;
        let size = Arc::new(Mutex::new((
            item_size.Width as u32,
            item_size.Height as u32,
        )));

        let jh = thread::spawn(unsafe {
            let size = size.clone();
            move || -> Result<()> {
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
                let d3d_device = d3d_device.unwrap();
                let d3d_device_context = d3d_device_context.unwrap();
                let dxgi_device: IDXGIDevice = d3d_device.cast()?;
                let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)?;
                let device: IDirect3DDevice = inspectable.cast()?;

                let pool = Direct3D11CaptureFramePool::Create(
                    &device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    3,
                    item_size,
                )?;

                let session = pool.CreateCaptureSession(&item)?; // I do not care if item size changed
                if ApiInformation::IsPropertyPresent(
                    &HSTRING::from("Windows.Graphics.Capture.GraphicsCaptureSession"),
                    &HSTRING::from("IsBorderRequired"),
                )? {
                    session.SetIsBorderRequired(true)?;
                }
                let terminate_c = terminate.clone();
                let token = pool.FrameArrived(&TypedEventHandler::<
                    Direct3D11CaptureFramePool,
                    IInspectable,
                >::new(move |frame, _| {
                    if terminate_c.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let frame = frame.as_ref().unwrap().TryGetNextFrame()?;
                    let surface = frame.Surface()?;
                    let interface = surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
                    let texture = interface.GetInterface::<ID3D11Texture2D>()?;
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    texture.GetDesc(&mut desc);
                    *size.lock() = (desc.Width, desc.Height);

                    let cpu_desc = D3D11_TEXTURE2D_DESC {
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: (D3D11_CPU_ACCESS_READ | D3D11_CPU_ACCESS_WRITE).0 as u32,
                        MiscFlags: 0,
                        ..desc
                    };
                    let mut cpu_texture = None;
                    d3d_device.CreateTexture2D(&cpu_desc, None, Some(&mut cpu_texture))?;
                    let cpu_texture = cpu_texture.unwrap();
                    d3d_device_context.CopyResource(&cpu_texture, &texture);

                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    d3d_device_context.Map(
                        &cpu_texture,
                        0,
                        D3D11_MAP_READ_WRITE,
                        0,
                        Some(&mut mapped),
                    )?;
                    let vframe = core::slice::from_raw_parts(
                        mapped.pData as *const u8,
                        (desc.Height * mapped.RowPitch) as usize,
                    )
                    .to_vec();
                    d3d_device_context.Unmap(&cpu_texture, 0);
                    let _ = tx.try_send(Frame::Video {
                        vframe,
                        size: (desc.Width, desc.Height),
                        pix_fmt: 28, // AV_PIX_FMT_BGRA
                    });
                    Ok(())
                }))?;
                session.StartCapture()?;

                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).as_bool() {
                    if terminate.load(Ordering::Relaxed) {
                        pool.RemoveFrameArrived(token)?;
                        session.Close()?;
                        break;
                    }
                    DispatchMessageW(&message);
                }
                Ok(())
            }
        });

        Ok(CaptureVideoDesc { size, jh: Some(jh) })
    }
}

impl AudioConfig {
    fn create(self, tx: Sender<Frame>, terminate: Arc<AtomicBool>) -> Result<CaptureAudioDesc> {
        let sample_rate = Arc::new(Mutex::new(0));
        let jh = thread::spawn(unsafe {
            let sample_rate = sample_rate.clone();
            let mut sample_rate_guard = sample_rate.lock_arc();
            move || -> Result<()> {
                let device_enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator as *const _, None, CLSCTX_ALL)?;
                let device = device_enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
                let audio_client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
                let (sample_rate, nb_channels, sample_size) = {
                    let format = audio_client.GetMixFormat()?;
                    audio_client.Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK,
                        10_000_000, // 10_000_000 * 100ns = 1s
                        0,
                        format,
                        None,
                    )?;
                    (
                        (*format).nSamplesPerSec as i32,
                        (*format).nChannels as i32,
                        (*format).wBitsPerSample as usize / 8,
                    )
                };
                *sample_rate_guard = sample_rate;
                drop(sample_rate_guard); // release the lock

                let buffer_frames = audio_client.GetBufferSize()?; // 1 frame = nb_channels(samples)
                let duration =
                    Duration::from_nanos(1_000_000_000 * buffer_frames as u64 / sample_rate as u64);
                let capture_client: IAudioCaptureClient = audio_client.GetService()?;
                audio_client.Start()?;
                let mut data: *mut u8 = core::ptr::null_mut();
                let mut nb_frames = 0u32;
                let mut flags = 0u32;
                while !terminate.load(Ordering::Relaxed) {
                    thread::sleep(duration / 2); // half of the buffer duration
                    let mut aframe = Vec::with_capacity(
                        buffer_frames as usize * nb_channels as usize * sample_size / 2,
                    );
                    let mut nb_samples = 0u32;
                    while capture_client.GetNextPacketSize()? != 0 {
                        capture_client.GetBuffer(
                            &mut data as *mut _,
                            &mut nb_frames as *mut _,
                            &mut flags as *mut _,
                            None,
                            None,
                        )?;
                        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 {
                            aframe.extend_from_slice(core::slice::from_raw_parts(
                                data,
                                nb_frames as usize * nb_channels as usize * sample_size,
                            ));
                            nb_samples += nb_frames;
                        }
                        capture_client.ReleaseBuffer(nb_frames)?;
                    }
                    if aframe.is_empty() {
                        // If no data was captured, send an empty frame
                        aframe.resize(aframe.capacity(), 0);
                        nb_samples = buffer_frames / 2;
                    }
                    let _ = tx.send(Frame::Audio {
                        aframe,
                        nb_samples: nb_samples as i32,
                        sample_rate,
                        nb_channels,
                        sample_fmt: 3, // AV_SAMPLE_FMT_FLT
                    });
                }
                audio_client.Stop()?;
                Ok(())
            }
        });
        let _ = *sample_rate.lock();
        Ok(CaptureAudioDesc {
            sample_rate,
            jh: Some(jh),
        })
    }
}

/// A description of the capture, including control and size.
///
/// Drop this descriptor to terminate the capture and clean up resources.
#[derive(Debug)]
pub struct CaptureDesc {
    terminate: Arc<AtomicBool>,
    rx: Option<Receiver<Frame>>,
    video_desc: CaptureVideoDesc,
    audio_desc: Option<CaptureAudioDesc>,
}

#[derive(Debug)]
struct CaptureVideoDesc {
    size: Arc<Mutex<(u32, u32)>>,
    jh: Option<JoinHandle<Result<()>>>,
}

impl CaptureVideoDesc {
    pub fn size(&self) -> (u32, u32) {
        *self.size.lock()
    }
}

impl Drop for CaptureVideoDesc {
    fn drop(&mut self) {
        if let Some(jh) = self.jh.take() {
            let _ = jh.join();
        }
    }
}

#[derive(Debug)]
struct CaptureAudioDesc {
    sample_rate: Arc<Mutex<i32>>,
    jh: Option<JoinHandle<Result<()>>>,
}

impl CaptureAudioDesc {
    pub fn sample_rate(&self) -> i32 {
        *self.sample_rate.lock()
    }
}

impl Drop for CaptureAudioDesc {
    fn drop(&mut self) {
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
    }

    fn size(&self) -> (u32, u32) {
        self.video_desc.size()
    }

    fn sample_rate(&self) -> Option<i32> {
        self.audio_desc.as_ref().map(|desc| desc.sample_rate())
    }
}

impl Deref for CaptureDesc {
    type Target = Receiver<Frame>;

    fn deref(&self) -> &Self::Target {
        self.rx.as_ref().unwrap()
    }
}

impl Drop for CaptureDesc {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.rx.take(); // drop the receiver to terminate the channel
    }
}
