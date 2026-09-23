//! `AppliedFilter` host for GTK (Linux-only).
//!
//! GTK never exposes a rendered widget's GPU texture handle through public
//! API, so the child is captured through the snapshot pipeline
//! (`snapshot_child` → `GskRenderNode` → `gsk_renderer_render_texture` →
//! `gdk_texture_download`), which is one GPU→CPU readback per content change.
//! The capture rasterizer is `gsk::CairoRenderer`: it runs entirely on the
//! CPU and owns no `GdkGLContext`, which is deliberate — creating one GL
//! context per filter host is what serialized `eglCreateContext` calls
//! inside the window's frame snapshot hard enough to wedge the first present
//! (the stress example renders hundreds of filtered views).
//!
//! The filter itself runs on the process-shared `GpuRuntime` — one wgpu
//! device for the whole application — into a device-owned RGBA8 texture,
//! whose pixels are read back into a `GdkMemoryTexture` for the compositor.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use glib::subclass::prelude::*;
use glib::thread_guard::ThreadGuard;
use gtk4::graphene;
use gtk4::gsk;
use gtk4::prelude::*;
use gtk4::{Orientation, Widget};
use waterui_graphics::GpuRuntime;
use waterui_graphics::{AppliedFilter, EffectContext, EffectFrameClock, EffectInput, EffectOutput};

use super::shared_gpu;

#[cfg(not(target_os = "linux"))]
compile_error!(
    "GTK AppliedFilter implementation is Linux-only. The waterui-gtk crate should not be built on non-Linux targets."
);

/// `gsk::CairoRenderer` rasterizes capture nodes into ARGB32 pixels, which
/// `GdkMemoryTexture` reports as `A8R8G8B8` — BGRA byte order in memory.
const INPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
/// The filter output format; it feeds `gdk::MemoryTexture` directly, so it
/// must stay a plain 8-bit RGBA layout.
const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

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
        format: wgpu::TextureFormat,
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
                format,
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
                // Pending async work observes the bumped generation and
                // discards its result. The shared device and a completed
                // filter setup outlive the widget, so only per-surface
                // presentation state resets.
                state.generation += 1;
                state.capture_dirty = true;
                state.capture_size = None;
                state.presented = None;
                state.input_texture = None;
                state.filter_task_in_flight = false;
                state.refilter_pending = false;
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
        /// The renderer that rasterizes the child's render node for capture.
        /// A CPU rasterizer: it owns no GL context, so no context survives
        /// or must be torn down with the widget.
        pub gsk_renderer: Option<gsk::CairoRenderer>,
        /// The process-shared wgpu device the filter runs on; set once the
        /// runtime resolves.
        pub runtime: Option<GpuRuntime>,
        /// Where the asynchronous bring-up stands. The shared device outlives
        /// the widget, so a completed setup survives unrealize — only pending
        /// work is discarded.
        pub setup_phase: SetupPhase,
        /// Set when the child's contents or the pixel size changed, so the
        /// next snapshot re-captures and re-filters.
        pub capture_dirty: bool,
        /// The input size the next capture will produce, maintained by
        /// `size_allocate` so `snapshot` never has to guess the scale factor.
        pub capture_size: Option<PixelSize>,
        pub input_texture: Option<CachedTexture>,
        /// The filtered frame currently on screen, read back from the filter
        /// output and presented as a `GdkMemoryTexture`.
        pub presented: Option<gdk4::Texture>,
        /// A filter+readback task is in flight; the next capture waits for it
        /// so uploads stay ordered.
        pub filter_task_in_flight: bool,
        /// The filter asked for another frame (animated parameters). The
        /// child did not change, so the cached input texture is re-filtered
        /// instead of re-captured.
        pub refilter_pending: bool,
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
                    gsk_renderer: None,
                    runtime: None,
                    setup_phase: SetupPhase::Idle,
                    capture_dirty: true,
                    capture_size: None,
                    input_texture: None,
                    presented: None,
                    filter_task_in_flight: false,
                    refilter_pending: false,
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
    /// borrowed across `ensure_ready`/`capture_and_filter` and panic.
    #[allow(
        clippy::cast_precision_loss,
        reason = "GSK works in f32 logical units; widget sizes are well inside its exact range"
    )]
    fn snapshot_filtered(&self, snapshot: &gtk4::Snapshot) {
        let obj = self.obj().clone();
        let child = self.state.borrow().child.clone();
        let Some(child) = child else { return };

        self.ensure_ready();

        let ready = matches!(self.state.borrow().setup_phase, SetupPhase::Ready);
        if !ready {
            // Runtime bring-up or filter setup still in flight; they queue a
            // redraw when they land, the same way `GpuSurface` does.
            return;
        }

        enum Pass {
            Recapture,
            Refilter,
            None,
        }
        let pass = {
            let mut state = self.state.borrow_mut();
            if state.filter_task_in_flight {
                Pass::None
            } else if state.capture_dirty {
                state.capture_dirty = false;
                state.filter_task_in_flight = true;
                Pass::Recapture
            } else if state.refilter_pending {
                state.refilter_pending = false;
                state.filter_task_in_flight = true;
                Pass::Refilter
            } else {
                Pass::None
            }
        };
        match pass {
            Pass::Recapture => self.capture_and_filter(&obj, &child),
            Pass::Refilter => self.refilter(&obj),
            Pass::None => {}
        }

        let presented = self.state.borrow().presented.clone();
        if let Some(texture) = presented {
            let rect = graphene::Rect::new(0.0, 0.0, obj.width() as f32, obj.height() as f32);
            snapshot.append_texture(&texture, &rect);
        }
    }

    /// Advances the asynchronous bring-up one step per call: resolve the
    /// shared runtime, then run `AppliedFilter::setup`. Both are spawned at
    /// most once; a `Ready` phase ends the state machine.
    fn ensure_ready(&self) {
        enum Kick {
            Runtime,
            FilterSetup { runtime: GpuRuntime, filter: AppliedFilter },
            Nothing,
        }

        let kick = {
            let mut state = self.state.borrow_mut();
            match state.setup_phase {
                SetupPhase::Ready | SetupPhase::RequestingDevice | SetupPhase::RequestingFilter => {
                    Kick::Nothing
                }
                SetupPhase::Idle => {
                    if let Some(runtime) = state.runtime.clone() {
                        let Some(filter) = state.filter.take() else {
                            Kick::Nothing
                        } else {
                            state.setup_phase = SetupPhase::RequestingFilter;
                            Kick::FilterSetup { runtime, filter }
                        }
                    } else {
                        state.setup_phase = SetupPhase::RequestingDevice;
                        Kick::Runtime
                    }
                }
            }
        };

        match kick {
            Kick::Nothing => {}
            Kick::Runtime => {
                let obj = self.obj().clone();
                let state = Rc::clone(&self.state);
                shared_gpu::ensure_shared_runtime(move |result| {
                    {
                        let mut state = state.borrow_mut();
                        state.setup_phase = SetupPhase::Idle;
                        match result {
                            Ok(runtime) => state.runtime = Some(runtime),
                            Err(error) => {
                                panic!("AppliedFilter: shared GPU runtime unavailable: {error}")
                            }
                        }
                    }
                    obj.queue_draw();
                });
            }
            Kick::FilterSetup { runtime, mut filter } => {
                let obj = self.obj().clone();
                let imp_state = Rc::clone(&self.state);
                let generation = self.state.borrow().generation;
                let context = runtime.context();
                let device = context.device.clone();
                let queue = context.queue.clone();
                let shader_cache = context.shader_cache.clone();
                gtk4::glib::MainContext::default().spawn_local(async move {
                    let context = EffectContext {
                        device: &device,
                        queue: &queue,
                        shader_cache: shader_cache.as_ref(),
                        input_format: INPUT_FORMAT,
                        output_format: OUTPUT_FORMAT,
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
                            // Torn down mid-setup; the next realize re-runs
                            // the whole bring-up.
                            state.setup_phase = SetupPhase::Idle;
                        }
                    }
                    obj.queue_draw();
                });
            }
        }
    }

    /// The per-snapshot handles the capture phases share, cloned out of
    /// the state so no `RefCell` borrow crosses a call boundary.
    fn runtime_parts(&self) -> Option<RuntimeParts> {
        let state = self.state.borrow();
        match (
            state.gsk_renderer.clone(),
            state.runtime.clone(),
            state.capture_size,
        ) {
            (Some(renderer), Some(runtime), Some(size)) => Some(RuntimeParts {
                renderer,
                runtime,
                size,
            }),
            _ => None,
        }
    }

    /// Captures the child's rendering, then runs the filter and the
    /// readback on a spawned task. The capture itself is synchronous — it is
    /// the snapshot pipeline, which only exists on the main thread — while
    /// the filter encode and the pixel readback happen off the snapshot.
    fn capture_and_filter(&self, obj: &FilteredHost, child: &gtk4::Widget) {
        let Some(parts) = self.runtime_parts() else {
            let mut state = self.state.borrow_mut();
            state.filter_task_in_flight = false;
            return;
        };
        let Some(capture) = self.capture_child(obj, child, &parts) else {
            // An empty child subtree is a legitimate empty frame — present
            // nothing, the dirty flag was already consumed.
            let mut state = self.state.borrow_mut();
            state.presented = None;
            state.filter_task_in_flight = false;
            return;
        };

        let obj = obj.clone();
        let imp_state = Rc::clone(&self.state);
        let generation = self.state.borrow().generation;
        glib::MainContext::default().spawn_local(async move {
            let presented = filter_and_readback(&imp_state, &parts, capture);
            {
                let mut state = imp_state.borrow_mut();
                state.filter_task_in_flight = false;
                if state.generation != generation {
                    return;
                }
                if let Some((texture, needs_redraw)) = presented {
                    state.presented = Some(texture);
                    if needs_redraw {
                        // Animated filters ask for continuous frames; the
                        // child is unchanged, so only the filter re-runs.
                        state.refilter_pending = true;
                    }
                }
            }
            obj.queue_draw();
        });
    }

    /// Re-runs the filter against the cached input texture — the animated-
    /// filter path, where the child pixels did not change.
    fn refilter(&self, obj: &FilteredHost) {
        let Some(parts) = self.runtime_parts() else {
            self.state.borrow_mut().filter_task_in_flight = false;
            return;
        };
        let obj = obj.clone();
        let imp_state = Rc::clone(&self.state);
        let generation = self.state.borrow().generation;
        glib::MainContext::default().spawn_local(async move {
            let presented = filter_and_readback(&imp_state, &parts, None);
            {
                let mut state = imp_state.borrow_mut();
                state.filter_task_in_flight = false;
                if state.generation != generation {
                    return;
                }
                if let Some((texture, needs_redraw)) = presented {
                    state.presented = Some(texture);
                    if needs_redraw {
                        state.refilter_pending = true;
                    }
                }
            }
            obj.queue_draw();
        });
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
        parts: &RuntimeParts,
    ) -> Option<CapturedFrame> {
        // Snapshot the child into its own `Snapshot` and rasterize the node
        // at device resolution: the capture is the filter's input, so it has
        // to carry real pixels, not logical units. The rasterizer is Cairo —
        // CPU-side, no GL context, no compositor involvement.
        let child_snapshot = gtk4::Snapshot::new();
        obj.snapshot_child(child, &child_snapshot);
        let node = child_snapshot.to_node()?;
        let scale = obj.scale_factor().max(1) as f32;
        let node =
            gsk::TransformNode::new(&node, Some(&gsk::Transform::default().scale(scale, scale)));
        let captured = parts.renderer.upcast_ref::<gsk::Renderer>().render_texture(
            &node,
            Some(&graphene::Rect::new(
                0.0,
                0.0,
                parts.size.width as f32,
                parts.size.height as f32,
            )),
        );

        // The readback: `gsk_renderer_render_texture` returns a `GdkTexture`
        // with no public pixel handle, so the capture crosses to CPU memory
        // here — the one GPU→CPU transfer the pipeline pays, and only when
        // the child's contents actually changed. Cairo rasterizes to ARGB32,
        // which `GdkMemoryTexture` reports as `A8R8G8B8` (BGRA in memory);
        // the input texture and the filter's `input_format` are Bgra8Unorm
        // to match.
        assert!(
            captured.format() == gdk4::MemoryFormat::A8r8g8b8Premultiplied,
            "AppliedFilter: cairo capture produced {:?}, expected A8R8G8B8 premultiplied",
            captured.format(),
        );
        let stride = parts.size.width.saturating_mul(4);
        let mut pixels = vec![0_u8; stride as usize * parts.size.height as usize];
        captured.download(&mut pixels, stride as usize);
        Some(CapturedFrame {
            pixels,
            size: parts.size,
        })
    }

/// The per-pass handles cloned out of `FilterState`, so no `RefCell`
/// borrow crosses a call boundary.
struct RuntimeParts {
    renderer: gsk::CairoRenderer,
    runtime: GpuRuntime,
    size: PixelSize,
}

/// A freshly rasterized child frame, pixel format `INPUT_FORMAT`.
struct CapturedFrame {
    pixels: Vec<u8>,
    size: PixelSize,
}

/// Uploads a fresh capture (if any) into the cached input texture, runs the
/// filter into an output texture, and reads the pixels back into a
/// `GdkMemoryTexture`. Returns the texture and the filter's "another frame
/// needed" hint; `None` means there was no input to filter. Runs inside a
/// spawned task: the readback blocks on a device poll, and snapshots must
/// not pay that.
fn filter_and_readback(
    state: &Rc<RefCell<imp::FilterState>>,
    parts: &RuntimeParts,
    capture: Option<CapturedFrame>,
) -> Option<(gdk4::Texture, bool)> {
    let shared = parts.runtime.context();
    let device = &shared.device;
    let queue = &shared.queue;

    if let Some(capture) = capture {
        let stride = capture.size.width.saturating_mul(4);
        let input_texture = {
            let mut st = state.borrow_mut();
            CachedTexture::get_or_create(
                &mut st.input_texture,
                device,
                "waterui_gtk_applied_filter_input",
                capture.size,
                INPUT_FORMAT,
            )
            .texture
            .clone()
        };
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &input_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &capture.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(capture.size.height),
            },
            wgpu::Extent3d {
                width: capture.size.width,
                height: capture.size.height,
                depth_or_array_layers: 1,
            },
        );
    }

    // The input texture and its size come from state so the refilter path —
    // no fresh capture — works on the cached upload.
    let (input_texture, input_view, input_size) = {
        let state = state.borrow();
        let input = state.input_texture.as_ref()?;
        (input.texture.clone(), input.view.clone(), input.size)
    };

    let ((raw_out_w, raw_out_h), timing) = {
        let mut st = state.borrow_mut();
        let filter = st.filter.as_ref().expect("AppliedFilter used before setup");
        (
            filter.output_size(input_size.width, input_size.height),
            st.frame_clock.tick(),
        )
    };
    let output_size = PixelSize {
        width: raw_out_w.max(1),
        height: raw_out_h.max(1),
    };
    let output_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("WaterUI GTK AppliedFilter Output"),
        size: wgpu::Extent3d {
            width: output_size.width,
            height: output_size.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: OUTPUT_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let output_view = output_texture.create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("WaterUI GTK AppliedFilter"),
    });
    let needs_redraw = {
        let mut filter = state.borrow_mut().filter.take()?;
        let input = EffectInput {
            device,
            queue,
            texture: &input_texture,
            view: input_view,
            format: INPUT_FORMAT,
            width: input_size.width,
            height: input_size.height,
            timing,
        };
        let output = EffectOutput {
            device,
            queue,
            texture: &output_texture,
            view: output_view,
            format: OUTPUT_FORMAT,
            width: output_size.width,
            height: output_size.height,
        };
        let result = filter
            .encode_render(&input, &output, &mut encoder)
            .unwrap_or_else(|error| panic!("AppliedFilter: filter render failed: {error}"));
        state.borrow_mut().filter = Some(filter);
        result
    };
    queue.submit([encoder.finish()]);

    let pixels = shared_gpu::readback_texture_rgba8(
        &parts.runtime,
        &output_texture,
        output_size.width,
        output_size.height,
    )
    .unwrap_or_else(|error| panic!("AppliedFilter: output readback failed: {error}"));

    let stride = usize::try_from(output_size.width).expect("texture width fits usize") * 4;
    let texture = gdk4::MemoryTexture::new(
        i32::try_from(output_size.width).expect("texture width fits i32"),
        i32::try_from(output_size.height).expect("texture height fits i32"),
        gdk4::MemoryFormat::R8g8b8a8Premultiplied,
        &gtk4::glib::Bytes::from_owned(pixels),
        stride,
    );
    Some((texture.upcast(), needs_redraw))
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
