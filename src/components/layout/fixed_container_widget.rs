//! A GTK widget implementing `WaterUI`'s `FixedContainer` layout contract.
//!
//! This is a plain `gtk4::Widget` subclass that delegates measurement and
//! placement to `WaterUI`'s Rust `Layout` engine, mirroring the Apple backend
//! behavior. Children are parented with `set_parent` and allocated directly
//! from `size_allocate` — the layout engine's rects are authoritative, so no
//! GTK layout manager or size request sits between the engine and the child.

use std::cell::RefCell;

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{Widget, glib};
use waterui_core::layout::{
    Layout, ProposalSize, Rect, Size, StretchAxis, SubView, ViewDimensions, measure_layout,
    with_memoized_children,
};

use crate::layout::{FixedSizeSubView, GtkSubView, apply_rects};

fn layout_debug_enabled() -> bool {
    std::env::var_os("WATERUI_GTK_LAYOUT_DEBUG").is_some()
}

fn trace_layout_rects(
    operation: &str,
    width: i32,
    height: i32,
    child_count: usize,
    rects: &[Rect],
) {
    tracing::debug!(
        target: "waterui::gtk::layout",
        operation,
        width,
        height,
        child_count,
        rect_count = rects.len(),
        "Laid out GTK fixed container"
    );
    for (index, rect) in rects.iter().enumerate().take(8) {
        tracing::debug!(
            target: "waterui::gtk::layout",
            operation,
            index,
            x = rect.x(),
            y = rect.y(),
            width = rect.width(),
            height = rect.height(),
            "GTK fixed-container child rectangle"
        );
    }
}

mod imp {
    // The glib subclass macros expand against the parent scope, so this module
    // deliberately re-exports it wholesale rather than tracking each generated use.
    #[allow(
        clippy::wildcard_imports,
        reason = "glib subclass macros expand against the parent scope"
    )]
    use super::*;

    #[derive(Debug, Default)]
    pub struct WuiFixedContainer {
        pub layout: RefCell<Option<Box<dyn Layout>>>,
        pub children: RefCell<Vec<(Widget, StretchAxis)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for WuiFixedContainer {
        const NAME: &'static str = "WuiFixedContainer";
        type Type = super::WuiFixedContainer;
        type ParentType = Widget;
    }

    impl ObjectImpl for WuiFixedContainer {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for WuiFixedContainer {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
        )]
        fn measure(&self, orientation: gtk4::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let layout_borrow = self.layout.borrow();
            let Some(layout) = layout_borrow.as_ref() else {
                panic!("WuiFixedContainer: missing layout (internal error)");
            };

            let children = self.children.borrow();
            if children.is_empty() {
                return (0, 0, -1, -1);
            }

            let subviews: Vec<GtkSubView> = children
                .iter()
                .map(|(w, axis)| GtkSubView::new(w.clone(), *axis))
                .collect();

            let refs: Vec<&dyn SubView> = subviews.iter().map(|v| v as &dyn SubView).collect();

            let proposal = if for_size >= 0 {
                let v = for_size as f32;
                match orientation {
                    gtk4::Orientation::Horizontal => ProposalSize::new(None, Some(v)),
                    gtk4::Orientation::Vertical => ProposalSize::new(Some(v), None),
                    _ => ProposalSize::UNSPECIFIED,
                }
            } else {
                ProposalSize::UNSPECIFIED
            };

            let size = measure_layout(layout.as_ref(), proposal, &refs).size;
            let w = size.width.max(0.0).round() as i32;
            let h = size.height.max(0.0).round() as i32;

            // GTK parents that cannot scroll an axis — a ScrolledWindow's
            // NEVER direction, a window's minimum size — read the *minimum*
            // and clamp their allocation to it. Reporting the natural size as
            // the minimum makes a wrapping label's full line the floor of a
            // whole scrollable stack, so the scroll allocates the content
            // wider than the viewport and the text never sees a bounded width
            // to wrap at. Ask each child for its GTK minimum — for_size is
            // the cross-axis constraint, so it propagates to the measured
            // orientation only — then let the layout aggregate those floors
            // (an hstack sums them, a vstack takes the widest, padding adds
            // its insets).
            let min_subviews: Vec<FixedSizeSubView> = children
                .iter()
                .map(|(widget, axis)| {
                    let (min_w, min_h) = match orientation {
                        gtk4::Orientation::Horizontal => {
                            let (min_w, ..) =
                                widget.measure(gtk4::Orientation::Horizontal, for_size);
                            let (min_h, ..) = widget.measure(gtk4::Orientation::Vertical, -1);
                            (min_w, min_h)
                        }
                        gtk4::Orientation::Vertical => {
                            let (min_w, ..) = widget.measure(gtk4::Orientation::Horizontal, -1);
                            let (min_h, ..) = widget.measure(gtk4::Orientation::Vertical, for_size);
                            (min_w, min_h)
                        }
                        _ => panic!("WuiFixedContainer: unexpected orientation {orientation:?}"),
                    };
                    FixedSizeSubView::new(
                        Size::new(min_w.max(0) as f32, min_h.max(0) as f32),
                        *axis,
                    )
                })
                .collect();
            let min_refs: Vec<&dyn SubView> =
                min_subviews.iter().map(|v| v as &dyn SubView).collect();
            let min_size =
                measure_layout(layout.as_ref(), ProposalSize::UNSPECIFIED, &min_refs).size;
            let min_w = (min_size.width.max(0.0).round() as i32).min(w);
            let min_h = (min_size.height.max(0.0).round() as i32).min(h);

            if layout_debug_enabled() {
                tracing::debug!(
                    target: "waterui::gtk::layout",
                    orientation = ?orientation,
                    for_size,
                    proposal_width = ?proposal.width,
                    proposal_height = ?proposal.height,
                    min_width = min_w,
                    min_height = min_h,
                    width = w,
                    height = h,
                    child_count = refs.len(),
                    "Measured GTK fixed container"
                );
            }
            match orientation {
                gtk4::Orientation::Horizontal => (min_w, w, -1, -1),
                gtk4::Orientation::Vertical => (min_h, h, -1, -1),
                _ => panic!("WuiFixedContainer: unexpected orientation {orientation:?}"),
            }
        }

        #[allow(
            clippy::cast_precision_loss,
            reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
        )]
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            self.obj().place_children(width, height, "allocate");
        }
    }
}

glib::wrapper! {
    pub struct WuiFixedContainer(ObjectSubclass<imp::WuiFixedContainer>)
        @extends Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl WuiFixedContainer {
    pub(crate) fn layout_measure(&self, proposal: ProposalSize) -> ViewDimensions {
        let imp = self.imp();
        let layout_borrow = imp.layout.borrow();
        let Some(layout) = layout_borrow.as_ref() else {
            panic!("WuiFixedContainer: missing layout (internal error)");
        };

        let children = imp.children.borrow();
        if children.is_empty() {
            return ViewDimensions::new(Size::zero());
        }

        let subviews: Vec<GtkSubView> = children
            .iter()
            .map(|(w, axis)| GtkSubView::new(w.clone(), *axis))
            .collect();
        let refs: Vec<&dyn SubView> = subviews.iter().map(|v| v as &dyn SubView).collect();

        measure_layout(layout.as_ref(), proposal, &refs)
    }

    /// Runs the layout engine at `width`×`height` and allocates each child at
    /// its resulting rect.
    #[allow(
        clippy::cast_precision_loss,
        reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
    )]
    fn place_children(&self, width: i32, height: i32, operation: &'static str) {
        let imp = self.imp();
        let layout_borrow = imp.layout.borrow();
        let Some(layout) = layout_borrow.as_ref() else {
            panic!("WuiFixedContainer: missing layout (internal error)");
        };

        let children = imp.children.borrow();
        if children.is_empty() {
            return;
        }

        let subviews: Vec<GtkSubView> = children
            .iter()
            .map(|(w, axis)| GtkSubView::new(w.clone(), *axis))
            .collect();
        let refs: Vec<&dyn SubView> = subviews.iter().map(|v| v as &dyn SubView).collect();

        let bounds = Rect::from_size(Size {
            width: (width.max(0)) as f32,
            height: (height.max(0)) as f32,
        });

        // Measure first with bounds-based proposal so children know available width/height.
        let proposal = ProposalSize::new(Some(bounds.width()), Some(bounds.height()));
        let rects = with_memoized_children(&refs, |refs| {
            let _ = layout.size_that_fits(proposal, refs);
            layout.place(bounds, refs)
        });
        if layout_debug_enabled() {
            trace_layout_rects(operation, width, height, children.len(), &rects);
        }
        apply_rects(&rects, &children);
    }

    /// Creates a container that lays `children` out with `layout`.
    #[must_use]
    pub fn new(layout: Box<dyn Layout>, children: Vec<(Widget, StretchAxis)>) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        if layout_debug_enabled() {
            tracing::debug!(
                target: "waterui::gtk::layout",
                widget_type = %obj.type_().name(),
                child_count = children.len(),
                "Created GTK fixed container"
            );
        }

        *imp.layout.borrow_mut() = Some(layout);
        *imp.children.borrow_mut() = children;

        for (child, _) in imp.children.borrow().iter() {
            child.set_parent(&obj);
        }

        obj
    }

    /// Replaces the container's children and reflows them under the stored
    /// layout.
    ///
    /// `LazyContainer` realizations whose layout is not a virtualizable stack
    /// materialize their whole membership here — those containers (the
    /// snackbar overlay's absolute layer) carry a handful of children, so
    /// rebuilding the set on every reconcile is the right granularity.
    pub fn set_children(&self, children: Vec<(Widget, StretchAxis)>) {
        let imp = self.imp();
        for (child, _) in std::mem::take(&mut *imp.children.borrow_mut()) {
            child.unparent();
        }
        for (child, _) in &children {
            child.set_parent(self);
        }
        *imp.children.borrow_mut() = children;
        self.queue_resize();
        // Reflow immediately at the current allocation so freshly materialized
        // children do not sit unallocated for a frame.
        let (width, height) = (self.width(), self.height());
        if width > 0 && height > 0 {
            self.place_children(width, height, "set_children");
        }
    }
}

#[cfg(test)]
mod tests {
    use gtk4::Label;
    use waterui_core::layout::{ProposalSize, StretchAxis, SubView};
    use waterui_layout::stack::VStackLayout;

    use super::*;
    use crate::layout::subview::GtkSubView;

    fn init() {
        gtk4::init().expect("GTK tests need a display; run them under xvfb-run");
    }

    /// A wrapping label's width must not shrink to fit a height proposal:
    /// `for_size` on the horizontal measure asks GTK for the narrowest width
    /// whose wrapped text still fits that height — the label would render one
    /// word (or one character) per line inside fixed bounds.
    #[test]
    fn wrapping_label_reports_natural_width_under_height_proposal() {
        init();
        let label = Label::new(Some("Clipped"));
        label.set_wrap(true);
        label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        let subview = GtkSubView::new(label.upcast(), StretchAxis::None);

        let dims = subview.measure(ProposalSize::new(Some(80.0), Some(80.0)));

        assert!(
            dims.size.width > 30.0,
            "label collapsed to {}px under an 80px height proposal",
            dims.size.width
        );
    }

    /// The container's reported minimum must stay below its natural size when
    /// a child can compress: a `ScrolledWindow` with `NEVER` policy allocates
    /// `max(viewport, child_minimum)`, so a minimum equal to the natural width
    /// of a long label forces scrollable content wider than the window and
    /// the text is clipped instead of wrapped.
    #[test]
    fn container_minimum_stays_below_natural_with_wrapping_text() {
        init();
        let label = Label::new(Some(
            "a fairly long line of text that should wrap rather than overflow the viewport",
        ));
        label.set_wrap(true);
        label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        let container = WuiFixedContainer::new(
            Box::new(VStackLayout::default()),
            vec![(label.upcast(), StretchAxis::None)],
        );

        let (min_w, nat_w, ..) = container.measure(gtk4::Orientation::Horizontal, -1);

        assert!(nat_w > 200, "long label natural width {nat_w}");
        assert!(
            min_w < nat_w,
            "minimum {min_w} must stay below natural {nat_w}"
        );
    }
}
