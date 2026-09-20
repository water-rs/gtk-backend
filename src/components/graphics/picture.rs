//! A `Picture` shown by a `gtk4::Picture`, rasterised on the CPU at the
//! size and scale the widget is drawn at and re-rasterised when the drawing
//! changes.
//!
//! The measurement contract is the one the Apple backend documents in
//! `WuiPictureView.sizeThatFits`: a proposed side is the layout's decision,
//! an unproposed side falls back to the recording's intrinsic size, and the
//! drawing is rasterised at whatever size it is given. `RecordingPaintable`
//! delivers it by reporting the intrinsic size as the paintable's while
//! `can_shrink` keeps the widget's GTK minimum at zero — so `leaf_measure`'s
//! minimum floor has nothing to raise, and a 16-px proposal on a 24-px
//! drawing answers 16.

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{Widget, gdk, glib, graphene};
use waterui_core::{Environment, Native, Signal};
use waterui_graphics::Picture;
use waterui_graphics::scene2d_cpu::rasterize_recording;

use crate::component::GtkComponent;
use crate::renderer::GtkRenderer;
use crate::util::store_watcher_guard;

mod imp {
    // The glib subclass macros expand against the parent scope, so this module
    // deliberately re-exports it wholesale rather than tracking each generated use.
    #[allow(
        clippy::wildcard_imports,
        reason = "glib subclass macros expand against the parent scope"
    )]
    use super::*;
    use std::cell::{Cell, OnceCell, RefCell};

    use gtk4::gdk::subclass::prelude::PaintableImpl;

    /// A raster drawn for one snapshot, kept so a repaint at the same pixel
    /// size does not run the rasteriser again.
    #[derive(Debug)]
    pub struct Raster {
        /// The pixel dimensions `texture` was rendered at.
        pub pixel_size: (u32, u32),
        pub texture: gdk::MemoryTexture,
    }

    #[derive(Debug, Default)]
    pub struct RecordingPaintable {
        /// The drawing being shown, installed by `RecordingPaintable::new`.
        pub picture: OnceCell<Picture>,
        /// The scale factor the widget draws at. `gdk::Snapshot` does not
        /// report it, so `connect_scale_factor_notify` mirrors it here.
        pub scale: Cell<f64>,
        /// The last raster; dropped when the recording changes so the next
        /// snapshot re-rasterises instead of replaying stale pixels.
        pub raster: RefCell<Option<Raster>>,
    }

    impl RecordingPaintable {
        /// The installed picture; `new` sets it before the paintable is shared.
        fn picture(&self) -> &Picture {
            self.picture
                .get()
                .expect("RecordingPaintable::new installs the picture")
        }

        /// The whole-pixel extent `points` rasterises to at the widget's
        /// scale — at least one pixel and within the rasteriser's
        /// 65535-addressable side.
        fn pixel_extent(&self, points: f64) -> u32 {
            let rounded = (points * self.scale.get()).round().max(1.0);
            assert!(
                rounded <= f64::from(u16::MAX),
                "a picture is at most 65535 pixels a side, got {rounded}"
            );
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "rounded to an integer and range-checked just above"
            )]
            let pixels = rounded as u32;
            pixels
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RecordingPaintable {
        const NAME: &'static str = "WuiRecordingPaintable";
        type Type = super::RecordingPaintable;
        type ParentType = glib::Object;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for RecordingPaintable {}

    impl PaintableImpl for RecordingPaintable {
        /// The intrinsic size never changes; the contents do, with the
        /// recording.
        fn flags(&self) -> gdk::PaintableFlags {
            gdk::PaintableFlags::STATIC_SIZE
        }

        /// The drawing's own size, rounded to whole points. The widget
        /// adopts it as its natural size; the minimum stays zero because the
        /// widget allows shrinking.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "Picture::new asserts a finite positive size; one past i32 is not a widget GTK can show"
        )]
        fn intrinsic_width(&self) -> i32 {
            self.picture().size().width.round() as i32
        }

        #[expect(
            clippy::cast_possible_truncation,
            reason = "Picture::new asserts a finite positive size; one past i32 is not a widget GTK can show"
        )]
        fn intrinsic_height(&self) -> i32 {
            self.picture().size().height.round() as i32
        }

        fn intrinsic_aspect_ratio(&self) -> f64 {
            let size = self.picture().size();
            f64::from(size.width) / f64::from(size.height)
        }

        /// `width` and `height` are the allocation in points: the recording
        /// is replayed into a texture of that many points at the widget's
        /// scale factor, never stretched from a raster baked at the
        /// intrinsic size.
        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            let pixel_size = (self.pixel_extent(width), self.pixel_extent(height));
            let mut raster = self.raster.borrow_mut();
            let texture = match raster.as_ref() {
                Some(raster) if raster.pixel_size == pixel_size => raster.texture.clone(),
                _ => {
                    let picture = self.picture();
                    #[expect(
                        clippy::cast_precision_loss,
                        reason = "pixel_size bounds both sides to 65535, which f32 holds exactly"
                    )]
                    let transform = picture.transform_to(pixel_size.0 as f32, pixel_size.1 as f32);
                    let recording = picture.recording().get();
                    let bitmap =
                        rasterize_recording(&recording, pixel_size.0, pixel_size.1, transform);
                    let stride =
                        usize::try_from(pixel_size.0 * 4).expect("a bitmap row fits usize");
                    let bytes = glib::Bytes::from_owned(bitmap.into_data());
                    let texture = gdk::MemoryTexture::new(
                        i32::try_from(pixel_size.0).expect("a bitmap side fits i32"),
                        i32::try_from(pixel_size.1).expect("a bitmap side fits i32"),
                        gdk::MemoryFormat::R8g8b8a8Premultiplied,
                        &bytes,
                        stride,
                    );
                    *raster = Some(Raster {
                        pixel_size,
                        texture: texture.clone(),
                    });
                    texture
                }
            };
            let snapshot = snapshot
                .downcast_ref::<gtk4::Snapshot>()
                .expect("GTK snapshots paintables through GtkSnapshot");
            #[expect(
                clippy::cast_possible_truncation,
                reason = "GSK geometry is f32 while the paintable size is f64"
            )]
            let bounds = graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
            snapshot.append_texture(&texture, &bounds);
        }
    }
}

glib::wrapper! {
    /// A `gdk::Paintable` that rasterises a [`Picture`] recording at the
    /// size GTK asks it to draw.
    ///
    /// GTK redraws a paintable on every frame, so the raster is cached by
    /// pixel size: a repaint at an unchanged size reuses the texture, a new
    /// size re-rasterises from the recording rather than scaling pixels.
    pub struct RecordingPaintable(ObjectSubclass<imp::RecordingPaintable>)
        @implements gdk::Paintable;
}

impl RecordingPaintable {
    fn new(picture: Picture, scale: f64) -> Self {
        let paintable: Self = glib::Object::new();
        paintable.imp().scale.set(scale);
        paintable
            .imp()
            .picture
            .set(picture)
            .expect("a fresh paintable holds no picture yet");
        paintable
    }

    /// The drawing's signal fired: the cached raster is stale and the
    /// paintable must redraw.
    fn recording_changed(&self) {
        self.imp().raster.take();
        self.invalidate_contents();
    }

    /// The widget's scale factor changed: the next snapshot rasterises at
    /// the new scale.
    fn scale_changed(&self, scale: f64) {
        self.imp().scale.set(scale);
        self.invalidate_contents();
    }
}

impl GtkComponent for Native<Picture> {
    fn render(self, _env: &Environment, _renderer: &mut GtkRenderer) -> Widget {
        let picture = self.into_inner();
        let widget = gtk4::Picture::new();
        // The paintable rasterises at the size it is given, so the widget
        // carries no minimum of its own: GTK reports a zero minimum and a
        // natural size equal to the recording's intrinsic size, which is
        // what lets a `.size(16, 16)` proposal around a 24-px drawing be
        // answered with 16 instead of the old `set_size_request` floor.
        widget.set_can_shrink(true);
        // `Fill` fits the contract: the paintable draws at exactly the size
        // it is handed, so there is no fixed raster to letterbox.
        widget.set_content_fit(gtk4::ContentFit::Fill);
        // The name the drawing offers; an application's own `.a11y_label(…)`
        // is applied to this same widget afterwards and so replaces it.
        if let Some(label) = picture.label() {
            widget.update_property(&[gtk4::accessible::Property::Label(label.as_str())]);
        }
        // What the drawing says lives on the description property — the value
        // channel's AT-SPI analogue — so a label never has to carry it.
        if let Some(value) = picture.value() {
            widget.update_property(&[gtk4::accessible::Property::Description(value.as_str())]);
        }

        let paintable = RecordingPaintable::new(picture.clone(), f64::from(widget.scale_factor()));
        widget.set_paintable(Some(&paintable));
        widget.connect_scale_factor_notify({
            let paintable = paintable.clone();
            move |widget| paintable.scale_changed(f64::from(widget.scale_factor()))
        });
        let guard = picture.recording().watch(move |_| {
            let paintable = paintable.clone();
            glib::idle_add_local_once(move || paintable.recording_changed());
        });
        store_watcher_guard(&widget, Box::new(guard));
        widget.upcast()
    }
}

#[cfg(test)]
mod tests {
    use nami::Computed;
    use waterui_core::layout::{ProposalSize, Size, StretchAxis, SubView};
    use waterui_core::{AnyView, Environment, Native, Str};
    use waterui_layout::frame::Frame;
    use waterui_layout::stack::hstack;

    use super::*;
    use crate::layout::subview::GtkSubView;

    fn init() {
        gtk4::init().expect("GTK tests need a display; run them under xvfb-run");
    }

    /// The `reminders` row: a drawing with an intrinsic size of 24×24 under
    /// `.size(16, 16)` next to a label. The picture must answer the proposed
    /// 16, not its intrinsic 24 — GTK's minimum is zero because the widget
    /// allows shrinking — and the row centres the 16-px icon on its cross
    /// axis instead of pinning it to the bottom (issue #98).
    #[test]
    fn sized_picture_answers_the_proposal_and_centres_in_the_row() {
        init();
        let env = Environment::new();
        let mut renderer = GtkRenderer::new();

        let make_icon = || {
            Picture::new(
                Size::new(24.0, 24.0),
                Computed::constant(Picture::record(|_| {})),
            )
        };

        // The leaf contract, straight from `leaf_measure`: unproposed is the
        // intrinsic size, proposed is the proposal.
        let icon = Native::new(make_icon()).render(&env, &mut renderer);
        let subview = GtkSubView::new(icon, StretchAxis::None);
        assert_eq!(
            subview.measure(ProposalSize::UNSPECIFIED).size,
            Size::new(24.0, 24.0),
            "an unproposed picture must answer its intrinsic size"
        );
        assert_eq!(
            subview
                .measure(ProposalSize::new(Some(16.0), Some(16.0)))
                .size,
            Size::new(16.0, 16.0),
            "a 16-px proposal was floored at GTK's intrinsic minimum"
        );

        // The same negotiation through `.size(16, 16)` inside an hstack: the
        // row is 64 px tall, so a centred 16-px icon sits at y = 24.
        let row = renderer.render_any(
            AnyView::new(hstack((
                Frame::new(Native::new(make_icon()))
                    .width(16.0)
                    .height(16.0),
                Str::from("row text"),
            ))),
            &env,
        );
        row.allocate(320, 64, -1, None);

        let picture_widget = row
            .first_child()
            .and_then(|frame| frame.first_child())
            .expect("the row holds the framed picture widget");
        assert!(picture_widget.is::<gtk4::Picture>());
        assert_eq!(
            (picture_widget.width(), picture_widget.height()),
            (16, 16),
            "the icon was allocated its intrinsic size instead of the proposal"
        );
        let origin = picture_widget
            .compute_point(&row, &graphene::Point::zero())
            .expect("the picture shares the row's widget tree");
        // GTK allocates whole pixels, so the origin is exact once rounded.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "an allocated origin is a whole pixel; rounding first makes the cast exact"
        )]
        let origin = (origin.x().round() as i32, origin.y().round() as i32);
        assert_eq!(origin, (0, 24), "the icon is not centred on the 64-px row");
    }
}
