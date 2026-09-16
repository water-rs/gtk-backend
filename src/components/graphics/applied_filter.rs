//! `AppliedFilter` host for GTK (Linux-only).
//!
//! GTK never exposes a rendered widget's GPU texture handle through public
//! API, so the child is captured through the snapshot pipeline
//! (`snapshot_child` → `GskRenderNode` → `gsk_renderer_render_texture` →
//! `gdk_texture_download`), which is one GPU→CPU readback per content change.
//! The filtered output never touches CPU memory again: it is rendered into a
//! GL texture owned by a `GdkGLContext` created with
//! `gdk_surface_create_gl_context` — a context in GTK's render-context share
//! group — and handed to the compositor as a `GdkGLTexture`.
//!
//! Context currency follows the same discipline as `GpuSurface`: every wgpu
//! call happens while the owning `GdkGLContext` is current, including each
//! poll of the async device request and filter setup futures.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

use gdk4::prelude::*;
use glib::subclass::prelude::*;
use glib::thread_guard::ThreadGuard;
use glow::HasContext;
use gtk4::graphene;
use gtk4::gsk;
use gtk4::prelude::*;
use gtk4::{Orientation, Widget};
use waterui_graphics::gpu_surface::WgslModuleCache;
use waterui_graphics::{AppliedFilter, EffectContext, EffectFrameClock, EffectInput, EffectOutput};

use super::gl_util::{make_gl_loader, texture_format_desc};

#[cfg(not(target_os = "linux"))]
compile_error!(
    "GTK AppliedFilter implementation is Linux-only. The waterui-gtk crate should not be built on non-Linux targets."
);

/// The filter pipeline needs compute shaders, which `wgpu-hal`'s GLES backend
/// only exposes on GL 4.3+ / ES 3.1+ contexts.
const REQUIRED_GL_MAJOR: i32 = 4;
const REQUIRED_GL_MINOR: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelSize {
    width: u32,
    height: u32,
}

/// One cached GPU texture the filter pipeline reads, reallocated only when
/// the captured size changes.
#[derive(Debug)]
pub struct CachedTexture {
    size: PixelSize,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl CachedTexture {
    fn get_or_create<'a>(
        slot: &'a mut Option<Self>,
        device: &wgpu::Device,
        label: &'static str,
        size: PixelSize,
    ) -> &'a Self {
        if slot.as_ref().is_none_or(|cached| cached.size != size) {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: size.width,
                    height: size.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::STORAGE_BINDING
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            *slot = Some(Self {
                size,
                texture,
                view,
            });
        }
        slot.as_ref().unwrap()
    }
}

/// A GL texture + framebuffer pair allocated on the host's `GdkGLContext`.
///
/// The texture is handed to `GdkGLTextureBuilder`, which takes ownership of
/// the GL name, so a fresh target is allocated for every rendered frame. The
/// framebuffer exists only to give wgpu an `ExternalNativeFramebuffer` to
/// render into; it is deleted once the frame's commands are submitted.
struct GlRenderTarget {
    texture: glow::NativeTexture,
    framebuffer: glow::NativeFramebuffer,
}

#[allow(
    clippy::cast_possible_wrap,
    reason = "GL enum and size constants fit in i32"
)]
fn create_gl_render_target(gl: &glow::Context, size: PixelSize) -> Result<GlRenderTarget, String> {
    // SAFETY: the owning `GdkGLContext` is current on this thread (every
    // caller runs inside `snapshot` right after `make_current`).
    unsafe {
        let texture = gl.create_texture()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        // The default MIN filter wants mipmaps this texture will never have;
        // without the clamp every sampler reports the texture incomplete.
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_storage_2d(
            glow::TEXTURE_2D,
            1,
            glow::RGBA8,
            size.width as i32,
            size.height as i32,
        );
        let framebuffer = gl.create_framebuffer()?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(texture),
            0,
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.bind_texture(glow::TEXTURE_2D, None);
        Ok(GlRenderTarget {
            texture,
            framebuffer,
        })
    }
}

pin_project_lite::pin_project! {
    /// Makes the host's `GdkGLContext` current before every poll of the
    /// wrapped future, the same discipline `WithAreaContextCurrent` applies
    /// to `GpuSurface`: wgpu's external-GL adapter requires the owning context
    /// current whenever a wgpu entry point runs, and an async task resuming on
    /// the main loop finds no context current at all.
    struct WithGlContextCurrent<F> {
        context: gdk4::GLContext,
        #[pin]
        future: F,
    }
}

impl<F: Future> Future for WithGlContextCurrent<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.project();
        // `make_current` needs a live surface. Once unrealize tears the EGL
        // context down there is no context to run the wrapped task's GL work
        // on, and polling it anyway — or dropping it, which runs glDelete* —
        // is UB. Stay parked; the task's state observes the bumped generation
        // on the next snapshot instead.
        if DrawContextExt::surface(this.context).is_none() {
            return Poll::Pending;
        }
        this.context.make_current();
        this.future.poll(cx)
    }
}

/// The async bring-up state machine: device request, then filter setup.
/// `Idle` doubles as "ready for the next step" once the device exists.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SetupPhase {
    #[default]
    Idle,
    RequestingDevice,
    RequestingFilter,
    Ready,
}

mod imp {
    use gtk4::subclass::prelude::*;

    // The glib subclass macros expand against the parent scope, so this module
    // deliberately re-exports it wholesale rather than tracking each generated use.
    #[allow(
        clippy::wildcard_imports,
        reason = "glib subclass macros expand against the parent scope"
    )]
    use super::*;

    /// Everything the filter host owns. All fields are main-thread only —
    /// GTK widgets are confined to the GTK thread, and the `RefCell` never
    /// crosses an await (async tasks take the values they need and hand them
    /// back on completion).
    #[derive(Debug)]
    pub struct FilteredHost {
        /// The state the widget carries, `Rc`-shared so async device-setup and
        /// filter-setup tasks can publish their result back.
        pub state: Rc<RefCell<FilterState>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FilteredHost {
        const NAME: &'static str = "WateruiFilteredHost";
        type Type = super::FilteredHost;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for FilteredHost {
        fn dispose(&self) {
            let (child, _paintable) = {
                let mut state = self.state.borrow_mut();
                // Dropping the paintable disconnects its `invalidate-contents`
                // handler; the handler only holds a `WeakRef` to us.
                (state.child.take(), state.paintable.take())
            };
            if let Some(child) = child {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for FilteredHost {
        fn compute_expand(&self, hexpand: &mut bool, vexpand: &mut bool) {
            let child = self.state.borrow().child.clone();
            if let Some(child) = child {
                *hexpand = child.compute_expand(Orientation::Horizontal);
                *vexpand = child.compute_expand(Orientation::Vertical);
            }
        }

        fn measure(&self, orientation: Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            self.state
                .borrow()
                .child
                .clone()
                .map_or((-1, -1, -1, -1), |child| {
                    child.measure(orientation, for_size)
                })
        }

        #[allow(
            clippy::cast_sign_loss,
            reason = "allocated sizes and scale factor are non-negative"
        )]
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let child = self.state.borrow().child.clone();
            if let Some(child) = child {
                child.allocate(width, height, baseline, None);
            }
            let scale = self.obj().scale_factor().max(1) as u32;
            let size = PixelSize {
                width: (width.max(1) as u32).saturating_mul(scale),
                height: (height.max(1) as u32).saturating_mul(scale),
            };
            let mut state = self.state.borrow_mut();
            if state.capture_size != Some(size) {
                state.capture_size = Some(size);
                state.capture_dirty = true;
            }
        }

        fn unrealize(&self) {
            {
                let mut state = self.state.borrow_mut();
                state.generation += 1;
                state.capture_dirty = true;
                state.capture_size = None;
                // Dropping wgpu objects runs glDelete* against the owning
                // context, so they go while it is still current — the surface
                // outlives the widget's unrealize.
                if let Some(context) = state.gl_context.take() {
                    context.make_current();
                    state.presented = None;
                    state.input_texture = None;
                    state.wgpu_queue = None;
                    state.wgpu_device = None;
                    state.wgpu_adapter = None;
                    state.wgpu_instance = None;
                    state.glow = None;
                    // `gsk_renderer_new_for_surface` renderers must be
                    // unrealized before disposal or GSK aborts.
                    if let Some(renderer) = state.gsk_renderer.take() {
                        renderer.unrealize();
                    }
                }
                state.setup_phase = SetupPhase::Idle;
            }
            self.parent_unrealize();
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            self.snapshot_filtered(snapshot);
        }
    }

    #[derive(Debug)]
    pub struct FilterState {
        pub child: Option<gtk4::Widget>,
        pub paintable: Option<gtk4::WidgetPaintable>,
        pub filter: Option<AppliedFilter>,
        /// The presentation context, created from the widget's surface on the
        /// first snapshot. In GTK's render-context share group, so the GL
        /// textures we hand to `GdkGLTextureBuilder` composite directly.
        pub gl_context: Option<gdk4::GLContext>,
        /// The renderer that rasterizes the child's render node for capture.
        /// Bound to the surface; recreated with the context on unrealize.
        pub gsk_renderer: Option<gsk::Renderer>,
        pub glow: Option<Rc<glow::Context>>,
        pub wgpu_instance: Option<wgpu::Instance>,
        pub wgpu_adapter: Option<wgpu::Adapter>,
        pub wgpu_device: Option<wgpu::Device>,
        pub wgpu_queue: Option<wgpu::Queue>,
        pub shader_cache: Arc<WgslModuleCache>,
        /// Where the async bring-up stands. The filter holds wgpu objects
        /// across `setup`, so a new context means a new setup — the same
        /// re-setup `GpuSurface` runs.
        pub setup_phase: SetupPhase,
        /// Set when the child's contents or the pixel size changed, so the
        /// next snapshot re-captures and re-filters.
        pub capture_dirty: bool,
        /// The input size the next capture will produce, maintained by
        /// `size_allocate` so `snapshot` never has to guess the scale factor.
        pub capture_size: Option<PixelSize>,
        pub input_texture: Option<CachedTexture>,
        /// The filtered frame currently on screen, kept as a `GdkTexture`
        /// because that is what owns the underlying GL texture name.
        pub presented: Option<gdk4::Texture>,
        pub frame_clock: EffectFrameClock,
        /// Bumped on unrealize; in-flight async work created against a dead
        /// context observes it and discards its result.
        pub generation: u64,
    }

    impl Default for FilteredHost {
        fn default() -> Self {
            Self {
                state: Rc::new(RefCell::new(FilterState {
                    child: None,
                    paintable: None,
                    filter: None,
                    gl_context: None,
                    gsk_renderer: None,
                    glow: None,
                    wgpu_instance: None,
                    wgpu_adapter: None,
                    wgpu_device: None,
                    wgpu_queue: None,
                    shader_cache: Arc::new(WgslModuleCache::new()),
                    setup_phase: SetupPhase::Idle,
                    capture_dirty: true,
                    capture_size: None,
                    input_texture: None,
                    presented: None,
                    frame_clock: EffectFrameClock::new(),
                    generation: 0,
                })),
            }
        }
    }
}

glib::wrapper! {
    /// A container that presents its child's filtered rendering.
    ///
    /// The child is parented normally — measured, allocated, realized — but
    /// never reaches the screen unfiltered: `snapshot` emits only the filtered
    /// texture node, never `snapshot_child`.
    pub struct FilteredHost(ObjectSubclass<imp::FilteredHost>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl FilteredHost {
    fn new() -> Self {
        glib::Object::new()
    }
}

/// Builds the filtered-container widget hosting `content`.
pub fn render_applied_filter(mut filter: AppliedFilter, content: Widget) -> Widget {
    let host = FilteredHost::new();
    content.set_parent(&host);
    // The host is transparent to layout — its measure and allocation pass the
    // content through untouched — so every layout channel reads through to
    // the content it captures.
    crate::layout::proposal::transparent_to_content(host.upcast_ref(), &content);
    let redraw_handle = filter.redraw_handle();

    // `GtkWidgetPaintable` reports damage inside the child subtree
    // (`invalidate-contents`); that is the recapture trigger — an unrelated
    // sibling redrawing must not re-run the capture pipeline.
    let paintable = gtk4::WidgetPaintable::new(Some(&content));
    paintable.connect_invalidate_contents({
        let host = host.downgrade();
        move |_| {
            let Some(host) = host.upgrade() else { return };
            host.imp().state.borrow_mut().capture_dirty = true;
            host.queue_draw();
        }
    });

    {
        let mut state = host.imp().state.borrow_mut();
        state.child = Some(content);
        state.filter = Some(filter);
        state.paintable = Some(paintable);
    }

    // The filter's redraw handle (animated parameters, external wakers)
    // schedules a GTK frame; the `WeakRef` crosses threads inside a
    // `ThreadGuard` and is only dereferenced back on the main context.
    let host_guard = Arc::new(ThreadGuard::new(host.downgrade()));
    let main_context = glib::MainContext::default();
    let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let host_guard = Arc::clone(&host_guard);
        main_context.invoke(move || {
            if let Some(host) = host_guard.get_ref().upgrade() {
                host.queue_draw();
            }
        });
    });
    redraw_handle.set_waker(Some(waker));

    host.upcast()
}

impl imp::FilteredHost {
    /// The `snapshot` vfunc body: ensure the GL/wgpu stack exists, recapture
    /// the child when its contents changed, run the filter, and emit the
    /// filtered texture node.
    ///
    /// Every `self.state.borrow()` is bound to a `let` before the code that
    /// might borrow again: a borrow sitting in a `match` or `if` scrutinee
    /// lives until the whole expression ends, which would keep the `RefCell`
    /// borrowed across `init_gl_context`/`capture_and_filter` and panic.
    #[allow(
        clippy::cast_precision_loss,
        reason = "GSK works in f32 logical units; widget sizes are well inside its exact range"
    )]
    fn snapshot_filtered(&self, snapshot: &gtk4::Snapshot) {
        let obj = self.obj().clone();
        let child = self.state.borrow().child.clone();
        let Some(child) = child else { return };
        let Some(surface) = obj.native().and_then(|native| native.surface()) else {
            return;
        };
        let existing = self.state.borrow().gl_context.clone();
        let gl_context = existing.unwrap_or_else(|| self.init_gl_context(&surface));

        gl_context.make_current();
        self.init_wgpu(&gl_context);
        self.init_filter(&gl_context);

        let ready = matches!(self.state.borrow().setup_phase, SetupPhase::Ready);
        if !ready {
            // Device request or filter setup still in flight; they queue a
            // redraw when they land, the same way `GpuSurface` does.
            return;
        }

        let capture_dirty = self.state.borrow().capture_dirty;
        let needs_redraw = if capture_dirty {
            self.capture_and_filter(&obj, &child).unwrap_or(false)
        } else {
            false
        };

        let presented = self.state.borrow().presented.clone();
        if let Some(texture) = presented {
            let rect = graphene::Rect::new(0.0, 0.0, obj.width() as f32, obj.height() as f32);
            snapshot.append_texture(&texture, &rect);
        }
        if needs_redraw {
            obj.queue_draw();
        }
    }

    /// Creates the presentation `GdkGLContext` on `surface` and the capture
    /// renderer bound to it.
    fn init_gl_context(&self, surface: &gdk4::Surface) -> gdk4::GLContext {
        let context = surface.create_gl_context().unwrap_or_else(|error| {
            panic!("AppliedFilter: gdk_surface_create_gl_context failed: {error}")
        });
        context.set_required_version(REQUIRED_GL_MAJOR, REQUIRED_GL_MINOR);
        context.realize().unwrap_or_else(|error| {
            panic!(
                "AppliedFilter: a GL {REQUIRED_GL_MAJOR}.{REQUIRED_GL_MINOR} context is required for the filter pipeline and this display cannot provide one: {error}"
            )
        });
        let renderer = gsk::Renderer::for_surface(surface).unwrap_or_else(|| {
            panic!("AppliedFilter: gsk_renderer_new_for_surface returned no renderer")
        });
        let mut state = self.state.borrow_mut();
        state.gsk_renderer = Some(renderer);
        state.gl_context = Some(context.clone());
        context
    }

    /// Adopts the realized context into wgpu and kicks off the async device
    /// request. Mirrors `init_wgpu_if_needed` in `gpu_surface.rs` minus the
    /// `GLArea` framebuffer probing — this host owns no framebuffer.
    fn init_wgpu(&self, gl_context: &gdk4::GLContext) {
        {
            let state = self.state.borrow();
            if state.wgpu_device.is_some() || state.setup_phase != SetupPhase::Idle {
                return;
            }
        }

        let mut loader = make_gl_loader(gl_context);
        // SAFETY: `gl_context` is current (callers run right after
        // `make_current`), and the loader resolves symbols from the platform
        // GL runtime libraries GDK already loaded.
        let glow_context = Rc::new(unsafe { glow::Context::from_loader_function(|s| loader(s)) });
        // SAFETY: same current-context requirement as above; `new_external`
        // probes the context while it is current.
        let exposed = unsafe {
            wgpu::hal::gles::Adapter::new_external(|s| loader(s), wgpu::GlBackendOptions::default())
        }
        .unwrap_or_else(|| panic!("AppliedFilter: wgpu-hal failed to create external adapter"));

        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::GL;
        let instance = wgpu::Instance::new(descriptor);
        // SAFETY: `exposed` came from wgpu-hal's GL backend above and the same
        // GL context is still current.
        let adapter = unsafe { instance.create_adapter_from_hal::<wgpu::hal::api::Gles>(exposed) };

        // Request what the adapter actually supports: downlevel limits would
        // starve compute filters of workgroup and storage resources the
        // context really has.
        let device_descriptor = wgpu::DeviceDescriptor {
            label: Some("WaterUI GTK AppliedFilter (GLES) Device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            memory_hints: wgpu::MemoryHints::Performance,
            experimental_features: wgpu::ExperimentalFeatures::default(),
            trace: wgpu::Trace::default(),
        };

        {
            let mut state = self.state.borrow_mut();
            state.wgpu_instance = Some(instance);
            state.wgpu_adapter = Some(adapter.clone());
            state.glow = Some(glow_context);
            state.setup_phase = SetupPhase::RequestingDevice;
        }

        let generation = self.state.borrow().generation;
        let obj = self.obj().clone();
        let imp_state = Rc::clone(&self.state);
        glib::MainContext::default().spawn_local(WithGlContextCurrent {
            context: gl_context.clone(),
            future: async move {
                let result = adapter.request_device(&device_descriptor).await;
                {
                    let mut state = imp_state.borrow_mut();
                    if state.generation != generation {
                        // The context this device was created against was torn
                        // down while the request was in flight; the device is
                        // dead.
                        return;
                    }
                    match result {
                        Ok((device, queue)) => {
                            device.on_uncaptured_error(Arc::new(|error: wgpu::Error| {
                                tracing::error!("[wgpu] uncaptured error: {error}");
                            }));
                            state.wgpu_device = Some(device);
                            state.wgpu_queue = Some(queue);
                            state.setup_phase = SetupPhase::Idle;
                        }
                        Err(error) => {
                            panic!("AppliedFilter: failed to request wgpu device: {error}")
                        }
                    }
                }
                obj.queue_draw();
            },
        });
    }

    /// Runs `AppliedFilter::setup` once the device exists, with the context
    /// made current on every poll.
    fn init_filter(&self, gl_context: &gdk4::GLContext) {
        let (device, queue, mut filter, shader_cache) = {
            let mut state = self.state.borrow_mut();
            if state.setup_phase != SetupPhase::Idle {
                return;
            }
            let (Some(device), Some(queue), Some(filter)) = (
                state.wgpu_device.clone(),
                state.wgpu_queue.clone(),
                state.filter.take(),
            ) else {
                return;
            };
            state.setup_phase = SetupPhase::RequestingFilter;
            (device, queue, filter, Arc::clone(&state.shader_cache))
        };

        let generation = self.state.borrow().generation;
        let obj = self.obj().clone();
        let imp_state = Rc::clone(&self.state);
        glib::MainContext::default().spawn_local(WithGlContextCurrent {
            context: gl_context.clone(),
            future: async move {
                let context = EffectContext {
                    device: &device,
                    queue: &queue,
                    shader_cache: shader_cache.as_ref(),
                    input_format: wgpu::TextureFormat::Rgba8Unorm,
                    output_format: wgpu::TextureFormat::Rgba8Unorm,
                };
                filter
                    .setup(&context)
                    .await
                    .unwrap_or_else(|error| panic!("AppliedFilter: filter setup failed: {error}"));
                {
                    let mut state = imp_state.borrow_mut();
                    state.filter = Some(filter);
                    if state.generation == generation {
                        state.setup_phase = SetupPhase::Ready;
                        state.capture_dirty = true;
                    } else {
                        // The filter was set up against a dead context; leave
                        // the phase `unrealize` already reset so the next
                        // snapshot runs the whole bring-up again.
                        state.setup_phase = SetupPhase::Idle;
                    }
                }
                obj.queue_draw();
            },
        });
    }

    /// The per-snapshot GPU handles the capture phases share, cloned out of
    /// the state so no `RefCell` borrow crosses a call boundary.
    fn gpu_parts(&self) -> Option<GpuParts> {
        let state = self.state.borrow();
        match (
            state.gsk_renderer.clone(),
            state.glow.clone(),
            state.wgpu_device.clone(),
            state.wgpu_queue.clone(),
            state.capture_size,
            state.gl_context.clone(),
        ) {
            (Some(renderer), Some(gl), Some(device), Some(queue), Some(size), Some(gl_context)) => {
                Some(GpuParts {
                    renderer,
                    gl,
                    device,
                    queue,
                    size,
                    gl_context,
                })
            }
            _ => None,
        }
    }

    /// Captures the child's rendering, runs the filter into a fresh GL
    /// texture, and publishes it as the presented `GdkTexture`.
    ///
    /// Runs inside `snapshot` with the host's GL context current. On success
    /// the capture-dirty flag is consumed and the return value is the
    /// filter's "another frame needed" hint; on failure the flag stays set
    /// so the next snapshot retries.
    fn capture_and_filter(&self, obj: &FilteredHost, child: &gtk4::Widget) -> Option<bool> {
        let gpu = self.gpu_parts()?;
        let Some((input_texture, input_view)) = self.capture_child(obj, child, &gpu) else {
            // An empty child subtree is a legitimate empty frame — present
            // nothing, but do consume the dirty flag.
            let mut state = self.state.borrow_mut();
            state.presented = None;
            state.capture_dirty = false;
            return Some(false);
        };
        let needs_redraw = self.render_filtered(&gpu, &input_texture, input_view);
        self.state.borrow_mut().capture_dirty = false;
        Some(needs_redraw)
    }

    /// Renders the child's render node to pixels and uploads them into the
    /// cached input texture. `None` means the child produced no node at all.
    #[allow(
        clippy::cast_precision_loss,
        reason = "GSK works in f32 logical units; widget sizes are well inside its exact range"
    )]
    fn capture_child(
        &self,
        obj: &FilteredHost,
        child: &gtk4::Widget,
        gpu: &GpuParts,
    ) -> Option<(wgpu::Texture, wgpu::TextureView)> {
        // Snapshot the child into its own `Snapshot` and rasterize the node
        // at device resolution: the capture is the filter's input, so it has
        // to carry real pixels, not logical units.
        let child_snapshot = gtk4::Snapshot::new();
        obj.snapshot_child(child, &child_snapshot);
        let node = child_snapshot.to_node()?;
        let scale = obj.scale_factor().max(1) as f32;
        let node =
            gsk::TransformNode::new(&node, Some(&gsk::Transform::default().scale(scale, scale)));
        let captured = gpu.renderer.render_texture(
            &node,
            Some(&graphene::Rect::new(
                0.0,
                0.0,
                gpu.size.width as f32,
                gpu.size.height as f32,
            )),
        );

        // The readback: `gsk_renderer_render_texture` returns a `GdkTexture`
        // with no public GL handle, so the capture crosses to CPU memory
        // here. This is the one GPU→CPU transfer the whole pipeline pays,
        // and only when the child's contents actually changed.
        assert!(
            captured.format() == gdk4::MemoryFormat::R8g8b8a8Premultiplied,
            "AppliedFilter: gsk_renderer_render_texture produced {:?}, expected R8G8B8A8 premultiplied",
            captured.format(),
        );
        let stride = gpu.size.width.saturating_mul(4);
        let mut pixels = vec![0_u8; stride as usize * gpu.size.height as usize];
        captured.download(&mut pixels, stride as usize);

        // `snapshot_child` lets a nested filter host bind its own GL context,
        // and `render_texture`/`download` leave the surface's render context
        // current. Every wgpu call below must run on this host's context —
        // same share group, so a wrong context produces silently misplaced
        // writes and corrupted per-context state rather than a clean error.
        gpu.gl_context.make_current();

        let (input_texture, input_view) = {
            let mut state = self.state.borrow_mut();
            let input = CachedTexture::get_or_create(
                &mut state.input_texture,
                &gpu.device,
                "waterui_gtk_applied_filter_input",
                gpu.size,
            );
            (input.texture.clone(), input.view.clone())
        };
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &input_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(gpu.size.height),
            },
            wgpu::Extent3d {
                width: gpu.size.width,
                height: gpu.size.height,
                depth_or_array_layers: 1,
            },
        );
        Some((input_texture, input_view))
    }

    /// Runs the filter into a fresh share-group GL texture and publishes it
    /// as `state.presented`. Returns the filter's "another frame needed" hint.
    fn render_filtered(
        &self,
        gpu: &GpuParts,
        input_texture: &wgpu::Texture,
        input_view: wgpu::TextureView,
    ) -> bool {
        // The output lives in a GL texture GDK will own, so wgpu renders into
        // a framebuffer wrapped around it rather than a device-created
        // texture. Re-assert the owning context first: the capture phase ends
        // with GDK's render context current.
        gpu.gl_context.make_current();
        let (output_width, output_height) = {
            let state = self.state.borrow();
            state
                .filter
                .as_ref()
                .expect("AppliedFilter used before setup")
                .output_size(gpu.size.width, gpu.size.height)
        };
        let output_size = PixelSize {
            width: output_width.max(1),
            height: output_height.max(1),
        };
        let target = create_gl_render_target(&gpu.gl, output_size).unwrap_or_else(|error| {
            panic!("AppliedFilter: failed to allocate the output GL texture: {error}")
        });

        let hal_texture = wgpu::hal::gles::Texture {
            inner: wgpu::hal::gles::TextureInner::ExternalNativeFramebuffer {
                inner: target.framebuffer,
            },
            drop_guard: None,
            mip_level_count: 1,
            array_layer_count: 1,
            format: wgpu::TextureFormat::Rgba8Unorm,
            format_desc: texture_format_desc(wgpu::TextureFormat::Rgba8Unorm),
            copy_size: wgpu::hal::CopyExtent {
                width: output_size.width,
                height: output_size.height,
                depth: 1,
            },
        };
        // SAFETY: `hal_texture` wraps the framebuffer allocated directly
        // above, whose color attachment is `target.texture` — an RGBA8 GL
        // texture of `output_size`, matching the descriptor. The wgpu texture
        // is dropped before the GL names leave scope below.
        let output_texture = unsafe {
            gpu.device.create_texture_from_hal::<wgpu::hal::api::Gles>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("WaterUI GTK AppliedFilter Output"),
                    size: wgpu::Extent3d {
                        width: output_size.width,
                        height: output_size.height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                },
            )
        };
        let output_view = output_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("WaterUI GTK AppliedFilter"),
            });
        let needs_redraw = {
            let mut state = self.state.borrow_mut();
            let timing = state.frame_clock.tick();
            let filter = state
                .filter
                .as_mut()
                .expect("AppliedFilter used before setup");
            let input = EffectInput {
                device: &gpu.device,
                queue: &gpu.queue,
                texture: input_texture,
                view: input_view,
                format: wgpu::TextureFormat::Rgba8Unorm,
                width: gpu.size.width,
                height: gpu.size.height,
                timing,
            };
            let output = EffectOutput {
                device: &gpu.device,
                queue: &gpu.queue,
                texture: &output_texture,
                view: output_view,
                format: wgpu::TextureFormat::Rgba8Unorm,
                width: output_size.width,
                height: output_size.height,
            };
            filter
                .encode_render(&input, &output, &mut encoder)
                .unwrap_or_else(|error| panic!("AppliedFilter: filter render failed: {error}"))
        };
        gpu.queue.submit([encoder.finish()]);

        // The filtered texture crosses to GTK's render context, so the wgpu
        // command stream must be fenced: `GdkGLTextureBuilder`'s `sync` slot
        // is exactly that hand-off — GDK waits on the fence before sampling.
        // SAFETY: the host's GL context is current; the fence is created after
        // the submit above so it signals after all of wgpu's GL commands.
        let fence = unsafe { gpu.gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0) }
            .unwrap_or_else(|error| panic!("AppliedFilter: glFenceSync failed: {error}"));

        // The wgpu texture drops its framebuffer reference here; GL keeps the
        // object alive until the context stops referencing it, and the
        // texture name itself is handed to GDK below.
        drop(output_texture);
        // SAFETY: `target.framebuffer` was created above and is not needed
        // once the wgpu commands referencing it are submitted.
        unsafe { gpu.gl.delete_framebuffer(target.framebuffer) };

        let gdk_texture = build_gl_texture(&gpu.gl_context, target.texture, output_size, fence);
        self.state.borrow_mut().presented = Some(gdk_texture);
        needs_redraw
    }
}

/// The GPU handles cloned out of `FilterState` for one capture pass, so no
/// `RefCell` borrow crosses a call boundary.
struct GpuParts {
    renderer: gsk::Renderer,
    gl: Rc<glow::Context>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    size: PixelSize,
    gl_context: gdk4::GLContext,
}

/// Wraps the GL texture name in a `GdkTexture` for the compositor.
///
/// `gdk4`'s safe binding exposes only `GLTextureBuilder`'s getters, so the
/// construction goes through `gdk4::ffi` directly. The builder transfers
/// ownership of both `texture` and `sync` to the built texture: GDK deletes
/// the texture name and the sync object on the shared context when the
/// `GdkTexture` is released.
fn build_gl_texture(
    context: &gdk4::GLContext,
    texture: glow::NativeTexture,
    size: PixelSize,
    sync: glow::NativeFence,
) -> gdk4::Texture {
    use glib::translate::ToGlibPtr;

    // SAFETY: `gdk_gl_texture_builder_new` has no preconditions and returns a
    // builder this function owns until `build` consumes it. `context` is a
    // live `GdkGLContext`; `texture` is a live GL texture name owned by this
    // context's share group; `sync` is a live `GLsync` whose ownership
    // transfers to the built texture.
    unsafe {
        let builder = gdk4::ffi::gdk_gl_texture_builder_new();
        gdk4::ffi::gdk_gl_texture_builder_set_context(builder, context.to_glib_none().0);
        gdk4::ffi::gdk_gl_texture_builder_set_id(builder, texture.0.get());
        #[allow(
            clippy::cast_possible_wrap,
            reason = "sizes fit i32 by widget allocation limits"
        )]
        {
            gdk4::ffi::gdk_gl_texture_builder_set_width(builder, size.width as i32);
            gdk4::ffi::gdk_gl_texture_builder_set_height(builder, size.height as i32);
        }
        gdk4::ffi::gdk_gl_texture_builder_set_format(
            builder,
            gdk4::ffi::GDK_MEMORY_R8G8B8A8_PREMULTIPLIED,
        );
        gdk4::ffi::gdk_gl_texture_builder_set_sync(builder, sync.0.cast());
        glib::translate::from_glib_full(gdk4::ffi::gdk_gl_texture_builder_build(
            builder,
            None,
            std::ptr::null_mut(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gdk4::MemoryFormat;
    use glib::Bytes;

    use super::*;

    /// Pumps the main context until `presented` differs from `previous`
    /// (or, with `None`, simply exists) — one iteration at a time so GTK's
    /// frame clock and the async bring-up tasks all get to run.
    fn wait_for_frame(host: &FilteredHost, previous: Option<&gdk4::Texture>) {
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let done = {
                let state = host.imp().state.borrow();
                match (&state.presented, previous) {
                    (Some(texture), Some(previous)) => {
                        !std::ptr::eq(texture.as_ptr(), previous.as_ptr())
                    }
                    (Some(_), None) => true,
                    (None, _) => false,
                }
            };
            if done {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the filtered frame"
            );
            context.iteration(true);
        }
    }

    #[allow(clippy::cast_sign_loss, reason = "texture dimensions are non-negative")]
    fn download_rgba(texture: &gdk4::Texture) -> Vec<u8> {
        let stride = texture.width() as usize * 4;
        let mut pixels = vec![0_u8; stride * texture.height() as usize];
        texture.download(&mut pixels, stride);
        pixels
    }

    #[allow(
        clippy::cast_sign_loss,
        reason = "the fixture size is a positive constant"
    )]
    fn solid_texture(pixel: [u8; 4], size: i32) -> gdk4::MemoryTexture {
        let pixels: Vec<u8> = (0..size * size).flat_map(|_| pixel).collect();
        gdk4::MemoryTexture::new(
            size,
            size,
            MemoryFormat::R8g8b8a8Premultiplied,
            &Bytes::from_owned(pixels),
            size as usize * 4,
        )
    }

    /// End-to-end: a solid-white `Picture` filtered by `Invert` must present
    /// an opaque black frame, and swapping the child's contents must produce
    /// a fresh capture (opaque white).
    #[test]
    fn applied_filter_inverts_and_recaptures_child_pixels() {
        gtk4::init().expect("GTK tests need a display; run them under xvfb-run");

        let picture = gtk4::Picture::for_paintable(&solid_texture([255, 255, 255, 255], 64));
        picture.set_content_fit(gtk4::ContentFit::Fill);
        let host = render_applied_filter(
            AppliedFilter::new(filtrate::FilterAdapter::new(filtrate::filters::Invert)),
            picture.clone().upcast(),
        );
        let host = host
            .downcast::<FilteredHost>()
            .expect("render_applied_filter returns the host widget");

        let window = gtk4::Window::new();
        window.set_default_size(64, 64);
        window.set_child(Some(&host));
        window.present();

        wait_for_frame(&host, None);
        let presented = host
            .imp()
            .state
            .borrow()
            .presented
            .clone()
            .expect("a presented texture exists after the first frame");
        assert_eq!((presented.width(), presented.height()), (64, 64));
        let pixels = download_rgba(&presented);
        assert_eq!(
            &pixels[32 * 64 * 4 + 32 * 4..][..4],
            &[0, 0, 0, 255],
            "inverting opaque white must produce opaque black"
        );

        // Changing the child's pixels must trigger a recapture via
        // `WidgetPaintable::invalidate-contents`.
        picture.set_paintable(Some(&solid_texture([0, 0, 0, 255], 64)));
        wait_for_frame(&host, Some(&presented));
        let presented = host
            .imp()
            .state
            .borrow()
            .presented
            .clone()
            .expect("a presented texture exists after recapture");
        let pixels = download_rgba(&presented);
        assert_eq!(
            &pixels[32 * 64 * 4 + 32 * 4..][..4],
            &[255, 255, 255, 255],
            "inverting opaque black must produce opaque white"
        );

        window.close();
    }
}
