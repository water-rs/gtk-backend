//! `GpuSurface` native renderer for GTK (Linux-only).
//!
//! Renders `GpuView`s on the process-shared [`GpuRuntime`] — one real wgpu
//! device for the whole application (Vulkan on this platform) — into an
//! offscreen texture, reads the pixels back, and presents them through a
//! `gtk::Picture` fed with `GdkMemoryTexture`s.
//!
//! The previous implementation hosted every surface in its own `GtkGLArea`
//! with a wgpu device adopted from that widget's GL context. GDK creates the
//! context synchronously inside the widget snapshot, so a screen full of GPU
//! widgets serialized hundreds of `eglCreateContext` calls inside a single
//! frame — under llvmpipe the frame never finishes and the window stays
//! uniformly black (the stress example). The shared device removes the
//! per-widget GL context entirely; `GpuView` is backend-agnostic wgpu, so the
//! Vulkan device serves every view unchanged.
//!
//! Readback makes frames one-present late relative to `render()` submission —
//! acceptable: the alternative is a per-widget GL context, which is the defect
//! being removed.

use std::cell::RefCell;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use waterui_core::layout::ProposalSize;
use waterui_core::{Environment, Native};
use waterui_graphics::GpuRuntime;
use waterui_graphics::gpu_surface::{
    GestureState, GpuContext, GpuFrame, GpuSurface, PointerState, RedrawHandle,
};
use waterui_graphics::input::SurfaceInputEvent;

use super::shared_gpu;
use crate::browser_input::{SurfaceInputSink, install as install_surface_input};
use crate::component::GtkComponent;
use crate::renderer::GtkRenderer;

#[cfg(not(target_os = "linux"))]
compile_error!(
    "GTK GpuSurface implementation is Linux-only. The waterui-gtk crate should not be built on non-Linux targets."
);

/// The offscreen target format every surface renders into. It feeds
/// `gdk::MemoryTexture`, so it must stay a plain 8-bit RGBA layout.
const SURFACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelSize {
    width: u32,
    height: u32,
}

impl PixelSize {
    fn from_widget(widget: &gtk4::Widget) -> Self {
        let scale = widget.scale_factor().max(1) as u32;
        let w = widget.width().max(1) as u32;
        let h = widget.height().max(1) as u32;
        Self {
            width: w.saturating_mul(scale),
            height: h.saturating_mul(scale),
        }
    }
}

/// The live render target: one offscreen texture at the widget's current pixel
/// size. Recreated on resize; the `GpuView` setup itself survives it because
/// the shared device does.
#[derive(Debug)]
struct RenderTarget {
    size: PixelSize,
    texture: wgpu::Texture,
}

#[derive(Debug)]
struct GpuState {
    gpu_surface: Option<GpuSurface>,
    msaa_max_samples: NonZeroU32,
    runtime: Option<GpuRuntime>,
    target: Option<RenderTarget>,

    /// Bumped on unrealize. Async work in flight captures the generation it
    /// started under and discards its result when the widget was torn down.
    generation: u64,
    setup_in_progress: bool,
    setup_done: bool,
    /// A readback task is in flight; the next frame waits for it so renders
    /// and readbacks stay ordered.
    readback_in_flight: bool,
    /// The view asked for another frame inside `render()`.
    view_wants_frame: bool,
    /// The allocation changed since the current target was created.
    size_dirty: bool,

    pointer: PointerState,
    gesture: GestureState,
    pan_active: bool,
    last_pinch_update: Option<Instant>,
    start_time: Instant,
    last_frame_time: Instant,
    redraw_handle: RedrawHandle,
    env: Environment,
}

impl GpuState {
    fn new(gpu_surface: GpuSurface, env: Environment) -> Self {
        let msaa_max_samples = gpu_surface.msaa_sample_limit();
        Self {
            start_time: Instant::now(),
            last_frame_time: Instant::now()
                .checked_sub(Duration::from_secs_f32(1.0 / 60.0))
                .unwrap(),
            gpu_surface: Some(gpu_surface),
            msaa_max_samples,
            runtime: None,
            target: None,
            generation: 0,
            setup_in_progress: false,
            setup_done: false,
            readback_in_flight: false,
            view_wants_frame: true,
            size_dirty: true,
            pointer: PointerState::default(),
            gesture: GestureState::default(),
            pan_active: false,
            last_pinch_update: None,
            redraw_handle: RedrawHandle::new(),
            env,
        }
    }

    /// Whether a frame should be produced: first frame, a view-requested
    /// redraw, an external redraw signal, or a size change.
    fn needs_frame(&self) -> bool {
        self.view_wants_frame || self.size_dirty || self.redraw_handle.is_dirty()
    }
}

/// Everything one `GpuView::setup` pass needs, moved into the async task so it
/// owns a consistent snapshot.
struct SetupInputs {
    gpu_surface: GpuSurface,
    runtime: GpuRuntime,
    msaa_max_samples: NonZeroU32,
    redraw_handle: RedrawHandle,
    env: Environment,
}

/// Installs a thread-safe waker on the surface's `RedrawHandle` so
/// `request_redraw()` from outside `render()` (worker threads, timers,
/// callbacks) schedules a GTK frame instead of waiting for unrelated activity.
fn install_redraw_waker(widget: &gtk4::Picture, state: &Rc<RefCell<GpuState>>) {
    // `WeakRef<Picture>` is not `Send`, so it crosses threads inside a
    // `ThreadGuard` and is only dereferenced back on the main context's
    // thread, where `invoke` runs the closure.
    let widget_guard = Arc::new(gtk4::glib::thread_guard::ThreadGuard::new(
        gtk4::prelude::ObjectExt::downgrade(widget),
    ));
    let main_context = gtk4::glib::MainContext::default();
    let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let widget_guard = Arc::clone(&widget_guard);
        main_context.invoke(move || {
            if let Some(widget) = widget_guard.get_ref().upgrade() {
                widget.queue_draw();
            }
        });
    });
    state.borrow().redraw_handle.set_waker(Some(waker));
}

/// Claims the surface and kicks the async bring-up once: shared runtime, then
/// `GpuView::setup`. Everything after that is frame pumping.
fn ensure_setup(widget: &gtk4::Picture, state: &Rc<RefCell<GpuState>>) {
    enum Next {
        NeedRuntime,
        NeedSetup(Box<SetupInputs>),
        Done,
    }

    let next = {
        let mut st = state.borrow_mut();
        if st.setup_done || st.setup_in_progress {
            Next::Done
        } else if let Some(runtime) = st.runtime.clone() {
            let Some(gpu_surface) = st.gpu_surface.take() else {
                Next::Done
            } else {
                st.setup_in_progress = true;
                Next::NeedSetup(Box::new(SetupInputs {
                    gpu_surface,
                    runtime,
                    msaa_max_samples: st.msaa_max_samples,
                    redraw_handle: st.redraw_handle.clone(),
                    env: st.env.clone(),
                }))
            }
        } else {
            Next::NeedRuntime
        }
    };

    match next {
        Next::Done => {}
        Next::NeedRuntime => {
            let state = Rc::clone(state);
            let widget = widget.clone();
            shared_gpu::ensure_shared_runtime(move |result| match result {
                Ok(runtime) => {
                    state.borrow_mut().runtime = Some(runtime);
                    ensure_setup(&widget, &state);
                }
                Err(error) => {
                    panic!("GpuSurface: shared GPU runtime unavailable: {error}");
                }
            });
        }
        Next::NeedSetup(inputs) => {
            let state = Rc::clone(state);
            let widget = widget.clone();
            let generation = state.borrow().generation;
            let SetupInputs {
                mut gpu_surface,
                runtime,
                msaa_max_samples,
                redraw_handle,
                mut env,
            } = *inputs;
            let context = runtime.context();
            let adapter = context.adapter.clone();
            let device = context.device.clone();
            let queue = context.queue.clone();
            let shader_cache = context.shader_cache.clone();
            let scene_renderer = context.scene_renderer().clone();
            gtk4::glib::MainContext::default().spawn_local(async move {
                let gpu_context = GpuContext::new(
                    &adapter,
                    &device,
                    &queue,
                    SURFACE_FORMAT,
                    &shader_cache,
                    &scene_renderer,
                    msaa_max_samples,
                    redraw_handle,
                );
                gpu_surface.setup(&gpu_context, &mut env).await;
                let mut st = state.borrow_mut();
                st.setup_in_progress = false;
                if st.generation != generation {
                    // The widget was unrealized mid-setup; the next realize
                    // runs the pass again against fresh state.
                    return;
                }
                st.gpu_surface = Some(gpu_surface);
                st.setup_done = true;
                st.view_wants_frame = true;
                drop(st);
                widget.queue_draw();
            });
        }
    }
}

/// Creates the render target texture for `size` on the shared device.
fn create_target(runtime: &GpuRuntime, size: PixelSize) -> RenderTarget {
    let texture = runtime
        .context()
        .device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("waterui_gtk_gpu_surface"),
            size: wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: SURFACE_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
    RenderTarget { size, texture }
}

/// Renders one frame when the view needs it, then reads the texture back into
/// a `GdkMemoryTexture` for presentation. Called from the widget's tick
/// callback, so it runs at most once per GTK frame.
fn pump_frame(widget: &gtk4::Picture, state: &Rc<RefCell<GpuState>>) {
    enum Work {
        Idle,
        Setup,
        Render {
            runtime: GpuRuntime,
            size: PixelSize,
        },
    }

    let work = {
        let st = state.borrow();
        if !st.setup_done || st.readback_in_flight {
            Work::Idle
        } else if st.runtime.is_none() || st.gpu_surface.is_none() {
            Work::Setup
        } else if st.needs_frame() {
            let runtime = st.runtime.clone().expect("runtime checked above");
            Work::Render {
                runtime,
                size: PixelSize::from_widget(widget.upcast_ref()),
            }
        } else {
            Work::Idle
        }
    };

    match work {
        Work::Idle => {}
        Work::Setup => ensure_setup(widget, state),
        Work::Render { runtime, size } => {
                    {
                let mut st = state.borrow_mut();
                // The texture tracks the current allocation; a resize since
                // the last frame recreates it before the view draws.
                if st.target.as_ref().is_none_or(|target| target.size != size) {
                    st.target = Some(create_target(&runtime, size));
                }
                let target = st.target.as_ref().expect("target just ensured");
                let shared = runtime.context();
                let now = Instant::now();
                let delta = now.saturating_duration_since(st.last_frame_time);
                st.last_frame_time = now;
                let elapsed = now.saturating_duration_since(st.start_time);
                let mut frame = GpuFrame::new(
                    shared.device.as_ref(),
                    shared.queue.as_ref(),
                    &target.texture,
                    target
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default()),
                    SURFACE_FORMAT,
                    size.width,
                    size.height,
                    f64::from(widget.scale_factor().max(1)),
                    st.pointer,
                    st.gesture,
                    elapsed,
                    delta,
                );
                let Some(gpu_surface) = st.gpu_surface.as_mut() else {
                    return;
                };
                gpu_surface.render(&mut frame);
                let _ = st.redraw_handle.take_dirty();
                st.view_wants_frame = frame.was_redraw_requested();
                st.size_dirty = false;
                st.readback_in_flight = true;
            }

            let state = Rc::clone(state);
            let widget = widget.clone();
            let generation = state.borrow().generation;
            let texture = {
                let st = state.borrow();
                st.target
                    .as_ref()
                    .expect("target exists after render")
                    .texture
                    .clone()
            };
            gtk4::glib::MainContext::default().spawn_local(async move {
                let pixels = shared_gpu::readback_texture_rgba8(
                    &runtime,
                    &texture,
                    size.width,
                    size.height,
                );
                let mut st = state.borrow_mut();
                st.readback_in_flight = false;
                if st.generation != generation {
                    return;
                }
                match pixels {
                    Ok(pixels) => {
                        let stride = usize::try_from(size.width)
                            .expect("texture width fits usize")
                            * 4;
                        let texture = gdk4::MemoryTexture::new(
                            i32::try_from(size.width).expect("texture width fits i32"),
                            i32::try_from(size.height).expect("texture height fits i32"),
                            gdk4::MemoryFormat::R8g8b8a8Premultiplied,
                            &gtk4::glib::Bytes::from_owned(pixels),
                            stride,
                        );
                        widget.set_paintable(Some(&texture.upcast()));
                    }
                    Err(error) => {
                        tracing::warn!(
                            "[gtk-gpu] frame readback failed; keeping last presented frame: {error}"
                        );
                    }
                }
                drop(st);
                // The view may have requested another frame while this one was
                // being read back; the next tick picks it up.
                widget.queue_draw();
            });
        }
    }
}

fn install_input_controllers(widget: &gtk4::Picture, state: &Rc<RefCell<GpuState>>) {
    let motion = gtk4::EventControllerMotion::new();
    motion.connect_enter({
        let widget = widget.clone();
        let state = Rc::clone(state);
        move |_ctrl, x, y| {
            let mut st = state.borrow_mut();
            let scale = widget.scale_factor().max(1) as f32;
            st.pointer.position = Some(waterui_core::layout::Point::new(
                x as f32 * scale,
                y as f32 * scale,
            ));
            widget.queue_draw();
        }
    });
    motion.connect_motion({
        let widget = widget.clone();
        let state = Rc::clone(state);
        move |_ctrl, x, y| {
            let mut st = state.borrow_mut();
            let scale = widget.scale_factor().max(1) as f32;
            st.pointer.position = Some(waterui_core::layout::Point::new(
                x as f32 * scale,
                y as f32 * scale,
            ));
            widget.queue_draw();
        }
    });
    motion.connect_leave({
        let widget = widget.clone();
        let state = Rc::clone(state);
        move |_ctrl| {
            let mut st = state.borrow_mut();
            st.pointer.position = None;
            widget.queue_draw();
        }
    });
    widget.add_controller(motion);

    let click = gtk4::GestureClick::new();
    click.set_button(0);
    click.connect_pressed({
        let widget = widget.clone();
        let state = Rc::clone(state);
        move |_gesture, _n_press, x, y| {
            let mut st = state.borrow_mut();
            let scale = widget.scale_factor().max(1) as f32;
            let p = waterui_core::layout::Point::new(x as f32 * scale, y as f32 * scale);
            st.pointer.hit = Some(p);
            widget.queue_draw();
        }
    });
    click.connect_released({
        let widget = widget.clone();
        let state = Rc::clone(state);
        move |_gesture, n_press, _x, _y| {
            let mut st = state.borrow_mut();
            st.pointer.hit = None;
            if n_press >= 2 {
                st.gesture.double_tap = true;
                st.gesture.active = false;
                st.gesture.pinch_scale = 1.0;
                st.gesture.pinch_center = None;
                st.gesture.pan_offset = waterui_core::layout::Point::new(0.0, 0.0);
                st.pan_active = false;
                st.last_pinch_update = None;
            }
            widget.queue_draw();
        }
    });
    widget.add_controller(click);

    let pan = gtk4::GestureDrag::new();
    pan.set_button(0);
    pan.connect_drag_begin({
        let widget = widget.clone();
        let state = Rc::clone(state);
        move |_gesture, _x, _y| {
            let mut st = state.borrow_mut();
            st.pan_active = true;
            st.gesture.active = true;
            st.gesture.pan_offset = waterui_core::layout::Point::new(0.0, 0.0);
            widget.queue_draw();
        }
    });
    pan.connect_drag_update({
        let widget = widget.clone();
        let state = Rc::clone(state);
        move |_gesture, offset_x, offset_y| {
            let mut st = state.borrow_mut();
            let scale = widget.scale_factor().max(1) as f32;
            st.gesture.active = true;
            st.gesture.pan_offset =
                waterui_core::layout::Point::new(offset_x as f32 * scale, offset_y as f32 * scale);
            widget.queue_draw();
        }
    });
    pan.connect_drag_end({
        let widget = widget.clone();
        let state = Rc::clone(state);
        move |_gesture, _offset_x, _offset_y| {
            let mut st = state.borrow_mut();
            st.pan_active = false;
            st.gesture.pan_offset = waterui_core::layout::Point::new(0.0, 0.0);
            if st.last_pinch_update.is_none() {
                st.gesture.active = false;
            }
            widget.queue_draw();
        }
    });
    widget.add_controller(pan);

    let zoom = gtk4::GestureZoom::new();
    zoom.connect_scale_changed({
        let widget = widget.clone();
        let state = Rc::clone(state);
        move |gesture, scale| {
            let mut st = state.borrow_mut();
            let scale_factor = widget.scale_factor().max(1) as f32;
            st.gesture.active = true;
            st.gesture.pinch_scale = scale as f32;
            st.gesture.pinch_center = gesture.bounding_box().map(|bbox| {
                let center_x = (bbox.width() as f32).mul_add(0.5, bbox.x() as f32);
                let center_y = (bbox.height() as f32).mul_add(0.5, bbox.y() as f32);
                waterui_core::layout::Point::new(center_x * scale_factor, center_y * scale_factor)
            });
            st.last_pinch_update = Some(Instant::now());
            widget.queue_draw();
        }
    });
    widget.add_controller(zoom);
}

/// Sizes the widget to honor the surface's layout contract instead of
/// assuming it is greedy on both axes: a non-stretch axis takes its extent
/// from the view's own measurement (an aspect-ratio renderer, a fixed-size
/// gauge).
fn apply_stretch_sizing(widget: &gtk4::Picture, gpu_surface: &GpuSurface) {
    let stretch = gpu_surface.stretch_axis();
    widget.set_hexpand(stretch.stretches_horizontal());
    widget.set_vexpand(stretch.stretches_vertical());
    if !(stretch.stretches_horizontal() && stretch.stretches_vertical()) {
        let measured = gpu_surface.measure(ProposalSize::UNSPECIFIED).size;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "GTK size requests are integer logical pixels"
        )]
        widget.set_size_request(
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

/// Delivers widget input to a GPU view that asked to handle its own.
///
/// The surface is briefly absent from the state while an in-flight `setup`
/// owns it; an event that lands then is dropped, which is what a view that has
/// not finished initializing can do with it anyway.
struct GpuSurfaceInput {
    widget: gtk4::Picture,
    state: Rc<RefCell<GpuState>>,
}

impl GpuSurfaceInput {
    fn new(widget: &gtk4::Picture, state: &Rc<RefCell<GpuState>>) -> Self {
        Self {
            widget: widget.clone(),
            state: Rc::clone(state),
        }
    }
}

impl SurfaceInputSink for GpuSurfaceInput {
    fn handle(&self, event: &SurfaceInputEvent) {
        {
            let mut state = self.state.borrow_mut();
            let Some(surface) = state.gpu_surface.as_mut() else {
                return;
            };
            surface.input(event);
        }
        // The view draws its own response to the event, and this widget renders
        // on demand.
        self.widget.queue_draw();
    }
}

pub(crate) fn render_gpu_surface(gpu_surface: GpuSurface, env: Environment) -> gtk4::Widget {
    let widget = gtk4::Picture::new();
    apply_stretch_sizing(&widget, &gpu_surface);
    widget.set_content_fit(gtk4::ContentFit::Fill);
    widget.set_visible(true);
    widget.set_can_target(true);

    let wants_input_events = gpu_surface.wants_input_events();
    let state = Rc::new(RefCell::new(GpuState::new(gpu_surface, env)));
    install_input_controllers(&widget, &state);
    // A view that draws its own interactive content — a browser page, a
    // terminal, an editor — takes the raw events instead of the per-frame
    // pointer snapshot. GTK delivers them to the widget's own event
    // controllers, so this is where the routing lives.
    if wants_input_events {
        widget.set_focusable(true);
        install_surface_input(
            widget.upcast_ref(),
            Rc::new(GpuSurfaceInput::new(&widget, &state)),
        );
    }

    widget.connect_realize({
        let state = Rc::clone(&state);
        move |widget| {
            install_redraw_waker(widget, &state);
            ensure_setup(widget, &state);
        }
    });

    widget.connect_resize({
        let state = Rc::clone(&state);
        move |widget, _width, _height| {
            let mut st = state.borrow_mut();
            st.size_dirty = true;
            drop(st);
            widget.queue_draw();
        }
    });

    widget.connect_unrealize({
        let state = Rc::clone(&state);
        move |_widget| {
            let mut st = state.borrow_mut();
            // In-flight async work must not resurrect state for a torn-down
            // widget; the shared device outlives unrealize, so the session and
            // the completed setup can be kept — only pending results die.
            st.generation += 1;
            st.view_wants_frame = true;
            st.size_dirty = true;
        }
    });

    widget.add_tick_callback({
        let state = Rc::clone(&state);
        move |widget, _clock| {
            let widget = widget
                .downcast_ref::<gtk4::Picture>()
                .expect("tick callback is on the Picture");
            ensure_setup(widget, &state);
            pump_frame(widget, &state);
            gtk4::glib::ControlFlow::Continue
        }
    });

    widget.upcast()
}

impl GtkComponent for Native<GpuSurface> {
    fn render(self, env: &Environment, _renderer: &mut GtkRenderer) -> gtk4::Widget {
        render_gpu_surface(self.into_inner(), env.clone())
    }
}
