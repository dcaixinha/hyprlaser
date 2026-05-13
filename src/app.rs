//! SCTK app: spawns one wlr-layer-shell `Overlay` surface per `wl_output`
//! so the laser is always visible wherever the cursor is, including
//! across monitor boundaries.
//!
//! HiDPI handling:
//!
//! - If the compositor advertises `wp_fractional_scale_manager_v1` and
//!   `wp_viewporter`, each surface gets a `wp_fractional_scale_v1`
//!   object plus a `wp_viewport`. The compositor sends a
//!   `preferred_scale` event with the actual scale as numerator over
//!   120 (e.g. 180 = 1.5x), and we render the buffer at
//!   `logical_size * scale` physical pixels while declaring the
//!   logical box via `viewport.set_destination`.
//! - Otherwise we fall back to integer scaling via SCTK's
//!   `wl_output.scale` (delivered as `scale_factor_changed`), which
//!   maps to `wl_surface.set_buffer_scale(N)` and a buffer of
//!   `logical_size * N` physical pixels.
//!
//! The trail history is shared across all surfaces (it's owned by the
//! `TrailBuffer` that the IPC thread writes to). Each output's
//! `Renderer` translates the shared global-coords trail into its own
//! surface-local physical pixels at draw time.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
};
use wayland_client::{
    globals::{registry_queue_init, GlobalList},
    protocol::{wl_output, wl_region, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

use crate::{
    config::LaserConfig, cursor_hide::CursorHider, hypr_ipc, render::Renderer, trail::TrailBuffer,
};

/// Default cursor-polling rate. 120 Hz keeps end-to-end latency under
/// ~8 ms while staying well below the cost where the polling thread
/// would meaningfully affect Hyprland's main loop.
const CURSOR_POLL_HZ: u32 = 120;

/// Per-output state: one Wayland layer surface, (lazily) one wgpu
/// renderer, and the scaling plumbing per `wl_output`.
struct OutputContext {
    /// The renderer is `None` until the first `configure` for this
    /// surface tells us the actual pixel size.
    renderer: Option<Renderer>,
    layer: LayerSurface,
    output: wl_output::WlOutput,
    /// Origin of this output in global compositor coordinates. Cached
    /// for fast per-frame use; refreshed on `update_output`.
    origin: (i32, i32),
    /// Logical pixel size of this surface, from `configure`.
    logical_width: u32,
    logical_height: u32,
    /// HiDPI scale. Starts at 1.0; updated by either
    /// `wp_fractional_scale_v1.preferred_scale` (fractional path) or
    /// `wl_output.scale` (integer fallback).
    scale: f64,
    /// Optional fractional-scale + viewport objects (present only if
    /// the compositor exposes those globals).
    _fractional_scale: Option<WpFractionalScaleV1>,
    viewport: Option<WpViewport>,
    /// True until we've consumed the first `configure` event.
    first_configure: bool,
}

/// Entry point used by `main.rs`. Returns once all surfaces are closed,
/// the user hits Ctrl+C, or an unrecoverable error occurs.
pub fn run(config: LaserConfig) -> Result<(), Box<dyn std::error::Error>> {
    // Shared trail buffer. The IPC thread pushes samples; the main
    // thread reads them each frame to render the streak.
    let trail = Arc::new(Mutex::new(TrailBuffer::new(config.trail_duration)));

    // Start the Hyprland cursor poller before touching Wayland — if
    // we're not in a Hyprland session, fail fast with a clear error.
    let (_cursor, _ipc_thread) = hypr_ipc::spawn(CURSOR_POLL_HZ, Arc::clone(&trail))?;

    // Ctrl+C handler. Installed before CursorHider so signals during
    // early startup still trigger proper cleanup via Drop.
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_handler = Arc::clone(&shutdown);
    ctrlc::set_handler(move || {
        log::info!("Ctrl+C received, shutting down");
        shutdown_handler.store(true, Ordering::Relaxed);
    })
    .map_err(|e| format!("failed to install Ctrl+C handler: {e}"))?;

    // Hide the OS cursor unless the user opted out. Held until `run`
    // returns so its Drop restores the cursor on every exit path.
    let _cursor_hider = if config.hide_cursor {
        Some(CursorHider::hide()?)
    } else {
        None
    };

    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| format!("wl_compositor not available: {e}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|e| format!("zwlr_layer_shell_v1 not available: {e}"))?;

    // Optional: fractional scale + viewporter. Both must be present to
    // use the fractional path; otherwise we fall back to integer scale.
    let fractional_mgr = try_bind::<WpFractionalScaleManagerV1>(&globals, &qh);
    let viewporter = try_bind::<WpViewporter>(&globals, &qh);
    let hidpi_mode = match (&fractional_mgr, &viewporter) {
        (Some(_), Some(_)) => HiDpiMode::Fractional,
        _ => HiDpiMode::IntegerOrNone,
    };
    log::info!("HiDPI mode: {hidpi_mode:?}");

    let mut state = AppState {
        conn: conn.clone(),
        compositor,
        layer_shell,
        fractional_mgr,
        viewporter,
        hidpi_mode,
        outputs: HashMap::new(),
        fractional_to_output: HashMap::new(),
        config,
        trail,
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        exit: false,
    };

    // CRITICAL: before we enumerate outputs and create surfaces, do a
    // roundtrip so the compositor sends us each output's geometry
    // (position + size). SCTK's `OutputState::info()` returns `None` or
    // partial info until the compositor's burst of `wl_output.geometry`
    // and `xdg_output.logical_position` events has been dispatched.
    event_queue.roundtrip(&mut state)?;

    // Seed surfaces from outputs already known at startup. Outputs
    // hot-plugged later are handled by `OutputHandler::new_output`.
    let known: Vec<_> = state.output_state.outputs().collect();
    for output in known {
        state.add_output_surface(&qh, output);
    }

    while !state.exit && !shutdown.load(Ordering::Relaxed) {
        event_queue.blocking_dispatch(&mut state)?;
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum HiDpiMode {
    /// Fall back to integer scaling via wl_surface.set_buffer_scale
    /// (uses `wl_output.scale`). Works on any compositor but only
    /// supports whole-number scales.
    IntegerOrNone,
    /// wp_fractional_scale_v1 + wp_viewporter available — use them.
    Fractional,
}

/// Best-effort `GlobalList::bind` for an optional protocol. Returns
/// `None` if the global isn't advertised — most importantly so we don't
/// fail on compositors without the staging protocols.
fn try_bind<I>(globals: &GlobalList, qh: &QueueHandle<AppState>) -> Option<I>
where
    I: Proxy + 'static,
    AppState: Dispatch<I, ()>,
{
    globals
        .bind::<I, _, _>(qh, 1..=1, ())
        .map_err(|e| {
            log::info!(
                "optional global {} unavailable: {e}",
                std::any::type_name::<I>()
            )
        })
        .ok()
}

/// Application state, shared across SCTK delegate impls.
struct AppState {
    conn: Connection,
    compositor: CompositorState,
    layer_shell: LayerShell,

    fractional_mgr: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    hidpi_mode: HiDpiMode,

    /// One overlay per output, keyed by the `wl_output` proxy id.
    outputs: HashMap<u32, OutputContext>,

    /// Reverse map: from a `wp_fractional_scale_v1` proxy id to its
    /// owning output's id. Used in the fractional-scale event handler
    /// to find which output to update.
    fractional_to_output: HashMap<u32, u32>,

    config: LaserConfig,
    trail: Arc<Mutex<TrailBuffer>>,

    registry_state: RegistryState,
    output_state: OutputState,

    exit: bool,
}

impl AppState {
    /// Look up the (logical) origin of `output` in global compositor
    /// coordinates. Falls back to physical `location` if the compositor
    /// hasn't sent an `xdg_output` description, and to `(0, 0)` if the
    /// output is unknown.
    fn origin_of(&self, output: &wl_output::WlOutput) -> (i32, i32) {
        match self.output_state.info(output) {
            Some(info) => info.logical_position.unwrap_or(info.location),
            None => (0, 0),
        }
    }

    /// Initial integer scale guess for an output, used when we don't
    /// have a fractional-scale stream yet (either because the
    /// compositor doesn't expose the protocol, or because the very
    /// first frame is being rendered before any preferred_scale event
    /// has arrived).
    fn initial_scale_of(&self, output: &wl_output::WlOutput) -> f64 {
        self.output_state
            .info(output)
            .map(|i| i.scale_factor as f64)
            .unwrap_or(1.0)
    }

    /// Create a new layer-shell surface bound to `output`, attaching
    /// the optional viewport + fractional-scale objects. Idempotent.
    fn add_output_surface(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let id = output.id().protocol_id();
        if self.outputs.contains_key(&id) {
            return;
        }

        let origin = self.origin_of(&output);
        let initial_scale = self.initial_scale_of(&output);
        let info_name = self
            .output_state
            .info(&output)
            .and_then(|i| i.name)
            .unwrap_or_else(|| format!("wl_output#{id}"));
        log::info!(
            "creating overlay for output {info_name} at global origin ({}, {}), \
             initial scale {initial_scale}",
            origin.0,
            origin.1,
        );

        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("hyprlaser"),
            // CRUCIAL for multi-output: explicitly bind to this output.
            Some(&output),
        );

        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);

        // Click-through: empty input region.
        let empty_region = self.compositor.wl_compositor().create_region(qh, ());
        layer.set_input_region(Some(&empty_region));
        empty_region.destroy();

        // HiDPI plumbing. Two mutually-exclusive paths:
        let (fractional_scale, viewport) = match self.hidpi_mode {
            HiDpiMode::Fractional => {
                let mgr = self.fractional_mgr.as_ref().unwrap();
                let vp_mgr = self.viewporter.as_ref().unwrap();
                let fs = mgr.get_fractional_scale(layer.wl_surface(), qh, ());
                let vp = vp_mgr.get_viewport(layer.wl_surface(), qh, ());
                // Remember which output this fractional-scale belongs
                // to so the event handler can route the preferred_scale
                // update back to us.
                self.fractional_to_output.insert(fs.id().protocol_id(), id);
                (Some(fs), Some(vp))
            }
            HiDpiMode::IntegerOrNone => {
                // Integer fallback: tell the compositor our buffer
                // resolution is `initial_scale`× the surface size.
                if (initial_scale - initial_scale.round()).abs() < f64::EPSILON {
                    layer.wl_surface().set_buffer_scale(initial_scale as i32);
                }
                (None, None)
            }
        };

        layer.commit();

        self.outputs.insert(
            id,
            OutputContext {
                renderer: None,
                layer,
                output,
                origin,
                logical_width: 0,
                logical_height: 0,
                scale: initial_scale,
                _fractional_scale: fractional_scale,
                viewport,
                first_configure: true,
            },
        );
    }

    /// Remove the surface and renderer for `output`. Called from
    /// `OutputHandler::output_destroyed`.
    fn remove_output_surface(&mut self, output: &wl_output::WlOutput) {
        let id = output.id().protocol_id();
        if let Some(ctx) = self.outputs.remove(&id) {
            log::info!("removing overlay for output wl_output#{id}");
            // Clean up the reverse fractional-scale map too.
            self.fractional_to_output.retain(|_, v| *v != id);
            drop(ctx);
        }
    }

    /// Find the output context whose layer surface owns `surface`.
    fn ctx_for_surface(&mut self, surface: &wl_surface::WlSurface) -> Option<&mut OutputContext> {
        self.outputs
            .values_mut()
            .find(|ctx| ctx.layer.wl_surface() == surface)
    }

    /// Find the output context for the given layer surface.
    fn ctx_for_layer(&mut self, layer: &LayerSurface) -> Option<&mut OutputContext> {
        self.outputs
            .values_mut()
            .find(|ctx| ctx.layer.wl_surface() == layer.wl_surface())
    }

    /// Compute the physical buffer size for an output context.
    fn physical_size(ctx: &OutputContext) -> (u32, u32) {
        let pw = (ctx.logical_width as f64 * ctx.scale).round() as u32;
        let ph = (ctx.logical_height as f64 * ctx.scale).round() as u32;
        (pw.max(1), ph.max(1))
    }

    /// Render one frame for the output context with id `id`, and
    /// schedule its next frame callback.
    fn draw_output(&mut self, qh: &QueueHandle<Self>, id: u32) {
        let now = Instant::now();
        let (samples_snapshot, trail_duration) = {
            let tb = match self.trail.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let mut buf: Vec<crate::trail::TrailSample> = Vec::with_capacity(tb.samples().len());
            buf.extend_from_slice(tb.samples());
            (buf, tb.duration())
        };

        let Some(ctx) = self.outputs.get_mut(&id) else {
            return;
        };
        // Compute the physical size up front so we can both reborrow
        // the renderer mutably *and* know the size in case it needs
        // resizing after a swapchain-lost error.
        let (pw, ph) = Self::physical_size(ctx);
        let Some(renderer) = ctx.renderer.as_mut() else {
            return;
        };

        // Re-request a frame callback for *this* surface so we keep
        // being woken at its refresh rate.
        let wl_surface = ctx.layer.wl_surface();
        wl_surface.frame(qh, wl_surface.clone());

        let result = renderer.draw(&samples_snapshot, trail_duration, now);
        match result {
            Ok(()) => {}
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                renderer.resize(pw, ph);
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                log::error!("wgpu reported OutOfMemory; exiting");
                self.exit = true;
            }
            Err(err) => {
                log::warn!("frame render failed: {err:?}");
            }
        }
    }

    /// Apply a new scale value to the named output. If the scale
    /// actually changed and we have a renderer, reconfigure the wgpu
    /// surface at the new physical size and tell the renderer.
    fn set_output_scale(&mut self, id: u32, scale: f64) {
        let Some(ctx) = self.outputs.get_mut(&id) else {
            return;
        };
        if (ctx.scale - scale).abs() < 1e-6 {
            return;
        }
        log::info!(
            "output wl_output#{id} scale updated: {:.3} -> {:.3}",
            ctx.scale,
            scale
        );
        ctx.scale = scale;

        // For the integer-fallback path, propagate to the wl_surface.
        if matches!(self.hidpi_mode, HiDpiMode::IntegerOrNone)
            && (scale - scale.round()).abs() < f64::EPSILON
        {
            ctx.layer.wl_surface().set_buffer_scale(scale as i32);
        }

        // If we already have a renderer, resize its swapchain to the
        // new physical size and propagate scale into uniforms.
        let (pw, ph) = Self::physical_size(ctx);
        if let Some(r) = ctx.renderer.as_mut() {
            r.set_scale(scale);
            r.resize(pw, ph);
        }
        // Re-apply viewport destination on the fractional path so the
        // logical box stays consistent across scale changes.
        if let Some(vp) = ctx.viewport.as_ref() {
            vp.set_destination(ctx.logical_width as i32, ctx.logical_height as i32);
            ctx.layer.commit();
        }
    }
}

// ── SCTK handler trait impls ────────────────────────────────────────────────

impl CompositorHandler for AppState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        // SCTK delivers this when wl_output.scale or wl_surface enter/
        // leave events imply a new integer scale for the surface.
        // We only act on it if we're in the integer-fallback HiDPI mode;
        // in fractional mode the canonical value comes through the
        // wp_fractional_scale_v1 protocol instead.
        if !matches!(self.hidpi_mode, HiDpiMode::IntegerOrNone) {
            return;
        }
        if let Some(ctx) = self.ctx_for_surface(surface) {
            let id = ctx.output.id().protocol_id();
            self.set_output_scale(id, new_factor.max(1) as f64);
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        let id_opt = self
            .ctx_for_surface(surface)
            .map(|ctx| ctx.output.id().protocol_id());
        if let Some(id) = id_opt {
            self.draw_output(qh, id);
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.add_output_surface(qh, output);
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let id = output.id().protocol_id();
        let new_origin = self.origin_of(&output);
        if let Some(ctx) = self.outputs.get_mut(&id) {
            if ctx.origin != new_origin {
                log::info!(
                    "output wl_output#{id} origin updated: ({}, {}) -> ({}, {})",
                    ctx.origin.0,
                    ctx.origin.1,
                    new_origin.0,
                    new_origin.1
                );
                ctx.origin = new_origin;
                if let Some(r) = ctx.renderer.as_mut() {
                    r.set_output_origin(new_origin);
                }
            }
        }

        // Integer scale may also have changed (mode change with new
        // wl_output.scale value). Only relevant in the integer path;
        // the fractional path gets its number through preferred_scale.
        if matches!(self.hidpi_mode, HiDpiMode::IntegerOrNone) {
            let new_scale = self.initial_scale_of(&output);
            self.set_output_scale(id, new_scale);
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.remove_output_surface(&output);
    }
}

impl LayerShellHandler for AppState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        let id_opt = self
            .ctx_for_layer(layer)
            .map(|ctx| ctx.output.id().protocol_id());
        if let Some(id) = id_opt {
            self.outputs.remove(&id);
            self.fractional_to_output.retain(|_, v| *v != id);
            if self.outputs.is_empty() {
                log::info!("all overlay surfaces closed by compositor; exiting");
                self.exit = true;
            }
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let id_opt = self
            .ctx_for_layer(layer)
            .map(|ctx| ctx.output.id().protocol_id());
        let Some(id) = id_opt else {
            return;
        };

        let (w, h) = configure.new_size;
        // Configure values are in **logical** pixels.
        let logical_width = if w == 0 { 1920 } else { w };
        let logical_height = if h == 0 { 1080 } else { h };

        // Re-read the output's origin (post-roundtrip the compositor
        // has definitely sent it).
        let fresh_origin = {
            let ctx = self.outputs.get(&id).unwrap();
            self.origin_of(&ctx.output)
        };

        let (need_init, origin, scale, pw, ph) = {
            let ctx = self.outputs.get_mut(&id).unwrap();
            ctx.logical_width = logical_width;
            ctx.logical_height = logical_height;
            if ctx.origin != fresh_origin {
                log::info!(
                    "output wl_output#{id} origin corrected on configure: ({}, {}) -> ({}, {})",
                    ctx.origin.0,
                    ctx.origin.1,
                    fresh_origin.0,
                    fresh_origin.1
                );
                ctx.origin = fresh_origin;
            }
            // On the fractional path, set the viewport destination to
            // the logical surface size every configure — that's what
            // tells the compositor how big our logical surface is
            // even though the buffer is at physical resolution.
            if let Some(vp) = ctx.viewport.as_ref() {
                vp.set_destination(logical_width as i32, logical_height as i32);
            }
            let init = ctx.first_configure;
            ctx.first_configure = false;
            let (pw, ph) = Self::physical_size(ctx);
            (init, ctx.origin, ctx.scale, pw, ph)
        };

        log::info!(
            "configure output#{id}: logical {}x{}, scale {:.3}, physical {}x{}",
            logical_width,
            logical_height,
            scale,
            pw,
            ph,
        );

        if need_init {
            let renderer = {
                let ctx = self.outputs.get(&id).unwrap();
                Renderer::new(
                    &self.conn,
                    ctx.layer.wl_surface(),
                    pw,
                    ph,
                    origin,
                    scale,
                    &self.config,
                )
            };
            match renderer {
                Ok(r) => {
                    self.outputs.get_mut(&id).unwrap().renderer = Some(r);
                }
                Err(err) => {
                    log::error!("failed to initialize wgpu renderer for output {id}: {err}");
                    self.outputs.remove(&id);
                    if self.outputs.is_empty() {
                        self.exit = true;
                        return;
                    }
                }
            }
        } else if let Some(ctx) = self.outputs.get_mut(&id) {
            if let Some(r) = ctx.renderer.as_mut() {
                r.resize(pw, ph);
                r.set_output_origin(origin);
                r.set_scale(scale);
            }
        }

        self.draw_output(qh, id);
    }
}

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(AppState);
delegate_output!(AppState);
delegate_layer!(AppState);
delegate_registry!(AppState);

// ── Dispatch impls for protocols we use directly (without SCTK wrappers) ───

// wl_region: no events.
impl Dispatch<wl_region::WlRegion, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<AppState>,
    ) {
    }
}

// wp_viewporter: no events (the manager singleton).
impl Dispatch<WpViewporter, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<AppState>,
    ) {
    }
}

// wp_viewport: no events.
impl Dispatch<WpViewport, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: <WpViewport as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<AppState>,
    ) {
    }
}

// wp_fractional_scale_manager_v1: no events.
impl Dispatch<WpFractionalScaleManagerV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &WpFractionalScaleManagerV1,
        _: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<AppState>,
    ) {
    }
}

// wp_fractional_scale_v1: receives preferred_scale events. Numerator
// over 120 → actual scale.
impl Dispatch<WpFractionalScaleV1, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &WpFractionalScaleV1,
        event: <WpFractionalScaleV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<AppState>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let scale_f = scale as f64 / 120.0;
            let fs_id = proxy.id().protocol_id();
            if let Some(output_id) = state.fractional_to_output.get(&fs_id).copied() {
                state.set_output_scale(output_id, scale_f);
            }
        }
    }
}
