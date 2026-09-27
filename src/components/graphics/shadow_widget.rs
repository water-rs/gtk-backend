//! A GTK widget that draws a shadow of its child's silhouette behind it.
//!
//! The shadow's outline resolves in [`WidgetImpl::snapshot`], where the
//! widget's allocated size is finally known, through the same
//! [`crate::shape_geometry`] resolver the clip uses — the silhouette a
//! `Metadata<Shadow>` carries is the same (kind, unit-space commands) pair a
//! clip shape carries. CSS cannot express this: `box-shadow` reads
//! `border-radius` per axis, where a percentage that is round on a square
//! surface is elliptical on every other one.
//!
//! `gsk`'s outset shadow takes only a rounded-rect outline, which covers every
//! kind but a custom path; a custom path shadows through `push_shadow` of the
//! silhouette fill — the child drawn on top then covers the fill itself.

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{Widget, glib, graphene, gsk};
use waterui_shape::{PathCommand, ShapeKind};

use super::gsk_path;
use crate::shape_geometry::{Corner, ShapeGeometry, resolve};
use crate::shape_path::bez_path;

mod imp {
    // The glib subclass macros expand against the parent scope, so this module
    // deliberately re-exports it wholesale rather than tracking each generated use.
    #[allow(
        clippy::wildcard_imports,
        reason = "glib subclass macros expand against the parent scope"
    )]
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Debug, Default)]
    pub struct WuiShadow {
        pub kind: Cell<ShapeKind>,
        /// The unit-space outline, consulted only for a custom path: every
        /// other kind resolves through `shape_geometry`.
        pub commands: RefCell<Vec<PathCommand>>,
        pub offset: Cell<(f32, f32)>,
        pub blur: Cell<f32>,
        pub color: Cell<gdk4::RGBA>,
        pub child: RefCell<Option<Widget>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for WuiShadow {
        const NAME: &'static str = "WuiShadow";
        type Type = super::WuiShadow;
        type ParentType = Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk4::BinLayout>();
        }
    }

    impl ObjectImpl for WuiShadow {
        fn dispose(&self) {
            if let Some(child) = self.child.borrow_mut().take() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for WuiShadow {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "GSK geometry is f32 while the resolved shape geometry is f64"
        )]
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let widget = self.obj();
            let width = f64::from(widget.width());
            let height = f64::from(widget.height());
            let (dx, dy) = self.offset.get();
            let blur = self.blur.get().max(0.0);
            let color = self.color.get();

            match resolve(self.kind.get(), width, height) {
                ShapeGeometry::Rounded(rect) => {
                    let bounds = graphene::Rect::new(
                        rect.x as f32,
                        rect.y as f32,
                        rect.width as f32,
                        rect.height as f32,
                    );
                    let size = |corner: Corner| {
                        graphene::Size::new(corner.horizontal as f32, corner.vertical as f32)
                    };
                    let [top_left, top_right, bottom_right, bottom_left] = rect.corners;
                    snapshot.append_outset_shadow(
                        &gsk::RoundedRect::new(
                            bounds,
                            size(top_left),
                            size(top_right),
                            size(bottom_right),
                            size(bottom_left),
                        ),
                        &color,
                        dx,
                        dy,
                        0.0,
                        blur,
                    );
                }
                ShapeGeometry::CustomPath => {
                    // No outset-shadow node takes an arbitrary path: shadow the
                    // silhouette fill itself and let the content drawn on top
                    // cover the fill.
                    let path = gsk_path(&bez_path(&self.commands.borrow(), width, height));
                    snapshot.push_shadow(&[gsk::Shadow::new(color, dx, dy, blur)]);
                    let bounds = graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
                    snapshot.append_node(&gsk::FillNode::new(
                        &gsk::ColorNode::new(&color, &bounds),
                        &path,
                        gsk::FillRule::Winding,
                    ));
                    snapshot.pop();
                }
            }

            self.parent_snapshot(snapshot);
        }
    }
}

glib::wrapper! {
    /// A single-child widget that draws a silhouette shadow behind its child.
    pub struct WuiShadow(ObjectSubclass<imp::WuiShadow>)
        @extends Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl WuiShadow {
    /// Wraps `child` in a widget that casts `color`'s shadow of `kind` — or of
    /// `commands` when the kind is [`ShapeKind::CustomPath`] — offset by
    /// (`offset_x`, `offset_y`) points and blurred by `radius`.
    #[must_use]
    pub fn new(
        kind: ShapeKind,
        commands: &[PathCommand],
        offset_x: f32,
        offset_y: f32,
        radius: f32,
        color: gdk4::RGBA,
        child: &Widget,
    ) -> Self {
        let widget: Self = glib::Object::new();
        widget.set_halign(gtk4::Align::Fill);
        widget.set_valign(gtk4::Align::Fill);
        widget.imp().kind.set(kind);
        *widget.imp().commands.borrow_mut() = commands.to_vec();
        widget.imp().offset.set((offset_x, offset_y));
        widget.imp().blur.set(radius);
        widget.imp().color.set(color);
        child.set_parent(&widget);
        *widget.imp().child.borrow_mut() = Some(child.clone());
        // `BinLayout` makes the widget exactly its child; every layout
        // channel reads through to the content it shadows.
        crate::layout::proposal::transparent_to_content(widget.upcast_ref(), child);
        widget
    }

    /// Updates the shadow color and redraws.
    pub fn set_color(&self, color: gdk4::RGBA) {
        self.imp().color.set(color);
        self.queue_draw();
    }
}
