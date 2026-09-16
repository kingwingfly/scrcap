//! This example use `wgpu` to display the captured frames (For test only).

use std::{sync::Arc, thread, time::Duration};

use anyhow::Result;
use crossbeam_channel::{Receiver, TryRecvError, bounded};
use log::{error, info, warn};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use wgpu::rwh::HasWindowHandle;
use wgpu::{CurrentSurfaceTexture, util::DeviceExt as _};
use winit::{
    application::ApplicationHandler,
    event::{KeyEvent, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId, WindowLevel},
};

use scrcap::{CaptureConfig, CaptureDesc, CaptureDescriptor as _, Target, VideoConfig, VideoFrame};

fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Debug)
        .init();
    let event_loop = EventLoop::with_user_event().build().unwrap();
    event_loop
        .run_app(&mut App::default())
        .expect("Event loop should run successfully");
}

#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Vertex {
    position: [f32; 3],
    tex_coords: [f32; 2],
}

impl Vertex {
    fn desc() -> wgpu::VertexBufferLayout<'static> {
        const ATTRIBS: [wgpu::VertexAttribute; 2] =
            wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2];
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &ATTRIBS,
        }
    }
}

const VERTICES: &[Vertex] = &[
    Vertex {
        position: [-1.0, 1.0, 0.0],
        tex_coords: [0.0, 0.0],
    }, // A
    Vertex {
        position: [-1.0, -1.0, 0.0],
        tex_coords: [0.0, 1.0],
    }, // B
    Vertex {
        position: [1.0, -1.0, 0.0],
        tex_coords: [1.0, 1.0],
    }, // C
    Vertex {
        position: [1.0, 1.0, 0.0],
        tex_coords: [1.0, 0.0],
    }, // D
];

const INDICES: &[u16] = &[0, 1, 2, 0, 2, 3];

#[derive(Debug, Default)]
struct App {
    state: Option<State>,
    /// The capture being built on another thread.
    pending: Option<Receiver<scrcap::error::Result<CaptureDesc>>>,
}

/// This window's own id, in the form [`VideoConfig::hide`] wants, so the preview does not
/// end up recording itself. Empty where the platform cannot hide a window.
#[allow(unused_variables, unused_mut)]
fn hide_self(window: &Window) -> Vec<isize> {
    let mut hide = Vec::new();
    #[cfg(target_os = "macos")]
    if let Ok(wgpu::rwh::RawWindowHandle::AppKit(handle)) =
        window.window_handle().map(|handle| handle.as_raw())
    {
        // The view belongs to this window, so it is alive for the call.
        let id =
            unsafe { scrcap::platform::window_id_from_ns_view(handle.ns_view.as_ptr() as isize) };
        hide.extend(id.map(|id| id as isize));
    }
    #[cfg(target_os = "windows")]
    if let Ok(wgpu::rwh::RawWindowHandle::Win32(handle)) =
        window.window_handle().map(|handle| handle.as_raw())
    {
        hide.push(handle.hwnd.get());
    }
    hide
}

/// Build the capture on another thread.
///
/// `Target::Pick` blocks `create` until the user chooses, and on Windows and macOS the picker
/// answers on this very thread, so `create` has to run somewhere else.
fn start_capture(window: &Window) -> Receiver<scrcap::error::Result<CaptureDesc>> {
    let hide = hide_self(window);
    #[cfg(target_os = "windows")]
    let target = Target::Pick(match window.window_handle().map(|handle| handle.as_raw()) {
        Ok(wgpu::rwh::RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
        _ => panic!("the picker needs a Win32 window to present from"),
    });
    #[cfg(not(target_os = "windows"))]
    let target = Target::Pick;

    let (tx, rx) = bounded(1);
    thread::spawn(move || {
        let _ = tx.send(
            CaptureConfig {
                video: VideoConfig {
                    channel_capacity: 2,
                    hide,
                    target,
                    fps: Some(60),
                },
                audio: None,
            }
            .create(),
        );
    });
    rx
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = event_loop
            .create_window(Window::default_attributes().with_window_level(WindowLevel::AlwaysOnTop))
            .expect("Window should be created");
        let state = pollster::block_on(State::new(window)).expect("State should be initialized");
        state.window.request_redraw();
        self.state = Some(state);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(rx), Some(state)) = (self.pending.as_ref(), self.state.as_mut()) else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(capture_desc)) => {
                state.attach(capture_desc);
                self.pending = None;
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Ok(Err(e)) => {
                error!("failed to start the capture: {e}");
                event_loop.exit();
            }
            Err(TryRecvError::Empty) => {
                event_loop.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(50)));
            }
            Err(TryRecvError::Disconnected) => {
                error!("the capture thread went away");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = &mut self.state else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => state.resize(size.width, size.height),
            WindowEvent::RedrawRequested => {
                if !state.render() {
                    event_loop.exit();
                }
                // Only once the window has shown a frame: GNOME's mutter has crashed capturing
                // a window, picked through the portal, that had not drawn yet.
                if state.presented && state.capture_desc.is_none() && self.pending.is_none() {
                    self.pending = Some(start_capture(&state.window));
                    event_loop
                        .set_control_flow(ControlFlow::wait_duration(Duration::from_millis(50)));
                }
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state: key_state,
                        ..
                    },
                ..
            } => state.handle_key(event_loop, code, key_state.is_pressed()),
            _ => {}
        }
    }
}

#[derive(Debug)]
struct State {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    is_surface_configured: bool,
    render_pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    num_indices: u32,
    texture_bind_group_layout: wgpu::BindGroupLayout,
    diffuse_texture: Texture,
    diffuse_bind_group: wgpu::BindGroup,
    /// Whether a frame has reached the screen yet.
    presented: bool,
    capture_desc: Option<CaptureDesc>,
}

impl State {
    async fn new(window: Window) -> Result<Self> {
        let window = Arc::new(window);
        let size = window.inner_size();

        // The instance is a handle to our GPU
        // BackendBit::PRIMARY => Vulkan + Metal + DX12 + Browser WebGPU
        // `Backends::PRIMARY` never uses the display handle (it only matters for GLES
        // on Wayland), so the no-display-handle defaults are what we want here.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let surface = instance.create_surface(window.clone()).unwrap();

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })
            .await?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: None,
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: Default::default(),
                trace: wgpu::Trace::Off,
            })
            .await?;

        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width,
            height: size.height,
            present_mode: surface_caps.present_modes[0],
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };

        // A white placeholder until the capture, and with it the real size, arrives.
        let diffuse_texture =
            Texture::from_bytes(&device, &queue, &[255; 4], 1, 1, Some("diffuse_texture"))?;

        let texture_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        // This should match the filterable field of the
                        // corresponding Texture entry above.
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
                label: Some("texture_bind_group_layout"),
            });

        let diffuse_bind_group = diffuse_texture.bind_group(&device, &texture_bind_group_layout);

        let render_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Render Pipeline Layout"),
                bind_group_layouts: &[Some(&texture_bind_group_layout)],
                immediate_size: 0,
            });

        let shader = device.create_shader_module(wgpu::include_wgsl!("shader/shader.wgsl"));

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Render Pipeline"),
            layout: Some(&render_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Some(Vertex::desc())],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: Some(wgpu::Face::Back),
                // Setting this to anything other than Fill requires Features::NON_FILL_POLYGON_MODE
                polygon_mode: wgpu::PolygonMode::Fill,
                // Requires Features::DEPTH_CLIP_CONTROL
                unclipped_depth: false,
                // Requires Features::CONSERVATIVE_RASTERIZATION
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Vertex Buffer"),
            contents: bytemuck::cast_slice(VERTICES),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Index Buffer"),
            contents: bytemuck::cast_slice(INDICES),
            usage: wgpu::BufferUsages::INDEX,
        });

        // Configure now: nothing can be presented before the first `Resized` otherwise.
        let is_surface_configured = size.width > 0 && size.height > 0;
        if is_surface_configured {
            surface.configure(&device, &config);
        }

        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            is_surface_configured,
            render_pipeline,
            vertex_buffer,
            index_buffer,
            num_indices: INDICES.len() as u32,
            texture_bind_group_layout,
            diffuse_texture,
            diffuse_bind_group,
            presented: false,
            capture_desc: None,
        })
    }

    /// Start showing a capture, sizing the texture to it.
    fn attach(&mut self, capture_desc: CaptureDesc) {
        let (width, height) = capture_desc.size();
        self.resize_texture(width, height);
        self.capture_desc = Some(capture_desc);
    }

    fn resize_texture(&mut self, width: u32, height: u32) {
        self.diffuse_texture = Texture::from_bytes(
            &self.device,
            &self.queue,
            &vec![255; 4 * width as usize * height as usize],
            width,
            height,
            Some("diffuse_texture"),
        )
        .expect("texture should be created");
        self.diffuse_bind_group = self
            .diffuse_texture
            .bind_group(&self.device, &self.texture_bind_group_layout);
    }

    fn resize(&mut self, width: u32, height: u32) {
        info!("Resizing to {width}x{height}");
        if width > 0 && height > 0 {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
            self.is_surface_configured = true;
        }
    }

    /// Returns false once the capture has ended, so the caller can leave the event loop.
    fn render(&mut self) -> bool {
        info!("Rendering frame");
        self.window.request_redraw();

        // We can't render unless the surface is configured
        if !self.is_surface_configured {
            return true;
        }

        // `get_current_texture` no longer returns a `Result`; every outcome, including
        // the ones that used to be a `SurfaceError`, is a `CurrentSurfaceTexture` variant.
        let (output, suboptimal) = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(output) => (output, false),
            // Usable, but the surface wants reconfiguring before the next frame.
            CurrentSurfaceTexture::Suboptimal(output) => (output, true),
            // Nothing to draw into right now; try again on the next redraw.
            CurrentSurfaceTexture::Timeout | CurrentSurfaceTexture::Occluded => return true,
            CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Lost => {
                warn!("Surface lost or outdated, reconfiguring...");
                let size = self.window.inner_size();
                self.resize(size.width, size.height);
                return true;
            }
            CurrentSurfaceTexture::Validation => {
                error!("Unable to render: surface validation error");
                return true;
            }
        };
        if !self.update() {
            return false;
        }
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Render Encoder"),
            });
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 1.0,
                            g: 1.0,
                            b: 1.0,
                            a: 0.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });

            render_pass.set_pipeline(&self.render_pipeline);
            render_pass.set_bind_group(0, &self.diffuse_bind_group, &[]);
            render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            render_pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);

            render_pass.draw_indexed(0..self.num_indices, 0, 0..1);
        }

        // submit will accept anything that implements IntoIter
        self.queue.submit([encoder.finish()]);
        self.queue.present(output);
        self.presented = true;

        // Reconfigure only once the frame is presented, so the surface is not
        // reconfigured while a texture acquired from it is still alive.
        if suboptimal {
            warn!("Surface suboptimal, reconfiguring...");
            let size = self.window.inner_size();
            self.resize(size.width, size.height);
        }
        true
    }

    fn handle_key(&self, event_loop: &ActiveEventLoop, code: KeyCode, is_pressed: bool) {
        #[allow(clippy::single_match)]
        match (code, is_pressed) {
            (KeyCode::Escape, true) => event_loop.exit(),
            _ => {}
        }
    }

    /// Returns false once the capture has ended.
    ///
    /// Never blocks: this runs on the event loop, so waiting for a frame here would freeze
    /// the window whenever delivery stalls -- which is exactly what happens when the capture
    /// is stopped from outside, e.g. macOS's "Stop Sharing".
    fn update(&mut self) -> bool {
        let Some(capture_desc) = &self.capture_desc else {
            return true;
        };
        let frame = match capture_desc.video().try_recv() {
            Ok(frame) => frame,
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => {
                info!("capture ended");
                return false;
            }
        };
        let VideoFrame { vframe, size, .. } = frame;
        // `write_texture` panics on an extent mismatch, and the source can resize mid-capture.
        if (size.0, size.1)
            != (
                self.diffuse_texture.texture_size.width,
                self.diffuse_texture.texture_size.height,
            )
        {
            info!("capture size changed to {size:?}");
            self.resize_texture(size.0, size.1);
        }
        self.queue.write_texture(
            // Tells wgpu where to copy the pixel data
            wgpu::TexelCopyTextureInfo {
                texture: &self.diffuse_texture.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            // The actual pixel data
            &vframe,
            // The layout of the texture
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * size.0),
                rows_per_image: Some(size.1),
            },
            self.diffuse_texture.texture_size,
        );
        true
    }
}

#[derive(Debug)]
pub struct Texture {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub sampler: wgpu::Sampler,
    pub texture_size: wgpu::Extent3d,
}

impl Texture {
    pub fn from_bytes(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bytes: &[u8],
        width: u32,
        height: u32,
        label: Option<&str>,
    ) -> Result<Self> {
        let texture_size = wgpu::Extent3d {
            width,
            height,
            // All textures are stored as 3D, we represent our 2D texture
            // by setting depth to 1.
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            size: texture_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Most images are stored using sRGB, so we need to reflect that here.
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            // TEXTURE_BINDING tells wgpu that we want to use this texture in shaders
            // COPY_DST means that we want to copy data to this texture
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            label,
            // This is the same as with the SurfaceConfig. It
            // specifies what texture formats can be used to
            // create TextureViews for this texture. The base
            // texture format (Bgra8UnormSrgb in this case) is
            // always supported. Note that using a different
            // texture format is not supported on the WebGL2
            // backend.
            view_formats: &[],
        });
        queue.write_texture(
            // Tells wgpu where to copy the pixel data
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            // The actual pixel data
            bytes,
            // The layout of the texture
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            texture_size,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Ok(Self {
            texture,
            view,
            sampler,
            texture_size,
        })
    }

    fn bind_group(&self, device: &wgpu::Device, layout: &wgpu::BindGroupLayout) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&self.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
            label: Some("diffuse_bind_group"),
        })
    }
}
