//! GPU leaf views for GTK (Linux-only): `GpuContentView`, `SceneView` and
//! `ExternalFrameView` on Cherenkov's render-target contract.
//!
//! Cherenkov's [`Engine`] owns a dedicated render thread that holds every
//! wgpu handle. Each hosted view is realized as a [`DmabufTarget`] render
//! target on an `Engine<Gpu>` built over a [`SharedDevice`]: the target
//! presents through a bounded pool of exportable dma-buf images, and every
//! [`DmabufFrame`] the engine hands over is wrapped in a
//! `GdkDmabufTexture` — the image's format/modifier pair is negotiated
//! against the display's `GdkDmabufFormats`, its acquire sync file is
//! attached to the image's reservation so GTK's implicit-sync access waits
//! on the GPU, and the texture's release callback returns a release sync
//! file for the engine to reuse the image. Nothing reads GPU output back
//! to the CPU and no thread ever blocks on a fence.
//!
//! One engine per widget keeps device-loss rebuild local to the widget
//! whose device died; the leaf contract ([`HostedLeaf`]) is shared by the
//! three view kinds, so `GpuContentView`'s producer binding, `SceneView`'s
//! recorded content and `ExternalFrameView`'s submitted frames all ride the
//! same surface lifecycle.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use gdk4::prelude::*;
use glib::subclass::prelude::*;
use glib::thread_guard::ThreadGuard;
use gtk4::prelude::*;
use gtk4::{Widget, graphene};
use waterui_core::layout::{ProposalSize, StretchAxis, ViewDimensions};
use waterui_core::{Environment, Native, NativeView};
use waterui_graphics::cherenkov::{
    Display, Draw, Engine, Fixed, FrameSink, FrameTime, GpuProducer, Next, RenderError, Surface,
    Visibility, kurbo,
};
use waterui_graphics::cherenkov_gpu::interop::dmabuf::{DmabufFormat, DmabufFrame, DmabufTarget};
use waterui_graphics::cherenkov_gpu::interop::{GpuContentBox, RedrawCallback, SharedDevice};
use waterui_graphics::cherenkov_gpu::{Gpu, GpuConfig};
use waterui_graphics::gpu::external::FrameReceiver;
use waterui_graphics::gpu::{DeviceLoss, ExternalFrameView, GpuContentView, RedrawHandle};
use waterui_graphics::input::SurfaceInputEvent;
use waterui_graphics::{
    HeldResources, SceneResources, SceneView, resolve_scene_proposal, scene_stretch_axis,
};

use crate::browser_input::{SurfaceInputSink, install as install_surface_input};
use crate::component::GtkComponent;
use crate::components::graphics::dmabuf as dmabuf_present;
use crate::layout::proposal::{install_axis_provider, install_measure_provider};
use crate::renderer::GtkRenderer;

#[cfg(not(target_os = "linux"))]
compile_error!(
    "GTK GPU surface implementation is Linux-only. The waterui-gtk crate should not be built on non-Linux targets."
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelSize {
    width: u32,
    height: u32,
}

impl PixelSize {
    /// The widget's allocation in device pixels, or `None` while it has no
    /// area to draw into (pre-allocation or collapsed).
    #[allow(
        clippy::cast_sign_loss,
        reason = "allocated sizes and the scale factor are non-negative"
    )]
    fn from_widget(widget: &Widget) -> Option<Self> {
        let scale = widget.scale_factor().max(1) as u32;
        let width = widget.width();
        let height = widget.height();
        (width > 0 && height > 0).then(|| Self {
            width: (width as u32).saturating_mul(scale),
            height: (height as u32).saturating_mul(scale),
        })
    }

    const fn tuple(self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// A `SceneView` plus its invalidation flag, which content-owned frame
/// sources set through the installed `SceneInvalidator`.
#[derive(Debug)]
struct SceneLeaf {
    view: SceneView,
    /// Starts dirty: the first `prepare` records regardless of geometry
    /// history.
    dirty: Rc<Cell<bool>>,
}

/// The semantic half of a mounted GPU surface: what is drawn, measured and
/// fed input. Each variant also owns its engine binding — a producer
/// install, a recording, or a frame subscription — which belongs to one
/// engine generation and is dropped with it.
enum HostedLeaf {
    Content {
        view: GpuContentView,
        bound: Option<ContentBinding>,
    },
    Scene {
        leaf: SceneLeaf,
        bound: Option<SceneBinding>,
    },
    External {
        view: ExternalFrameView,
        bound: Option<ExternalBinding>,
    },
}

impl std::fmt::Debug for HostedLeaf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Content { .. } => "GpuContentView",
            Self::Scene { .. } => "SceneView",
            Self::External { .. } => "ExternalFrameView",
        };
        f.write_str(name)
    }
}

/// A `GpuContentView` bound to the surface's root layer at one pixel size.
struct ContentBinding {
    producer: GpuProducer<Gpu>,
    size: PixelSize,
}

/// The engine-generation resources behind a `SceneView`'s installed
/// recording: the registration table it names, and the handles keeping the
/// previous recording's registrations alive until replaced.
struct SceneBinding {
    resources: SceneResources,
    held: HeldResources,
    /// The `(width, height, scale)` the installed recording was made for —
    /// logical points and the display scale its root transform carries. A
    /// resize at the same logical size but a different scale rewrites the
    /// recording's transform, so scale is part of the key.
    recorded_geometry: Option<(f32, f32, f64)>,
}

/// An `ExternalFrameView`'s running stream and its binding on the surface.
struct ExternalBinding {
    producer: GpuProducer<Gpu>,
    sink: FrameSink<Gpu>,
    receiver: FrameReceiver,
    size: PixelSize,
}

impl HostedLeaf {
    fn measure(&self, proposal: ProposalSize) -> ViewDimensions {
        match self {
            Self::Content { view, .. } => view.measure(proposal),
            Self::External { view, .. } => view.measure(proposal),
            Self::Scene { leaf, .. } => {
                let resolved = resolve_scene_proposal(leaf.view.intrinsic_size(), proposal);
                ViewDimensions::new(waterui_core::layout::Size::new(
                    resolved.width.unwrap_or(0.0),
                    resolved.height.unwrap_or(0.0),
                ))
            }
        }
    }

    fn stretch_axis(&self) -> StretchAxis {
        match self {
            Self::Content { view, .. } => NativeView::stretch_axis(view),
            Self::External { view, .. } => NativeView::stretch_axis(view),
            Self::Scene { leaf, .. } => scene_stretch_axis(leaf.view.intrinsic_size()),
        }
    }

    fn accessibility_label(&self) -> Option<String> {
        match self {
            Self::Content { view, .. } => view.accessibility_label().map(str::to_owned),
            Self::External { view, .. } => view.accessibility_label().map(str::to_owned),
            Self::Scene { leaf, .. } => leaf.view.accessibility_label(),
        }
    }

    fn accessibility_value(&self) -> Option<String> {
        match self {
            Self::Content { view, .. } => view.accessibility_value().map(str::to_owned),
            Self::External { view, .. } => view.accessibility_value().map(str::to_owned),
            Self::Scene { leaf, .. } => leaf.view.accessibility_value(),
        }
    }

    fn resolved_hdr_preference(&self) -> Option<bool> {
        match self {
            Self::Content { view, .. } => view.resolved_hdr_preference(),
            Self::External { view, .. } => view.resolved_hdr_preference(),
            Self::Scene { .. } => None,
        }
    }

    fn wants_input_events(&mut self) -> bool {
        match self {
            Self::Content { view, .. } => view.wants_input_events(),
            Self::Scene { leaf, .. } => leaf.view.content_mut().wants_input_events(),
            // `ExternalFrameView` carries no input contract.
            Self::External { .. } => false,
        }
    }

    fn input(&mut self, event: &SurfaceInputEvent) {
        match self {
            Self::Content { view, .. } => view.input(event),
            Self::Scene { leaf, .. } => leaf.view.content_mut().input(event),
            Self::External { .. } => {}
        }
    }

    /// Installs the scene's invalidation callback — the wake that marks the
    /// recording dirty and asks the host for a frame. The other leaves take
    /// their wake through the producer/stream handles themselves.
    fn mount(&mut self, wake: &Arc<dyn Fn() + Send + Sync>) {
        if let Self::Scene { leaf, .. } = self {
            let dirty = Rc::clone(&leaf.dirty);
            let wake = Arc::clone(wake);
            leaf.view
                .content_mut()
                .set_invalidator(Some(Rc::new(move || {
                    dirty.set(true);
                    wake();
                })));
        }
    }

    /// The per-frame UI hook a `GpuContentView` declares; others no-op.
    fn before_frame(&mut self) {
        if let Self::Content { view, .. } = self {
            view.frame();
        }
    }

    /// Clears the engine binding; called before the surface and engine are
    /// dropped so producers retire while the render thread still exists.
    fn unbind(&mut self) {
        match self {
            Self::Content { bound, .. } => *bound = None,
            Self::Scene { bound, .. } => *bound = None,
            Self::External { bound, .. } => *bound = None,
        }
    }

    /// Ensures the leaf's binding exists on `surface` and feeds it for the
    /// frame: producer installs at the current pixel size, a dirty scene
    /// records and installs, an external stream submits its newest frame.
    ///
    /// Called inside `snapshot`, on the UI thread, before `engine.render`.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "physical pixels and the backing scale become f32 layout points"
    )]
    fn prepare(
        &mut self,
        engine: &Rc<Engine<Gpu>>,
        device: &SharedDevice,
        surface: &Surface<Gpu>,
        size: PixelSize,
        scale: f64,
        wake: &Arc<dyn Fn() + Send + Sync>,
    ) {
        match self {
            Self::Content { view, bound } => {
                if bound.is_none() || bound.as_ref().is_some_and(|binding| binding.size != size) {
                    // `engine_content` answers the same content object every
                    // call, so a binding rebuilt after device loss re-installs
                    // it with its state intact.
                    let producer = bound.take().map_or_else(
                        || {
                            let wake = wake.clone();
                            engine.gpu_producer(GpuContentBox::new(
                                view.engine_content(),
                                move || wake(),
                            ))
                        },
                        |binding| binding.producer,
                    );
                    surface.update(|tx| {
                        tx[surface.root()].content(producer.at(size.tuple()));
                    });
                    *bound = Some(ContentBinding { producer, size });
                }
            }
            Self::Scene { leaf, bound } => {
                let binding = bound.get_or_insert_with(|| {
                    // A fresh engine generation: content carrying
                    // registrations or recordings from the old engine must
                    // rebuild before the first record against this one.
                    leaf.view.content_mut().rebuild_for_engine();
                    SceneBinding {
                        resources: SceneResources::new(Rc::clone(engine)),
                        held: HeldResources::empty(),
                        recorded_geometry: None,
                    }
                });
                let points = (
                    (f64::from(size.width) / scale) as f32,
                    (f64::from(size.height) / scale) as f32,
                );
                let geometry = (points.0, points.1, scale);
                if !leaf.dirty.replace(false) && binding.recorded_geometry == Some(geometry) {
                    return;
                }
                let mut resources = binding.resources.recording();
                let mut again = false;
                let recording = surface.record(|recorder| {
                    // `build_scene` produces logical-point geometry while the
                    // surface renders physical pixels, so the recording wraps
                    // it in the display-scale transform — the conversion
                    // `Display::scale` deliberately never applies itself.
                    recorder.transform(kurbo::Affine::scale(scale), |recorder| {
                        again = leaf.view.content_mut().build_scene(
                            recorder,
                            &mut resources,
                            points.0,
                            points.1,
                        );
                    });
                });
                let held = resources.finish();
                surface.update(|tx| {
                    tx[surface.root()].layout_size(Fixed(kurbo::Size::new(
                        f64::from(points.0),
                        f64::from(points.1),
                    )));
                    tx[surface.root()].content(recording);
                });
                // The previous recording's registrations stay alive until
                // its replacement is installed.
                binding.held = held;
                binding.recorded_geometry = Some(geometry);
                if again {
                    leaf.dirty.set(true);
                    wake();
                }
            }
            Self::External { view, bound } => {
                let binding = bound.get_or_insert_with(|| {
                    let stream = view.stream();
                    let receiver = stream.start(
                        &device.device,
                        &device.queue,
                        RedrawHandle::new({
                            let wake = wake.clone();
                            move || wake()
                        }),
                    );
                    let (producer, sink) = engine.frame_producer();
                    ExternalBinding {
                        producer,
                        sink,
                        receiver,
                        size,
                    }
                });
                if binding.size != size {
                    surface.update(|tx| {
                        tx[surface.root()].content(binding.producer.at(size.tuple()));
                    });
                    binding.size = size;
                }
                if let Some(frame) = binding.receiver.take() {
                    binding.sink.submit(frame);
                }
            }
        }
    }
}

/// The device + engine pair one widget owns, rebuilt together after a
/// reported device loss.
struct EngineStack {
    /// The shared device `ExternalFrameView` streams are started on; the
    /// engine renders on it too, so a stream's textures are already on the
    /// right device.
    device: SharedDevice,
    engine: Rc<Engine<Gpu>>,
    /// Reports this device's driver-announced death; on loss the whole
    /// stack — binding, surface, engine — is rebuilt on a fresh device.
    loss: DeviceLoss,
}

/// Builds the engine's device and the engine over it, then installs the
/// host wake and starts observing the device for loss.
fn build_engine(wake: &Arc<dyn Fn() + Send + Sync>) -> EngineStack {
    let mut config = GpuConfig {
        redraw: Some(RedrawCallback::new({
            let wake = wake.clone();
            move || wake()
        })),
        ..GpuConfig::default()
    };
    let device = SharedDevice::create(&config)
        .unwrap_or_else(|error| panic!("GpuSurfaceHost: SharedDevice::create failed: {error}"));
    config.device = Some(device.clone());
    let engine = Engine::<Gpu>::new(config)
        .unwrap_or_else(|error| panic!("GpuSurfaceHost: Engine::new failed: {error}"));
    engine.set_waker({
        let wake = wake.clone();
        move || wake()
    });
    let loss = DeviceLoss::observe(&device.device);
    EngineStack {
        device,
        engine: Rc::new(engine),
        loss,
    }
}

/// Everything the widget carries.
///
/// Main-thread only: `Engine`, `Surface`, `Layer` and the leaf views are all
/// `!Send` by design, and the `RefCell` never crosses a call boundary —
/// `snapshot`'s borrows are taken and released per step.
pub struct GpuHostState {
    leaf: HostedLeaf,
    stack: Option<EngineStack>,
    surface: Option<Surface<Gpu>>,
    /// The channel the engine's presented frames arrive on; paired with
    /// `surface` — a dma-buf target's frame receiver lives as long as the
    /// surface does.
    frames: Option<Receiver<DmabufFrame>>,
    /// The display's declared `(fourcc, modifiers)` set, resolved once on
    /// the first surface creation. Resolution panics on an empty
    /// intersection — a display that cannot import dma-buf cannot host
    /// this widget.
    formats: Option<Vec<DmabufFormat>>,
    /// The last dma-buf texture presented; kept so a snapshot between
    /// engine frames still has the widget's content to emit.
    presented: Option<gdk4::Texture>,
    /// Wakes shared by every engine- and producer-side redraw callback;
    /// routes through `glib::idle_add_once` so producer threads only ever
    /// touch a `WeakRef`, and on-thread wakes always defer past the
    /// snapshot's mutable borrow.
    wake: Arc<dyn Fn() + Send + Sync>,
    /// Count of frames this surface has rendered and presented; the e2e
    /// readiness gate sequences per-surface completion events with it.
    frames_completed: u64,
    /// The `measure(UNSPECIFIED)` answer last pushed into GTK sizing; see
    /// `sync_surface_sizing`.
    sizing_snapshot: Cell<Option<(f32, f32)>>,
}

impl std::fmt::Debug for GpuHostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuHostState")
            .field("leaf", &self.leaf)
            .field("has_engine", &self.stack.is_some())
            .field("has_surface", &self.surface.is_some())
            .finish_non_exhaustive()
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

    /// A plain `Widget` — not a `GLArea`: the engine owns all GPU state on
    /// its render thread and presents through `snapshot`, so this widget
    /// never holds a GL context at all.
    #[derive(Debug, Default)]
    pub struct GpuSurfaceHost {
        /// Set by `render_gpu_surface` right after construction; the leaf
        /// cannot arrive through `ObjectSubclass::new`.
        pub state: OnceCell<Rc<RefCell<GpuHostState>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GpuSurfaceHost {
        const NAME: &'static str = "WateruiGpuSurfaceHost";
        type Type = super::GpuSurfaceHost;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for GpuSurfaceHost {}

    impl WidgetImpl for GpuSurfaceHost {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            self.snapshot_gpu(snapshot);
        }

        fn map(&self) {
            self.parent_map();
            let Some(state) = self.state.get() else {
                return;
            };
            if let Some(surface) = state.borrow().surface.as_ref() {
                surface
                    .visibility(Visibility::Visible)
                    .expect("GpuSurfaceHost: visibility update on a dead render thread");
            }
        }

        fn unmap(&self) {
            let Some(state) = self.state.get() else {
                return;
            };
            if let Some(surface) = state.borrow().surface.as_ref() {
                surface
                    .visibility(Visibility::Hidden)
                    .expect("GpuSurfaceHost: visibility update on a dead render thread");
            }
            self.parent_unmap();
        }

        fn unrealize(&self) {
            if let Some(state) = self.state.get() {
                let mut state = state.borrow_mut();
                // Producers and stream receivers retire against the engine
                // that made them — drop the binding first, then the frame
                // channel, the retained texture (whose release fires the
                // frame's release fd), the surface, then the engine (whose
                // drop joins the render thread).
                state.leaf.unbind();
                state.frames.take();
                state.presented = None;
                state.surface.take();
                state.stack.take();
            }
            self.parent_unrealize();
        }
    }
}

glib::wrapper! {
    /// Presents a GPU leaf's rendered frame.
    pub struct GpuSurfaceHost(ObjectSubclass<imp::GpuSurfaceHost>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl GpuSurfaceHost {
    fn state(&self) -> &Rc<RefCell<GpuHostState>> {
        self.imp()
            .state
            .get()
            .expect("GpuSurfaceHost state is installed at construction")
    }
}

/// The `wake` closure shared by `Engine::set_waker`, `GpuConfig::redraw`,
/// `GpuContentBox`, `ExternalFrameStream` and the scene invalidator. GPU
/// objects are `!Send` and live on the render thread, so it only queues a
/// draw: it crosses threads inside a `ThreadGuard` and dereferences its
/// `WeakRef` back on the main context. `idle_add_once` always defers —
/// `MainContext::invoke` runs inline when the calling thread owns the
/// context, and producers do wake from the UI thread inside `borrow_mut`
/// scopes.
fn make_wake(host: &GpuSurfaceHost) -> Arc<dyn Fn() + Send + Sync> {
    let host_guard = Arc::new(ThreadGuard::new(host.downgrade()));
    Arc::new(move || {
        let host_guard = Arc::clone(&host_guard);
        glib::idle_add_once(move || {
            if let Some(host) = host_guard.get_ref().upgrade() {
                // A wake can also be the first sign of an intrinsic size (a
                // fetched image, a decoded first frame): GTK sizing must
                // follow the leaf's live measure.
                sync_surface_sizing(&host);
                host.queue_draw();
            }
        });
    })
}

/// Re-runs the widget's GTK sizing when the leaf's own
/// `measure(UNSPECIFIED)` answer moved. A leaf's intrinsic size can arrive
/// after the widget exists — a fetched image publishing its first frame, a
/// video learning its aspect — and the creation-time `size_request` plus
/// the natural answer the measure provider reports must follow it: a host
/// left at its empty-measure zero stays zero-allocated and can never
/// present a frame.
fn sync_surface_sizing(host: &GpuSurfaceHost) {
    let state = host.state().borrow();
    let measured = state.leaf.measure(ProposalSize::UNSPECIFIED).size;
    let snapshot = (measured.width, measured.height);
    if state.sizing_snapshot.get() == Some(snapshot) {
        return;
    }
    state.sizing_snapshot.set(Some(snapshot));
    apply_stretch_sizing(host, &state.leaf);
    host.queue_resize();
}

/// Sizes the host widget to honor the leaf's layout contract instead of
/// assuming it is greedy on both axes: a non-stretch axis takes its extent
/// from the view's own measurement (an aspect-ratio renderer, a fixed-size
/// gauge).
fn apply_stretch_sizing(host: &GpuSurfaceHost, leaf: &HostedLeaf) {
    let stretch = leaf.stretch_axis();
    host.set_hexpand(stretch.stretches_horizontal());
    host.set_vexpand(stretch.stretches_vertical());
    if !(stretch.stretches_horizontal() && stretch.stretches_vertical()) {
        let measured = leaf.measure(ProposalSize::UNSPECIFIED).size;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "GTK size requests are integer logical pixels"
        )]
        host.set_size_request(
            if stretch.stretches_horizontal() {
                -1
            } else {
                measured.width.ceil() as i32
            },
            if stretch.stretches_vertical() {
                -1
            } else {
                measured.height.ceil() as i32
            },
        );
    }
}

/// Installs the layout providers that let the leaf answer for its host:
/// the leaf's own measure answers layout probes live — the intrinsic
/// arrives after creation, and `sync_surface_sizing` renegotiates when it
/// does.
fn install_surface_providers(host: &GpuSurfaceHost, state: &Rc<RefCell<GpuHostState>>) {
    install_measure_provider(host.upcast_ref(), {
        let state = Rc::clone(state);
        move |_, proposal, _axis, _memo| Some(state.borrow().leaf.measure(proposal))
    });
    install_axis_provider(host.upcast_ref(), {
        let state = Rc::clone(state);
        move |_| state.borrow().leaf.stretch_axis()
    });
}

/// Installs the leaf's accessibility label and description on the widget.
fn install_accessibility(host: &GpuSurfaceHost, leaf: &HostedLeaf) {
    if let Some(label) = leaf.accessibility_label() {
        host.update_property(&[gtk4::accessible::Property::Label(label.as_str())]);
    }
    if let Some(value) = leaf.accessibility_value() {
        host.update_property(&[gtk4::accessible::Property::Description(value.as_str())]);
    }
}

/// Delivers the host widget's input to a leaf that asked to handle its
/// own.
struct GpuSurfaceInput {
    host: glib::WeakRef<GpuSurfaceHost>,
}

impl SurfaceInputSink for GpuSurfaceInput {
    fn handle(&self, event: &SurfaceInputEvent) {
        let Some(host) = self.host.upgrade() else {
            return;
        };
        host.state().borrow_mut().leaf.input(event);
        // The view draws its own response to the event, and this widget
        // renders on demand.
        host.queue_draw();
    }
}

impl imp::GpuSurfaceHost {
    /// The `snapshot` vfunc body: ensure the engine stack exists, bind the
    /// leaf, drive one render, and emit the newest presented frame as a
    /// texture node.
    ///
    /// Presentation is fully asynchronous: `engine.render` only waits on
    /// the render thread's submission acceptance — the same request/reply
    /// the old path used — and the dma-buf frames land on the surface's
    /// channel afterwards. A snapshot that arrives between a submission
    /// and its frame re-emits the retained texture; the engine's wake
    /// (`idle_add_once`-deferred, like every wake path) schedules the
    /// snapshot that picks the frame up.
    #[allow(
        clippy::cast_precision_loss,
        reason = "GSK works in f32 logical units; widget sizes are well inside its exact range"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "one sequential bring-up + render pass; splitting it would scatter surface state across helpers"
    )]
    fn snapshot_gpu(&self, snapshot: &gtk4::Snapshot) {
        let obj = self.obj().clone();
        let Some(size) = PixelSize::from_widget(obj.upcast_ref()) else {
            return;
        };
        let Some(state_cell) = self.state.get() else {
            return;
        };

        // Rebuild the whole stack when the driver reports the device dead;
        // the leaf rebinds its content on the new engine below.
        {
            let lost = state_cell
                .borrow()
                .stack
                .as_ref()
                .is_some_and(|stack| stack.loss.is_lost());
            if lost {
                let mut state = state_cell.borrow_mut();
                let reason = state
                    .stack
                    .as_ref()
                    .and_then(|stack| stack.loss.reason())
                    .unwrap_or_default();
                tracing::error!("[gtk-gpu] GPU device lost, rebuilding engine: {reason}");
                state.leaf.unbind();
                state.frames.take();
                state.presented = None;
                state.surface.take();
                state.stack.take();
            }
        }

        {
            let mut state = state_cell.borrow_mut();
            if state.stack.is_none() {
                tracing::debug!(
                    "[gtk-gpu] creating engine host_id={} size={}x{}",
                    obj.as_ptr() as usize,
                    size.width,
                    size.height
                );
                let wake = Arc::clone(&state.wake);
                state.stack = Some(build_engine(&wake));
            }
            let state = &mut *state;
            if state.formats.is_none() {
                // Resolving the set panics when the display cannot import
                // any of the engine's fourccs — an unimportable GPU
                // surface fails at the first frame, not silently empty.
                state.formats = Some(dmabuf_present::display_formats(&obj.display()));
            }
            if state.surface.is_none() {
                let formats = state.formats.as_ref().expect("formats just resolved");
                let (target, frames) = DmabufTarget::new(size.tuple());
                let target = target.formats(formats.clone());
                let surface = state
                    .stack
                    .as_ref()
                    .expect("stack just built")
                    .engine
                    .surface(target)
                    .expect("GpuSurfaceHost: dma-buf surface creation failed");
                state.surface = Some(surface);
                state.frames = Some(frames);
            }
            let stack = state.stack.as_ref().expect("stack just built");
            let surface = state.surface.as_ref().expect("surface just built");
            if surface.size() != size.tuple() {
                surface
                    .resize(size.tuple())
                    .expect("GpuSurfaceHost: surface resize failed");
            }
            let scale = f64::from(obj.scale_factor().max(1));
            surface
                .display(Display {
                    scale,
                    headroom: 1.0,
                })
                .expect("GpuSurfaceHost: display update on a dead render thread");
            state.leaf.before_frame();
            state.leaf.prepare(
                &stack.engine,
                &stack.device,
                surface,
                size,
                scale,
                &state.wake,
            );
        }

        let result = {
            let state = state_cell.borrow();
            state
                .stack
                .as_ref()
                .expect("stack present")
                .engine
                .render(FrameTime::now())
        };

        let next = match result {
            Ok(next) => Some(next),
            Err(RenderError::Hidden) => None,
            Err(error) => {
                // A device that died mid-frame surfaces as a render error;
                // rebuild on the next snapshot rather than panicking on a
                // driver event.
                let lost = state_cell
                    .borrow()
                    .stack
                    .as_ref()
                    .is_some_and(|stack| stack.loss.is_lost());
                if lost {
                    let mut state = state_cell.borrow_mut();
                    state.leaf.unbind();
                    state.frames.take();
                    state.presented = None;
                    state.surface.take();
                    state.stack.take();
                    obj.queue_draw();
                    return;
                }
                panic!("GpuSurfaceHost: engine render failed: {error}");
            }
        };

        // The newest presented frame wins; every older frame was never
        // imported and goes back with its own acquire as the release.
        let mut state = state_cell.borrow_mut();
        let mut latest: Option<DmabufFrame> = None;
        if let Some(frames) = state.frames.as_ref() {
            while let Ok(frame) = frames.try_recv() {
                if let Some(older) = latest.replace(frame) {
                    dmabuf_present::release_unread(older);
                }
            }
        }
        let rect = graphene::Rect::new(0.0, 0.0, obj.width() as f32, obj.height() as f32);
        if let Some(texture) =
            latest.and_then(|frame| dmabuf_present::texture(&obj.display(), frame))
        {
            snapshot.append_texture(&texture, &rect);
            state.presented = Some(texture.upcast());
            state.frames_completed += 1;
            tracing::debug!(
                "[gtk-gpu] frame presented host_id={} seq={}",
                obj.as_ptr() as usize,
                state.frames_completed
            );
        } else if let Some(texture) = state.presented.clone() {
            snapshot.append_texture(&texture, &rect);
        }
        drop(state);
        if matches!(next, Some(Next::At { .. })) {
            // A damage flag alone does not wake the frame clock after it
            // has idled; a tick callback forces the clock to tick once and
            // the callback marks the widget damaged for that tick.
            obj.add_tick_callback(|widget, _clock| {
                widget.queue_draw();
                glib::ControlFlow::Break
            });
        }
    }
}

/// Builds the `GpuSurfaceHost` presenting `leaf`.
fn render_gpu_surface(mut leaf: HostedLeaf) -> gtk4::Widget {
    let creation_measure = leaf.measure(ProposalSize::UNSPECIFIED).size;
    let wants_input_events = leaf.wants_input_events();
    let hdr = leaf.resolved_hdr_preference();

    let host: GpuSurfaceHost = glib::Object::new();
    tracing::debug!(
        "[gtk-gpu] create surface host host_id={}",
        host.as_ptr() as usize
    );

    let wake = make_wake(&host);
    let state = Rc::new(RefCell::new(GpuHostState {
        leaf,
        stack: None,
        surface: None,
        frames: None,
        formats: None,
        presented: None,
        wake,
        frames_completed: 0,
        sizing_snapshot: Cell::new(Some((creation_measure.width, creation_measure.height))),
    }));
    host.imp()
        .state
        .set(Rc::clone(&state))
        .expect("GpuSurfaceHost state installed once");
    {
        let wake = Arc::clone(&state.borrow().wake);
        state.borrow_mut().leaf.mount(&wake);
    }
    install_surface_providers(&host, &state);
    install_accessibility(&host, &state.borrow().leaf);

    if hdr == Some(true) {
        // The dma-buf negotiation only declares SRGB colour states, so an
        // HDR preference resolves to SDR presentation here.
        tracing::debug!(
            "[gtk-gpu] leaf requests HDR; the dma-buf presentation path declares SDR colour states"
        );
    }

    apply_stretch_sizing(&host, &state.borrow().leaf);
    host.set_visible(true);
    host.set_can_target(true);

    if wants_input_events {
        host.set_focusable(true);
        install_surface_input(
            host.upcast_ref(),
            Rc::new(GpuSurfaceInput {
                host: host.downgrade(),
            }),
        );
    }

    host.upcast()
}

impl GtkComponent for Native<GpuContentView> {
    fn render(self, _env: &Environment, _renderer: &mut GtkRenderer) -> Widget {
        render_gpu_surface(HostedLeaf::Content {
            view: self.into_inner(),
            bound: None,
        })
    }
}

impl GtkComponent for Native<SceneView> {
    fn render(self, _env: &Environment, _renderer: &mut GtkRenderer) -> Widget {
        render_gpu_surface(HostedLeaf::Scene {
            leaf: SceneLeaf {
                view: self.into_inner(),
                dirty: Rc::new(Cell::new(true)),
            },
            bound: None,
        })
    }
}

impl GtkComponent for Native<ExternalFrameView> {
    fn render(self, _env: &Environment, _renderer: &mut GtkRenderer) -> Widget {
        render_gpu_surface(HostedLeaf::External {
            view: self.into_inner(),
            bound: None,
        })
    }
}
