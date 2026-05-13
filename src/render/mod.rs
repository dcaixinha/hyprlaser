//! GPU rendering.
//!
//! One `Renderer` per `wl_output`. Each renderer owns its own wgpu
//! surface/device/queue/pipeline and renders the laser trail clipped
//! to its output's pixel rectangle. The trail itself lives outside
//! (in `crate::trail::TrailBuffer`) so all renderers share the same
//! global cursor history — that's what makes the trail look continuous
//! when the cursor crosses monitors.
//!
//! ## Lifetime story
//!
//! `AppState` owns the `wayland_client::Connection` and each
//! `LayerSurface`, both of which keep the underlying `wl_display` and
//! `wl_surface` alive. We construct a `wgpu::Surface<'static>` via
//! `create_surface_unsafe` using raw pointers borrowed from those
//! objects. The `'static` lifetime is a soundness *promise* we make on
//! the basis that the renderer never outlives `AppState`. Struct field
//! ordering in `AppState`'s output context keeps that promise.

use std::{ptr::NonNull, time::Instant};

use bytemuck::{Pod, Zeroable};

use crate::{
    config::LaserConfig,
    trail::{TrailSample, MAX_SAMPLES},
};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use wayland_client::{protocol::wl_surface, Connection, Proxy};
use wgpu::{
    util::DeviceExt, Backends, BindGroup, BindGroupDescriptor, BindGroupEntry,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, Buffer,
    BufferBindingType, BufferUsages, Color, ColorTargetState, ColorWrites,
    CommandEncoderDescriptor, CompositeAlphaMode, Device, DeviceDescriptor, FragmentState,
    Instance, InstanceDescriptor, LoadOp, MultisampleState, Operations, PipelineLayoutDescriptor,
    PresentMode, PrimitiveState, Queue, RenderPassColorAttachment, RenderPassDescriptor,
    RenderPipeline, RenderPipelineDescriptor, RequestAdapterOptions, ShaderModuleDescriptor,
    ShaderSource, ShaderStages, StoreOp, Surface, SurfaceConfiguration, SurfaceTargetUnsafe,
    TextureUsages, TextureViewDescriptor, VertexState,
};

/// Number of cursor-position samples kept in the motion-trail ring
/// buffer. Must match `TRAIL_SAMPLES` in `shader.wgsl` and
/// `trail::MAX_SAMPLES`. Bumped to 32 so a ~500 ms streak (at our
/// 120 Hz IPC poll rate) is well-sampled.
pub const TRAIL_SAMPLES: usize = MAX_SAMPLES;

/// CPU-side mirror of the WGSL `Uniforms` struct. Must match `shader.wgsl`
/// in layout and field order.
///
/// Layout (cumulative offsets in bytes):
///   0   resolution           vec2  (align 8)
///   8   dot_radius           f32
///   12  edge_softness        f32
///   16  tail_half_width      f32
///   20  _pad                 f32
///   24  trail_len            u32
///   28  _pad2                u32
///   32  color                vec4  (align 16 — satisfied)
///   48  trail_bounds_min     vec2  (align 8 — satisfied)
///   56  trail_bounds_max     vec2  (align 8 — satisfied)
///   64  trail[32]            array<vec4>  (align 16 — satisfied)
///   576 end
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
struct LaserUniform {
    resolution: [f32; 2],
    dot_radius: f32,
    edge_softness: f32,
    tail_half_width: f32,
    _pad: f32,
    trail_len: u32,
    _pad2: u32,
    /// Linear-RGB color (alpha component unused; we drive alpha from
    /// the head dot / streak coverage).
    color: [f32; 4],
    /// AABB around the entire trail polyline, expanded by
    /// `dot_radius + edge_softness`. Lets the shader skip the
    /// per-segment loop entirely for pixels nowhere near the streak.
    trail_bounds_min: [f32; 2],
    trail_bounds_max: [f32; 2],
    /// Each entry is `(x, y, age, _unused)`. Index 0 is the newest
    /// sample (this frame). Higher indices are older.
    trail: [[f32; 4]; TRAIL_SAMPLES],
}

pub struct Renderer {
    // Field-order matters: the surface (and everything it owns inside
    // wgpu) must be dropped before `_connection`-equivalent state that
    // owns the raw pointers. `device`/`queue` hold no Wayland-facing
    // resources, but `surface` does.
    surface: Surface<'static>,
    device: Device,
    queue: Queue,
    surface_config: SurfaceConfiguration,

    pipeline: RenderPipeline,
    uniform_buffer: Buffer,
    bind_group: BindGroup,

    // Visual parameters — all CLI-configurable; see `LaserConfig`.
    /// Dot radius in **logical** pixels (multiplied by `scale` before
    /// upload so the dot stays the same physical size on HiDPI).
    dot_radius: f32,
    /// Half-width of the streak at the oldest sample (tail tip), in
    /// logical pixels. Same scaling rule as `dot_radius`.
    tail_half_width: f32,
    /// Laser color in linear RGB.
    color: [f32; 3],

    /// Origin of our output in global **logical** compositor coords.
    /// Subtracted from each trail sample before scaling.
    output_origin: (i32, i32),
    /// HiDPI scale of this output. 1.0 for non-HiDPI; can be
    /// fractional (1.25, 1.5, 1.75) via wp_fractional_scale_v1 or
    /// integer (2, 3) via wl_output's scale event.
    scale: f64,
}

impl Renderer {
    pub fn new(
        conn: &Connection,
        wl_surface: &wl_surface::WlSurface,
        width: u32,
        height: u32,
        output_origin: (i32, i32),
        scale: f64,
        config: &LaserConfig,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Force Vulkan only. Letting wgpu fall back to GL is bad for us:
        // the GLES backend on Mesa advertises `max_texture_dimension_2d = 2048`,
        // which is smaller than many modern monitors (e.g. 5120x1440
        // ultrawides) — and we render a fullscreen-sized swapchain texture,
        // so configuring the surface would panic.
        let instance = Instance::new(InstanceDescriptor {
            backends: Backends::VULKAN,
            ..Default::default()
        });

        let display_ptr = conn.backend().display_ptr() as *mut _;
        let display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(display_ptr).ok_or("wl_display pointer was null")?,
        ));

        let surface_ptr = wl_surface.id().as_ptr() as *mut _;
        let window_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(surface_ptr).ok_or("wl_surface pointer was null")?,
        ));

        // SAFETY: the raw pointers above point into objects owned by the
        // `AppState` that also owns this `Renderer`, and the struct field
        // ordering in `AppState` guarantees `Renderer` is dropped first.
        let surface = unsafe {
            instance.create_surface_unsafe(SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: display_handle,
                raw_window_handle: window_handle,
            })?
        };

        let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .ok_or("no compatible GPU adapter found for wgpu")?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &DeviceDescriptor {
                label: Some("hyprlaser device"),
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))?;

        let caps = surface.get_capabilities(&adapter);

        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| matches!(f, wgpu::TextureFormat::Bgra8UnormSrgb))
            .or_else(|| caps.formats.first().copied())
            .ok_or("surface advertises no formats")?;

        let alpha_mode = if caps
            .alpha_modes
            .contains(&CompositeAlphaMode::PreMultiplied)
        {
            CompositeAlphaMode::PreMultiplied
        } else if caps
            .alpha_modes
            .contains(&CompositeAlphaMode::PostMultiplied)
        {
            CompositeAlphaMode::PostMultiplied
        } else {
            return Err(format!(
                "compositor doesn't advertise a per-pixel alpha mode; got {:?}",
                caps.alpha_modes
            )
            .into());
        };

        let surface_config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode: PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        log::info!(
            "wgpu surface ready: {}x{}, format={:?}, alpha_mode={:?}",
            surface_config.width,
            surface_config.height,
            format,
            alpha_mode
        );

        // ── Pipeline setup ───────────────────────────────────────────────
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("hyprlaser shader"),
            source: ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let dot_radius = config.dot_radius;
        let tail_half_width = config.tail_half_width;
        let color_rgb = config.color_linear;

        let initial_uniform = LaserUniform {
            resolution: [surface_config.width as f32, surface_config.height as f32],
            dot_radius,
            edge_softness: 1.0,
            tail_half_width,
            _pad: 0.0,
            trail_len: 0,
            _pad2: 0,
            color: [color_rgb[0], color_rgb[1], color_rgb[2], 1.0],
            // Degenerate bounds: max < min so the shader rejects every
            // pixel until we have real samples to bound.
            trail_bounds_min: [f32::INFINITY, f32::INFINITY],
            trail_bounds_max: [f32::NEG_INFINITY, f32::NEG_INFINITY],
            trail: [[0.0; 4]; TRAIL_SAMPLES],
        };

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("hyprlaser uniform buffer"),
            contents: bytemuck::bytes_of(&initial_uniform),
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("hyprlaser bind group layout"),
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::FRAGMENT,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("hyprlaser bind group"),
            layout: &bind_group_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("hyprlaser pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("hyprlaser pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: "vs_main",
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: "fs_main",
                compilation_options: Default::default(),
                targets: &[Some(ColorTargetState {
                    format,
                    // Premultiplied alpha: src already encodes color*alpha,
                    // so we use straight `(src) + (1 - src.a) * dst`.
                    blend: Some(BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            primitive: PrimitiveState::default(),
            depth_stencil: None,
            multisample: MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Ok(Self {
            surface,
            device,
            queue,
            surface_config,
            pipeline,
            uniform_buffer,
            bind_group,
            dot_radius,
            tail_half_width,
            color: color_rgb,
            output_origin,
            scale,
        })
    }

    /// Update the cached output origin. Called from `AppState` when the
    /// compositor moves an output (rearrange in Hyprland settings, etc).
    pub fn set_output_origin(&mut self, origin: (i32, i32)) {
        self.output_origin = origin;
    }

    /// Update the HiDPI scale. Called when the compositor sends us a
    /// new `wp_fractional_scale_v1.preferred_scale` or
    /// `wl_output.scale` event for our output.
    pub fn set_scale(&mut self, scale: f64) {
        self.scale = scale;
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if width == self.surface_config.width && height == self.surface_config.height {
            return;
        }
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    /// Serialize a shared global trail into a fixed-size uniform array
    /// of *physical* surface-local sample positions, plus the polyline
    /// AABB used by the shader for early-out.
    ///
    /// Coordinate path: trail samples live in global **logical** coords
    /// (Hyprland's `cursorpos`). We subtract the output's logical
    /// origin → surface-local logical pixels → multiply by `scale` →
    /// physical fragment-buffer pixels (which is what the shader sees
    /// via `@builtin(position)`). The same scale is applied to
    /// `dot_radius` and `tail_half_width` so the dot appears the same
    /// physical size regardless of monitor DPI.
    fn build_uniform(
        &self,
        samples: &[TrailSample],
        trail_duration: std::time::Duration,
        now: Instant,
    ) -> LaserUniform {
        let mut trail = [[0.0f32; 4]; TRAIL_SAMPLES];
        let trail_secs = trail_duration.as_secs_f32().max(0.001);
        let ox = self.output_origin.0 as f32;
        let oy = self.output_origin.1 as f32;
        let scale = self.scale as f32;

        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        let mut count = 0usize;

        for sample in samples.iter().take(TRAIL_SAMPLES) {
            // logical → surface-local logical → physical
            let lx = (sample.pos.0 - ox) * scale;
            let ly = (sample.pos.1 - oy) * scale;
            let age = now.duration_since(sample.timestamp).as_secs_f32() / trail_secs;
            trail[count] = [lx, ly, age.clamp(0.0, 1.0), 0.0];
            min_x = min_x.min(lx);
            min_y = min_y.min(ly);
            max_x = max_x.max(lx);
            max_y = max_y.max(ly);
            count += 1;
        }

        // Scale the visual sizes too, so a 5-px logical dot is 5 px
        // logical (~7.5 physical at scale 1.5) on screen rather than
        // 5 physical pixels (which would look tiny on HiDPI).
        let dot_radius_px = self.dot_radius * scale;
        let tail_half_width_px = self.tail_half_width * scale;
        // edge_softness stays at 1 *physical* pixel so the AA band is
        // always one screen pixel wide.
        let edge_softness_px = 1.0;

        // Pad the bounds by the maximum half-width so the AABB fully
        // contains the visible streak (including its anti-aliased edge).
        let pad = dot_radius_px + edge_softness_px;
        let (bounds_min, bounds_max) = if count == 0 {
            // No samples yet — degenerate box rejects everything.
            (
                [f32::INFINITY, f32::INFINITY],
                [f32::NEG_INFINITY, f32::NEG_INFINITY],
            )
        } else {
            ([min_x - pad, min_y - pad], [max_x + pad, max_y + pad])
        };

        LaserUniform {
            resolution: [
                self.surface_config.width as f32,
                self.surface_config.height as f32,
            ],
            dot_radius: dot_radius_px,
            edge_softness: edge_softness_px,
            tail_half_width: tail_half_width_px,
            _pad: 0.0,
            trail_len: count as u32,
            _pad2: 0,
            color: [self.color[0], self.color[1], self.color[2], 1.0],
            trail_bounds_min: bounds_min,
            trail_bounds_max: bounds_max,
            trail,
        }
    }

    /// Render a frame for this output, using `samples` (the shared
    /// global trail) translated into surface-local coordinates.
    pub fn draw(
        &mut self,
        samples: &[TrailSample],
        trail_duration: std::time::Duration,
        now: Instant,
    ) -> Result<(), wgpu::SurfaceError> {
        let uniform = self.build_uniform(samples, trail_duration, now);
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));

        let frame = self.surface.get_current_texture()?;
        let view = frame.texture.create_view(&TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("hyprlaser frame encoder"),
            });

        {
            let mut rpass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("hyprlaser dot pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: Operations {
                        // Clear to fully transparent — every pixel that
                        // isn't the dot stays see-through.
                        load: LoadOp::Clear(Color::TRANSPARENT),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rpass.set_pipeline(&self.pipeline);
            rpass.set_bind_group(0, &self.bind_group, &[]);
            rpass.draw(0..3, 0..1);
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }
}
