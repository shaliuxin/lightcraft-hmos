//! wgpu (GLES backend) renderer that drives egui on the HarmonyOS NativeWindow surface.
//!
//! The desktop build presents through eframe; here the host owns the `OHNativeWindow` handle
//! (via `raw_window_handle`) and creates a `wgpu::Surface` directly on the GL backend, because
//! the HarmonyOS NDK exposes only OpenGL ES. The vendored `wgpu-hal` supplies the OHOS EGL
//! presentation fixes.
use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, OhosNdkWindowHandle, WindowHandle,
};
use std::{ffi::c_void, ptr::NonNull, sync::Arc};

unsafe extern "C" {
    fn OH_NativeWindow_NativeObjectUnreference(object: *mut c_void) -> i32;
}

/// The XComponent callback already took a native reference before handing the handle to the
/// worker; dropping this returns it.
pub struct Window(pub usize);
impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: each Window owns exactly one reference taken at surface creation and is
        // dropped only after the surface borrowing it is destroyed.
        unsafe {
            OH_NativeWindow_NativeObjectUnreference(self.0 as *mut c_void);
        }
    }
}
impl HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(DisplayHandle::ohos())
    }
}
impl HasWindowHandle for Window {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        let ptr = NonNull::new(self.0 as *mut c_void)
            .ok_or(raw_window_handle::HandleError::Unavailable)?;
        // SAFETY: the borrowed handle cannot outlive the owned native reference.
        Ok(unsafe { WindowHandle::borrow_raw(OhosNdkWindowHandle::new(ptr).into()) })
    }
}

/// Whether the frame was successfully presented to the surface.
#[derive(Clone, Copy, PartialEq)]
pub enum Present {
    Presented,
    NeedsReconfigure,
    Skipped,
}

pub struct Render {
    pub surface: Option<wgpu::Surface<'static>>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub renderer: egui_wgpu::Renderer,
    pub config: wgpu::SurfaceConfiguration,
    pub adapter: wgpu::Adapter,
    pub instance: wgpu::Instance,
    pub window: Option<Arc<Window>>,
    /// GPU's max 2D texture dimension. egui's font atlas must be capped at this or the
    /// OHOS GLES backend rejects `create_texture` (limit is 8192 here, not 16384).
    pub max_texture_side: u32,
}

/// Build the wgpu surface directly over the OHOS NativeWindow using the explicit raw-handle
/// target (matching the working AniCraft reference on the same device): an OHOS display handle
/// plus the `OhosNdk` window.
///
/// The previous `create_surface(window)` (safe, trait-driven) path plus `power_preference:
/// NoPreference` was producing a surface whose frames reported `Presented` at 60fps yet never
/// reached the OHOS compositor (solid black screen) — the classic sign of a non-hardware GL
/// adapter rendering off-screen. The `create_surface_unsafe(RawHandle{ Ohos, OhosNdk })` shape
/// combined with a `HighPerformance` adapter is the proven-working configuration.
fn surface_for(instance: &wgpu::Instance, ptr: usize) -> Result<wgpu::Surface<'static>, String> {
    let non_null = NonNull::new(ptr as *mut c_void).ok_or("null OHNativeWindow pointer")?;
    let target = wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: Some(raw_window_handle::RawDisplayHandle::Ohos(
            raw_window_handle::OhosDisplayHandle::new(),
        )),
        raw_window_handle: raw_window_handle::RawWindowHandle::OhosNdk(
            raw_window_handle::OhosNdkWindowHandle::new(non_null),
        ),
    };
    // SAFETY: `ptr` is a live OHNativeWindow whose native reference is held by the surrounding
    // `Window` (owned via `Arc`); its lifetime outlives this surface. The C++ bridge took one
    // `OH_NativeWindow_NativeObjectReference` before handing the handle over.
    let surface = unsafe { instance.create_surface_unsafe(target) }
        .map_err(|e| format!("create_surface_unsafe failed: {e:?}"))?;
    Ok(surface)
}

impl Render {
    pub fn new(window: Arc<Window>, w: u32, h: u32) -> Result<Self, String> {
        fn stage(s: &str) {
            crate::bridge::notify(0, "stage", s, &[]);
        }
        stage("Render::new: creating instance (GL)");
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::GL,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        stage("Render::new: creating surface");
        let surface = surface_for(&instance, window.0)?;
        stage("Render::new: surface created, requesting adapter");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .map_err(|e| e.to_string())?;
        stage("Render::new: adapter ready, requesting device");
        // Cap egui's font atlas at the GPU's real 2D texture limit (8192 on this OHOS GLES
        // backend); a larger atlas fails `create_texture` validation.
        let max_texture_side = adapter.limits().max_texture_dimension_2d.max(512);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("LightCraft"),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|e| e.to_string())?;
        stage("Render::new: device ready, configuring surface");
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| matches!(f, wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm))
            .or_else(|| caps.formats.first().copied())
            .unwrap_or(wgpu::TextureFormat::Bgra8Unorm);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: w.max(1),
            height: h.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        stage("Render::new: surface configured, creating egui renderer");
        let renderer = egui_wgpu::Renderer::new(&device, format, Default::default());
        stage("Render::new: complete");
        Ok(Self {
            surface: Some(surface),
            device,
            queue,
            renderer,
            config,
            adapter,
            instance,
            window: Some(window),
            max_texture_side,
        })
    }

    pub fn resize(&mut self, w: u32, h: u32) -> Result<(), String> {
        if w == 0 || h == 0 {
            return Ok(());
        }
        self.config.width = w;
        self.config.height = h;
        if let Some(surface) = &self.surface {
            surface.configure(&self.device, &self.config);
        }
        Ok(())
    }

    /// Tear down the surface but keep the device/queue for the next attach.
    pub fn detach(&mut self) {
        self.surface = None;
        self.window = None;
    }

    pub fn attach(&mut self, window: Arc<Window>, w: u32, h: u32) -> Result<(), String> {
        let surface = surface_for(&self.instance, window.0)?;
        self.surface = Some(surface);
        self.window = Some(window);
        self.resize(w.max(1), h.max(1))
    }

    pub fn attached(&self) -> bool {
        self.surface.is_some()
    }

    /// Present one egui frame to the surface.
    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        textures_delta: &egui::epaint::textures::TexturesDelta,
        shapes: Vec<egui::epaint::ClippedShape>,
        pixels_per_point: f32,
    ) -> Result<Present, String> {
        let surface = match self.surface.as_ref() {
            Some(s) => s,
            None => return Ok(Present::Skipped),
        };
        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                return Ok(Present::NeedsReconfigure)
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(Present::Skipped)
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err("native surface validation failed".into())
            }
        };
        let jobs = ctx.tessellate(shapes, pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point,
        };
        // Upload egui's texture atlas (glyphs, icons, images) before drawing. egui-wgpu keeps
        // texture uploads SEPARATE from `update_buffers`; the working AniCraft reference calls
        // these for every delta. Without them every textured shape samples an empty atlas and
        // renders invisible, so only the clear color is visible (the black-screen symptom).
        for (tex_id, deltas) in &textures_delta.set {
            for delta in deltas.iter() {
                self.renderer.update_texture(&self.device, &self.queue, *tex_id, delta);
            }
        }
        for tex_id in &textures_delta.free {
            self.renderer.free_texture(tex_id);
        }
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let mut commands = self.renderer.update_buffers(&self.device, &self.queue, &mut encoder, &jobs, &screen);
        let view = frame.texture.create_view(&Default::default());
        {
            let attachments = [Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.11, g: 0.11, b: 0.12, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("LightCraft UI"),
                color_attachments: &attachments,
                ..Default::default()
            });
            self.renderer.render(&mut pass.forget_lifetime(), &jobs, &screen);
        }
        commands.push(encoder.finish());
        self.queue.submit(commands);
        self.queue.present(frame);
        Ok(Present::Presented)
    }
}
