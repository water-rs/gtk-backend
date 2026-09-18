//! A GTK widget implementing `WaterUI`'s `FixedContainer` layout contract.
//!
//! This is a plain `gtk4::Widget` subclass that delegates measurement and
//! placement to `WaterUI`'s Rust `Layout` engine, mirroring the Apple backend
//! behavior. Children are parented with `set_parent` and allocated directly
//! from `size_allocate` — the layout engine's rects are authoritative, so no
//! GTK layout manager or size request sits between the engine and the child.
//!
//! The engine's [`Layout::place`] also selects a [`ProposalSize`] per child.
//! When this widget is itself the placed child, the parent's
//! [`apply_placements`] delivers that packet to [`Self::note_selected_proposal`]
//! and it — not the bounds — drives the next placement pass. When no parent
//! delivered one (the widget is natively hosted: window content, scroll-view
//! content, a list row), the finite offer is reconstructed from the hosting
//! bounds at this boundary and nowhere deeper.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{Widget, glib};
use waterui_core::layout::{
    Layout, ProposalSize, Rect, Size, StretchAxis, SubView, SubviewPlacement, ViewDimensions,
    measure_layout, with_memoized_children,
};

use crate::layout::proposal::{
    install_axis_provider, install_proposal_sink, proposals_equal, query_axis, reported_axis,
    scroll_axes_for, scroll_offer,
};
use crate::layout::{
    FixedSizeSubView, GtkSubView, LayoutMeasureMemo, apply_placements, layout_measure_key,
};
use crate::util::store_watcher_guards;

fn layout_debug_enabled() -> bool {
    std::env::var_os("WATERUI_GTK_LAYOUT_DEBUG").is_some()
}

fn trace_layout_placements(
    operation: &str,
    width: i32,
    height: i32,
    child_count: usize,
    placements: &[SubviewPlacement],
) {
    tracing::debug!(
        target: "waterui::gtk::layout",
        operation,
        width,
        height,
        child_count,
        placement_count = placements.len(),
        "Laid out GTK fixed container"
    );
    for (index, placement) in placements.iter().enumerate().take(8) {
        tracing::debug!(
            target: "waterui::gtk::layout",
            operation,
            index,
            x = placement.frame.x(),
            y = placement.frame.y(),
            width = placement.frame.width(),
            height = placement.frame.height(),
            proposal_width = ?placement.proposal.width,
            proposal_height = ?placement.proposal.height,
            "GTK fixed-container child placement"
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
        /// The proposal the parent layout selected for this container —
        /// `None` while no Rust-placed parent has delivered one (natively
        /// hosted roots reconstruct their offer from the bounds instead).
        pub selected_proposal: Cell<Option<ProposalSize>>,
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
            let mut memo = MeasureMemo::default();
            let (min_w, min_h, w, h) = self.measure_inner(orientation, for_size, &mut memo);
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

    /// One pass's measure answers: `(widget, orientation, for_size)` →
    /// `(min_width, min_height, natural_width, natural_height)` for the GTK
    /// minimum floor, plus the shared container-layout memo the `GtkSubView`
    /// wrappers carry down the natural pass so each nested container is
    /// probed once per distinct proposal.
    #[derive(Default)]
    struct MeasureMemo {
        gtk: std::collections::HashMap<(usize, i32, i32), (i32, i32, i32, i32)>,
        layout: LayoutMeasureMemo,
    }

    const fn orientation_key(orientation: gtk4::Orientation) -> i32 {
        match orientation {
            gtk4::Orientation::Horizontal => 0,
            gtk4::Orientation::Vertical => 1,
            _ => 2,
        }
    }

    impl WuiFixedContainer {
        /// The measure computation behind `measure`, memoized per
        /// `(widget, orientation, for_size)` within a single negotiation.
        ///
        /// The minimum floor asks every child for *both* orientations, so
        /// asking a nested container through GTK's `Widget::measure` makes
        /// each level re-run the whole subtree's min computation — O(2^depth)
        /// leaf measures per negotiation, which stalls the main loop for
        /// tens of seconds on a picker-deep tree and keeps the window from
        /// ever reaching its first paint. Container children are therefore
        /// answered through this memoized path instead of GTK dispatch; a
        /// node is queried under at most three `(orientation, for_size)`
        /// variants per pass, so a negotiation stays linear in the tree.
        fn measure_inner(
            &self,
            orientation: gtk4::Orientation,
            for_size: i32,
            memo: &mut MeasureMemo,
        ) -> (i32, i32, i32, i32) {
            let key = (
                self.obj().upcast_ref::<Widget>().as_ptr() as usize,
                orientation_key(orientation),
                for_size,
            );
            if let Some(&hit) = memo.gtk.get(&key) {
                return hit;
            }
            let result = self.measure_uncached(orientation, for_size, memo);
            memo.gtk.insert(key, result);
            result
        }

        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
        )]
        fn measure_uncached(
            &self,
            orientation: gtk4::Orientation,
            for_size: i32,
            memo: &mut MeasureMemo,
        ) -> (i32, i32, i32, i32) {
            let layout_borrow = self.layout.borrow();
            let Some(layout) = layout_borrow.as_ref() else {
                panic!("WuiFixedContainer: missing layout (internal error)");
            };

            let children = self.children.borrow();

            // No empty-children shortcut: a leaf-shaped layout such as
            // `SpacerLayout` still owes an honest answer (its minimum length),
            // and `measure_layout` handles an empty child set correctly.
            let subviews: Vec<GtkSubView> = children
                .iter()
                .map(|(w, axis)| GtkSubView::with_memo(w.clone(), *axis, Some(memo.layout.clone())))
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
                        gtk4::Orientation::Horizontal => (
                            child_min(widget, gtk4::Orientation::Horizontal, for_size, memo),
                            child_min(widget, gtk4::Orientation::Vertical, -1, memo),
                        ),
                        gtk4::Orientation::Vertical => (
                            child_min(widget, gtk4::Orientation::Horizontal, -1, memo),
                            child_min(widget, gtk4::Orientation::Vertical, for_size, memo),
                        ),
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
            (min_w, min_h, w, h)
        }
    }

    /// The GTK minimum `widget` reports for `orientation` under `for_size`.
    /// A `WuiFixedContainer` child answers through the memoized inner pass so
    /// its subtree is measured once per pass instead of once per calling
    /// orientation; native leaves go through `Widget::measure`, the honest
    /// channel for GTK widgets.
    fn child_min(
        widget: &Widget,
        orientation: gtk4::Orientation,
        for_size: i32,
        memo: &mut MeasureMemo,
    ) -> i32 {
        widget
            .downcast_ref::<super::WuiFixedContainer>()
            .map_or_else(
                || widget.measure(orientation, for_size).0,
                |container| {
                    let (min_w, min_h, ..) =
                        container.imp().measure_inner(orientation, for_size, memo);
                    match orientation {
                        gtk4::Orientation::Horizontal => min_w,
                        gtk4::Orientation::Vertical => min_h,
                        _ => panic!("WuiFixedContainer: unexpected orientation {orientation:?}"),
                    }
                },
            )
    }
}

glib::wrapper! {
    pub struct WuiFixedContainer(ObjectSubclass<imp::WuiFixedContainer>)
        @extends Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl WuiFixedContainer {
    /// Measures this container's layout under `proposal`, sharing `memo`
    /// across the negotiation.
    ///
    /// `measure_layout` memoizes children only for the duration of its own
    /// call, so without a shared map every proposal a parent probes re-runs
    /// this container's whole subtree — and stack distribution probes at the
    /// unspecified, minimum, ideal, and allocated mains plus place and guide
    /// resolution, which multiplies with each nesting level. One map shared
    /// by every `GtkSubView` in the pass collapses the negotiation to one
    /// measure per `(container, proposal)` pair.
    pub(crate) fn layout_measure_shared(
        &self,
        proposal: ProposalSize,
        memo: Option<&LayoutMeasureMemo>,
    ) -> ViewDimensions {
        let key = memo.map(|_| layout_measure_key(self.upcast_ref(), proposal));
        if let (Some(memo), Some(key)) = (memo, key)
            && let Some(dimensions) = memo.borrow().get(&key)
        {
            return dimensions.clone();
        }

        let imp = self.imp();
        let layout_borrow = imp.layout.borrow();
        let Some(layout) = layout_borrow.as_ref() else {
            panic!("WuiFixedContainer: missing layout (internal error)");
        };

        let children = imp.children.borrow();
        // No empty-children shortcut: `measure_layout` answers for an empty
        // child set, and leaf-shaped layouts (`SpacerLayout`) still owe their
        // minimum length.
        let subviews: Vec<GtkSubView> = children
            .iter()
            .map(|(w, axis)| GtkSubView::with_memo(w.clone(), *axis, memo.cloned()))
            .collect();
        let refs: Vec<&dyn SubView> = subviews.iter().map(|v| v as &dyn SubView).collect();

        let dimensions = measure_layout(layout.as_ref(), proposal, &refs);
        if let (Some(memo), Some(key)) = (memo, key) {
            memo.borrow_mut().insert(key, dimensions.clone());
        }
        dimensions
    }

    /// Runs the layout engine at `width`×`height` and allocates each child at
    /// its resulting placement.
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
            .map(|(w, axis)| {
                GtkSubView::with_memo(w.clone(), *axis, Some(LayoutMeasureMemo::default()))
            })
            .collect();
        let refs: Vec<&dyn SubView> = subviews.iter().map(|v| v as &dyn SubView).collect();

        let bounds = Rect::from_size(Size {
            width: (width.max(0)) as f32,
            height: (height.max(0)) as f32,
        });

        // The placement proposal is the packet the parent layout selected for
        // this container; a natively hosted root — window content, scroll-view
        // content, a list row — has no parent delivery, so the offer is
        // reconstructed at this boundary and nowhere deeper. Scroll-hosted
        // content leaves each scrolling axis unspecified instead of playing
        // its (potentially natural-sized) bounds back as the negotiated
        // proposal. Measurement probes never write `selected_proposal`, so
        // the packet in force survives however many passes the engine ran.
        let proposal = imp.selected_proposal.get().unwrap_or_else(|| {
            if let Some((scrolls_h, scrolls_v)) = scroll_axes_for(self.upcast_ref()) {
                scroll_offer(scrolls_h, scrolls_v, bounds.width(), bounds.height())
            } else {
                ProposalSize::new(Some(bounds.width()), Some(bounds.height()))
            }
        });

        let placements = with_memoized_children(&refs, |refs| {
            let _ = layout.size_that_fits(proposal, refs);
            layout.place(bounds, proposal, refs)
        });
        if layout_debug_enabled() {
            trace_layout_placements(operation, width, height, children.len(), &placements);
        }
        apply_placements(&placements, &children);
    }

    /// Stores the proposal the parent layout selected for this container.
    ///
    /// Called by [`deliver_proposal`](crate::layout::proposal::deliver_proposal)
    /// through the widget's installed sink. A changed packet queues allocation:
    /// GTK may skip a child `size_allocate` whose frame did not move, and the
    /// queue flag is what forces the relayout when only the proposal changed.
    fn note_selected_proposal(&self, proposal: ProposalSize) {
        let imp = self.imp();
        let previous = imp.selected_proposal.replace(Some(proposal));
        if previous.is_none_or(|old| !proposals_equal(old, proposal)) {
            self.queue_allocate();
        }
    }

    /// The axis this container claims, re-derived from live child state.
    ///
    /// `Layout::stretch_axis` is answered with each child's *current* axis —
    /// queried through the widget markers, not the snapshot taken when the
    /// children were rendered — so a dynamic child whose content swapped
    /// axes propagates the new claim. A leaf-shaped container (a `Spacer`'s
    /// empty `SpacerLayout`) has no children to derive from and falls back
    /// to the axis its view declared at render time.
    fn live_stretch_axis(&self) -> StretchAxis {
        let imp = self.imp();
        let children = imp.children.borrow();
        if children.is_empty()
            && let Some(axis) = reported_axis(self.upcast_ref())
        {
            return axis;
        }
        let axes: Vec<StretchAxis> = children
            .iter()
            .map(|(child, recorded)| query_axis(child).unwrap_or(*recorded))
            .collect();
        let layout_borrow = imp.layout.borrow();
        layout_borrow
            .as_ref()
            .expect("WuiFixedContainer: missing layout (internal error)")
            .stretch_axis(&axes)
    }

    /// Creates a container that lays `children` out with `layout`.
    ///
    /// # Panics
    ///
    /// Panics if the installed proposal sink ever runs on a widget that is
    /// not a `WuiFixedContainer`; the sink is attached to the created
    /// container and only proposals delivered to it reach the sink.
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

        // Reactive inputs inside the layout (a spacing binding, a watched
        // member) invalidate through this callback; the returned guards keep
        // the subscriptions alive for the widget's lifetime.
        let weak = obj.downgrade();
        let guards = layout.watch_invalidation(Rc::new(move || {
            if let Some(obj) = weak.upgrade() {
                obj.queue_resize();
            }
        }));
        store_watcher_guards(&obj, guards);

        *imp.layout.borrow_mut() = Some(layout);
        *imp.children.borrow_mut() = children;

        for (child, _) in imp.children.borrow().iter() {
            child.set_parent(&obj);
        }

        // The widget-side layout contract: a delivered selected proposal is
        // this container's next placement packet, and its stretch axis is
        // derived live from whatever children it currently holds.
        install_proposal_sink(obj.upcast_ref(), |w, proposal| {
            let container = w
                .downcast_ref::<Self>()
                .expect("WuiFixedContainer proposal sink on wrong widget");
            container.note_selected_proposal(proposal);
        });
        install_axis_provider(obj.upcast_ref(), |w| {
            let container = w
                .downcast_ref::<Self>()
                .expect("WuiFixedContainer axis provider on wrong widget");
            container.live_stretch_axis()
        });

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
    use waterui_core::layout::{
        PlacedSubview, ProposalSize, StretchAxis, SubView, VerticalAlignment,
    };
    use waterui_core::{AnyView, Environment, Native};
    use waterui_layout::stack::VStackLayout;

    use super::*;
    use crate::component::GtkComponent;
    use crate::layout::proposal::{
        deliver_proposal, forward_box_content_slot, forward_proposals_to_child, query_axis,
        query_priority, reoffer_proposal, row_offer, set_layout_priority, set_scroll_axes,
        transparent_to_content,
    };
    use crate::layout::subview::GtkSubView;
    use crate::renderer::GtkRenderer;

    fn logical_extent(value: i32) -> f32 {
        num_traits::cast(value).expect("GTK integer geometry fits f32")
    }

    fn init() {
        gtk4::init().expect("GTK tests need a display; run them under xvfb-run");
    }

    /// A layout that places every child at a fixed frame under the proposal
    /// the test selects, and records each proposal `place` was invoked with
    /// and each proposal `size_that_fits` was probed with. The parent's
    /// instance picks the packet delivered to a child; a nested container's
    /// instance observes what arrived.
    #[derive(Debug)]
    struct ProbeLayout {
        frame: Rect,
        selected: Rc<Cell<ProposalSize>>,
        placed: Rc<RefCell<Vec<ProposalSize>>>,
        probed: Rc<RefCell<Vec<ProposalSize>>>,
        vertical_guide: Option<(VerticalAlignment, f32)>,
    }

    impl ProbeLayout {
        fn new(
            frame_size: f32,
            selected: Rc<Cell<ProposalSize>>,
        ) -> (Self, Rc<RefCell<Vec<ProposalSize>>>) {
            let placed = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    frame: Rect::from_size(Size::new(frame_size, frame_size)),
                    selected,
                    placed: Rc::clone(&placed),
                    probed: Rc::new(RefCell::new(Vec::new())),
                    vertical_guide: None,
                },
                placed,
            )
        }

        /// The proposals `size_that_fits` was probed with, in order.
        fn probes(&self) -> Rc<RefCell<Vec<ProposalSize>>> {
            Rc::clone(&self.probed)
        }

        /// Exposes `alignment` as an explicit vertical guide resolving to `value`.
        fn with_vertical_guide(mut self, alignment: VerticalAlignment, value: f32) -> Self {
            self.vertical_guide = Some((alignment, value));
            self
        }
    }

    impl Layout for ProbeLayout {
        fn size_that_fits(&self, proposal: ProposalSize, _children: &[&dyn SubView]) -> Size {
            self.probed.borrow_mut().push(proposal);
            Size::new(self.frame.width(), self.frame.height())
        }

        fn place(
            &self,
            _bounds: Rect,
            proposal: ProposalSize,
            children: &[&dyn SubView],
        ) -> Vec<SubviewPlacement> {
            self.placed.borrow_mut().push(proposal);
            children
                .iter()
                .map(|_| SubviewPlacement::new(self.frame, self.selected.get()))
                .collect()
        }

        fn explicit_vertical_alignments(&self) -> Vec<VerticalAlignment> {
            self.vertical_guide
                .map(|(alignment, _)| vec![alignment])
                .unwrap_or_default()
        }

        fn explicit_vertical(
            &self,
            alignment: VerticalAlignment,
            _bounds: Rect,
            _children: &[PlacedSubview<'_>],
        ) -> Option<f32> {
            self.vertical_guide
                .and_then(|(guide, value)| (guide == alignment).then_some(value))
        }
    }

    /// A labeled child inside a container whose placement the test controls.
    fn label_child() -> (Widget, StretchAxis) {
        (Label::new(Some("child")).upcast(), StretchAxis::None)
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

    /// `Layout::place` picks a proposal per child; `apply_placements` must
    /// deliver that packet to the child before allocating it, so a nested
    /// container lays its own children out under the same negotiated packet.
    #[test]
    fn selected_proposal_reaches_nested_container() {
        init();
        let chosen = ProposalSize::new(Some(64.0), Some(48.0));
        let inner_selected = Rc::new(Cell::new(ProposalSize::UNSPECIFIED));
        let (inner_layout, inner_placed) = ProbeLayout::new(10.0, inner_selected);
        let inner = WuiFixedContainer::new(Box::new(inner_layout), vec![label_child()]);

        let parent_selected = Rc::new(Cell::new(chosen));
        let (parent_layout, _) = ProbeLayout::new(40.0, parent_selected);
        let parent = WuiFixedContainer::new(
            Box::new(parent_layout),
            vec![(inner.clone().upcast(), StretchAxis::None)],
        );

        parent.allocate(200, 200, -1, None);

        assert_eq!(
            inner.imp().selected_proposal.get(),
            Some(chosen),
            "container did not retain the parent's selected proposal"
        );
        assert_eq!(
            inner_placed.borrow().last().copied(),
            Some(chosen),
            "nested placement ran under a different proposal than delivered"
        );
    }

    /// A layout-transparent wrapper — the `GtkBox` metadata realization —
    /// must forward a delivered selected proposal to the content it hosts.
    #[test]
    fn transparent_wrapper_forwards_selected_proposal() {
        init();
        let chosen = ProposalSize::new(Some(96.0), Some(24.0));
        let (inner_layout, inner_placed) =
            ProbeLayout::new(10.0, Rc::new(Cell::new(ProposalSize::UNSPECIFIED)));
        let inner = WuiFixedContainer::new(Box::new(inner_layout), vec![label_child()]);

        let wrapper = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        wrapper.append(&inner);
        forward_proposals_to_child(wrapper.upcast_ref(), inner.upcast_ref());

        let parent_selected = Rc::new(Cell::new(chosen));
        let (parent_layout, _) = ProbeLayout::new(40.0, parent_selected);
        let parent = WuiFixedContainer::new(
            Box::new(parent_layout),
            vec![(wrapper.upcast(), StretchAxis::None)],
        );

        parent.allocate(200, 200, -1, None);

        assert_eq!(inner.imp().selected_proposal.get(), Some(chosen));
        assert_eq!(inner_placed.borrow().last().copied(), Some(chosen));
    }

    /// Minimum, ideal, and maximum probes are queries, not placements: none
    /// may overwrite the selected proposal a parent delivered — including an
    /// unbounded maximum, which must survive as `f32::INFINITY` and never
    /// reach GTK as `i32::MAX`.
    #[test]
    fn measurement_probes_do_not_overwrite_selected_proposal() {
        init();
        let chosen = ProposalSize::new(Some(64.0), Some(48.0));
        let (inner_layout, _) = ProbeLayout::new(10.0, Rc::new(Cell::new(chosen)));
        let inner = WuiFixedContainer::new(Box::new(inner_layout), vec![label_child()]);

        let parent_selected = Rc::new(Cell::new(chosen));
        let (parent_layout, _) = ProbeLayout::new(40.0, parent_selected);
        let parent = WuiFixedContainer::new(
            Box::new(parent_layout),
            vec![(inner.clone().upcast(), StretchAxis::None)],
        );
        parent.allocate(200, 200, -1, None);
        assert_eq!(inner.imp().selected_proposal.get(), Some(chosen));

        for probe in [
            ProposalSize::ZERO,
            ProposalSize::UNSPECIFIED,
            ProposalSize::INFINITY,
        ] {
            let _ = inner.layout_measure_shared(probe, None);
            assert_eq!(
                inner.imp().selected_proposal.get(),
                Some(chosen),
                "probe {probe:?} clobbered the selected proposal"
            );
        }

        // The subview path keeps the unbounded query unbounded: a maximum
        // probe reports the natural size, not a 2-billion-pixel answer.
        let subview = GtkSubView::new(Label::new(Some("abc")).upcast(), StretchAxis::None);
        let dims = subview.measure(ProposalSize::INFINITY);
        assert!(dims.size.width.is_finite() && dims.size.height.is_finite());
        assert!(dims.size.width > 0.0 && dims.size.height > 0.0);
    }

    /// GTK skips a `size_allocate` whose allocation is unchanged, so a
    /// changed proposal at equal bounds would silently lose the relayout
    /// unless the delivery queues it. The parent's new `place` delivers the
    /// new packet; the child's placement must rerun under it.
    #[test]
    fn equal_bounds_changed_proposal_still_relayouts() {
        init();
        let inner_placed = Rc::new(RefCell::new(Vec::new()));
        let inner_layout = ProbeLayout {
            frame: Rect::from_size(Size::new(10.0, 10.0)),
            selected: Rc::new(Cell::new(ProposalSize::UNSPECIFIED)),
            placed: Rc::clone(&inner_placed),
            probed: Rc::new(RefCell::new(Vec::new())),
            vertical_guide: None,
        };
        let inner = WuiFixedContainer::new(Box::new(inner_layout), vec![label_child()]);

        let first = ProposalSize::new(Some(64.0), Some(48.0));
        let second = ProposalSize::new(Some(120.0), Some(48.0));
        let parent_selected = Rc::new(Cell::new(first));
        // The frame is identical across both passes — only the proposal moves.
        let (parent_layout, _) = ProbeLayout::new(40.0, Rc::clone(&parent_selected));
        let parent = WuiFixedContainer::new(
            Box::new(parent_layout),
            vec![(inner.clone().upcast(), StretchAxis::None)],
        );

        parent.allocate(200, 200, -1, None);
        assert_eq!(inner_placed.borrow().last().copied(), Some(first));

        // Unrelated probes between placements must not disturb the packet.
        let _ = inner.layout_measure_shared(ProposalSize::ZERO, None);
        let _ = inner.layout_measure_shared(ProposalSize::INFINITY, None);
        assert_eq!(inner.imp().selected_proposal.get(), Some(first));

        parent_selected.set(second);
        parent.queue_allocate();
        parent.allocate(200, 200, -1, None);

        assert_eq!(inner.imp().selected_proposal.get(), Some(second));
        assert_eq!(
            inner_placed.borrow().last().copied(),
            Some(second),
            "equal bounds with a changed proposal did not relayout the child"
        );
    }

    /// `Spacer` renders through `SpacerLayout`: the container reports the
    /// minimum length even with zero children, and the widget carries the
    /// lowest default layout priority so a stack squeezes it first.
    #[test]
    fn spacer_reports_min_length_and_lowest_priority() {
        init();
        let env = Environment::new();
        let mut renderer = GtkRenderer::new();
        let widget =
            Native::new(waterui_layout::spacer::Spacer::new(10.0)).render(&env, &mut renderer);

        let (min_w, nat_w, ..) = widget.measure(gtk4::Orientation::Horizontal, -1);
        assert_eq!((min_w, nat_w), (10, 10));

        let subview = GtkSubView::new(widget, StretchAxis::MainAxis);
        assert_eq!(subview.priority(), i32::MIN);
    }

    /// The dynamic host reads its layout-facing answers through the live
    /// child: axis and priority follow whatever content it currently holds,
    /// and a proposal delivered before a swap is replayed to the replacement.
    #[test]
    fn dynamic_host_reports_live_child_traits_and_replays_proposal() {
        init();
        let env = Environment::new();
        let mut renderer = GtkRenderer::new();
        let (handler, dynamic) = waterui_core::dynamic::Dynamic::new();
        let host = Native::new(dynamic).render(&env, &mut renderer);

        // Deliver before any content exists: the packet is retained on the host.
        let chosen = ProposalSize::new(Some(64.0), Some(48.0));
        deliver_proposal(&host, chosen);

        handler.set(waterui_layout::spacer::Spacer::new(8.0));
        let context = glib::MainContext::default();
        while context.iteration(false) {}

        let child = host.first_child().expect("dynamic content not rendered");
        assert_eq!(
            query_axis(&host),
            Some(StretchAxis::MainAxis),
            "dynamic host did not report the spacer's main-axis stretch"
        );
        assert_eq!(query_priority(&host), Some(i32::MIN));
        assert_eq!(
            child
                .downcast_ref::<WuiFixedContainer>()
                .expect("spacer content is not a layout container")
                .imp()
                .selected_proposal
                .get(),
            Some(chosen),
            "replacement child did not inherit the retained proposal"
        );

        // A non-stretching child flips every live answer back.
        handler.set(waterui_core::Str::from("text"));
        while context.iteration(false) {}
        assert_eq!(query_axis(&host), Some(StretchAxis::None));
        assert_eq!(query_priority(&host), Some(0));
    }

    /// The scroll owner's offer: `None` down every scrolling axis, the
    /// finite viewport extent across each non-scrolling one.
    #[test]
    fn scroll_offer_leaves_scrolling_axes_unspecified() {
        let offer = crate::layout::proposal::scroll_offer(false, true, 320.0, 240.0);
        assert_eq!(offer.width, Some(320.0));
        assert_eq!(offer.height, None);

        let both = crate::layout::proposal::scroll_offer(true, true, 320.0, 240.0);
        assert_eq!((both.width, both.height), (None, None));
    }

    /// A container hosted inside scroll content — through a transparent
    /// wrapper — must reconstruct the owner's offer, not its own bounds.
    #[test]
    fn scroll_axes_marker_reaches_wrapped_container() {
        init();
        let inner = WuiFixedContainer::new(Box::new(VStackLayout::default()), vec![label_child()]);
        let wrapper = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        wrapper.append(&inner);
        crate::layout::proposal::set_scroll_axes(wrapper.upcast_ref(), false, true);

        assert_eq!(
            crate::layout::proposal::scroll_axes_for(inner.upcast_ref()),
            Some((false, true)),
            "scroll annotation did not reach through the transparent wrapper"
        );
    }

    /// A transparent wrapper must deliver a measurement probe to its content
    /// untouched: `None` stays `None`, `f32::INFINITY` stays unbounded, and
    /// the content's explicit guides come back offset by its margins —
    /// GTK's integer `measure` round trip drops all three.
    #[test]
    fn transparent_wrapper_preserves_raw_probe_and_guides() {
        init();
        let (inner_layout, _) =
            ProbeLayout::new(10.0, Rc::new(Cell::new(ProposalSize::UNSPECIFIED)));
        let inner_layout = inner_layout.with_vertical_guide(VerticalAlignment::Center, 12.0);
        let probed = inner_layout.probes();
        let inner = WuiFixedContainer::new(Box::new(inner_layout), vec![label_child()]);
        inner.set_margin_top(4);
        inner.set_margin_bottom(4);

        let wrapper = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        wrapper.append(&inner);
        transparent_to_content(wrapper.upcast_ref(), inner.upcast_ref());

        let subview = GtkSubView::new(wrapper.upcast(), StretchAxis::None);
        let dimensions = subview.measure(ProposalSize::new(Some(f32::INFINITY), Some(64.0)));

        assert_eq!(
            probed.borrow().last().copied(),
            Some(ProposalSize::new(Some(f32::INFINITY), Some(56.0))),
            "the raw probe did not reach the wrapped container"
        );
        assert_eq!(
            dimensions.explicit_vertical(VerticalAlignment::Center),
            Some(16.0),
            "the explicit guide did not survive the transparent hop"
        );
    }

    /// A wrapped `Spacer` keeps answering through the wrapper: the live axis
    /// and priority read through to the content, while an explicit priority
    /// recorded on the wrapper itself still wins.
    #[test]
    fn transparent_wrapper_forwards_axis_and_priority() {
        init();
        let env = Environment::new();
        let mut renderer = GtkRenderer::new();
        let (content, axis) = renderer.render_any_with_axis(
            AnyView::new(Native::new(waterui_layout::spacer::Spacer::new(8.0))),
            &env,
        );
        assert_eq!(axis, StretchAxis::MainAxis);

        let wrapper = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        wrapper.append(&content);
        transparent_to_content(wrapper.upcast_ref(), &content);

        assert_eq!(
            query_axis(wrapper.upcast_ref()),
            Some(StretchAxis::MainAxis),
            "the wrapper did not forward the spacer's stretch axis"
        );
        assert_eq!(
            query_priority(wrapper.upcast_ref()),
            Some(i32::MIN),
            "the wrapper did not forward the spacer's default priority"
        );

        // An explicit override on the wrapper beats the forwarded answer.
        set_layout_priority(wrapper.upcast_ref(), 7);
        assert_eq!(query_priority(wrapper.upcast_ref()), Some(7));
    }

    /// The delivered selected proposal must undergo the same margin
    /// transformation measurement applies: a margined child retains and
    /// places under the content-box packet, not the margin-box one.
    #[test]
    fn delivered_proposal_shrinks_by_child_margins() {
        init();
        let chosen = ProposalSize::new(Some(100.0), Some(80.0));
        let (inner_layout, inner_placed) =
            ProbeLayout::new(10.0, Rc::new(Cell::new(ProposalSize::UNSPECIFIED)));
        let inner = WuiFixedContainer::new(Box::new(inner_layout), vec![label_child()]);
        inner.set_margin_start(10);
        inner.set_margin_end(10);
        inner.set_margin_top(4);
        inner.set_margin_bottom(4);

        let parent_selected = Rc::new(Cell::new(chosen));
        let (parent_layout, _) = ProbeLayout::new(40.0, parent_selected);
        let parent = WuiFixedContainer::new(
            Box::new(parent_layout),
            vec![(inner.clone().upcast(), StretchAxis::None)],
        );

        parent.allocate(200, 200, -1, None);

        assert_eq!(
            inner.width(),
            20,
            "allocation removed horizontal margins twice"
        );
        assert_eq!(
            inner.height(),
            32,
            "allocation removed vertical margins twice"
        );
        let origin = inner
            .compute_point(&parent, &gtk4::graphene::Point::zero())
            .expect("child and parent share a widget tree");
        assert_eq!((origin.x(), origin.y()), (10.0, 4.0));

        let expected = ProposalSize::new(Some(80.0), Some(72.0));
        assert_eq!(
            inner.imp().selected_proposal.get(),
            Some(expected),
            "the retained packet kept margin-box geometry"
        );
        assert_eq!(
            inner_placed.borrow().last().copied(),
            Some(expected),
            "place ran under a different proposal than measurement"
        );
    }

    /// The list row's chrome is real siblings, not transparency: the
    /// forwarded packet must shrink by the visible siblings' natural extents
    /// and the box's spacing — the slot the content is actually allocated.
    #[test]
    fn row_chrome_negotiates_the_content_slot() {
        init();
        let (inner_layout, _) =
            ProbeLayout::new(10.0, Rc::new(Cell::new(ProposalSize::UNSPECIFIED)));
        let content = WuiFixedContainer::new(Box::new(inner_layout), vec![label_child()]);
        content.set_hexpand(true);

        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        row.set_margin_start(12);
        row.set_margin_end(12);
        row.set_margin_top(8);
        row.set_margin_bottom(8);
        row.append(&content);
        let button = gtk4::Button::from_icon_name("edit-delete-symbolic");
        row.append(&button);
        forward_box_content_slot(row.upcast_ref(), content.upcast_ref());

        deliver_proposal(
            row.upcast_ref(),
            row_offer(gtk4::Orientation::Vertical, 400.0),
        );

        let (_, button_natural, ..) = button.measure(gtk4::Orientation::Horizontal, -1);
        let expected = ProposalSize::new(
            Some(400.0 - 24.0 - 8.0 - logical_extent(button_natural.max(0))),
            None,
        );
        assert_eq!(
            content.imp().selected_proposal.get(),
            Some(expected),
            "the row forwarded its whole margin box, chrome included"
        );

        // Chrome that hides frees the slot again — the row re-runs the
        // negotiation against the packet already in force.
        button.set_visible(false);
        reoffer_proposal(row.upcast_ref());
        assert_eq!(
            content.imp().selected_proposal.get(),
            Some(ProposalSize::new(Some(400.0 - 24.0), None)),
            "a hidden sibling still ate into the content's slot"
        );
    }

    /// A row's scroll marker must shape the FIRST allocation pass: the
    /// layout container inside reconstructs the owner's raw scroll axis —
    /// `None` down the scrolling axis — instead of playing its own finite
    /// bounds back as the negotiated proposal.
    #[test]
    fn scroll_marker_shapes_first_allocation_pass() {
        init();
        let (inner_layout, inner_placed) =
            ProbeLayout::new(10.0, Rc::new(Cell::new(ProposalSize::UNSPECIFIED)));
        let inner = WuiFixedContainer::new(Box::new(inner_layout), vec![label_child()]);

        let row = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        row.append(&inner);
        set_scroll_axes(row.upcast_ref(), false, true);

        row.allocate(200, 50, -1, None);

        let last = inner_placed.borrow().last().copied();
        assert_eq!(
            last.map(|proposal| proposal.width),
            Some(Some(200.0)),
            "first-pass offer did not carry the cross-axis extent"
        );
        assert_eq!(
            last.map(|proposal| proposal.height),
            Some(None),
            "the scrolling axis arrived bounded on the first pass"
        );
    }

    /// GTK already includes native-leaf margins in sizes and baselines;
    /// the raw bridge must not count or subtract them a second time.
    #[allow(
        clippy::float_cmp,
        reason = "every compared value is an exact integer-measure conversion or a min/max of such values, so equality is deterministic"
    )]
    #[test]
    fn leaf_measurement_accounts_for_margins() {
        init();
        let label = Label::new(Some("abc"));
        label.set_margin_start(10);
        label.set_margin_end(10);
        label.set_margin_top(4);
        label.set_margin_bottom(4);
        let subview = GtkSubView::new(label.clone().upcast(), StretchAxis::None);

        let (min_w, nat_w, ..) = label.measure(gtk4::Orientation::Horizontal, -1);
        let (min_h, nat_h, ..) = label.measure(gtk4::Orientation::Vertical, -1);

        let generous = subview.measure(ProposalSize::new(Some(500.0), Some(500.0)));
        assert_eq!(generous.size.width, logical_extent(nat_w));
        assert_eq!(generous.size.height, logical_extent(nat_h));

        // GTK minimum sizes already include margins even for tight offers.
        let tight = subview.measure(ProposalSize::new(Some(30.0), Some(10.0)));
        let expected_w = 30.0_f32
            .min(logical_extent(nat_w))
            .max(logical_extent(min_w.min(nat_w)));
        let expected_h = 10.0_f32
            .min(logical_extent(nat_h))
            .max(logical_extent(min_h.min(nat_h)));
        assert_eq!(tight.size.width, expected_w);
        assert_eq!(tight.size.height, expected_h);
    }

    /// The dynamic host's raw probe reads through to the live child: after a
    /// swap the host measures as the new content, not as the empty box it
    /// was at render time.
    #[test]
    fn dynamic_host_measures_the_live_child() {
        init();
        let env = Environment::new();
        let mut renderer = GtkRenderer::new();
        let (handler, dynamic) = waterui_core::dynamic::Dynamic::new();
        let host = Native::new(dynamic).render(&env, &mut renderer);

        let subview = GtkSubView::new(host, StretchAxis::None);
        handler.set(waterui_layout::spacer::Spacer::new(8.0));
        let context = glib::MainContext::default();
        while context.iteration(false) {}

        let dimensions = subview.measure(ProposalSize::UNSPECIFIED);
        assert_eq!(subview.stretch_axis(), StretchAxis::MainAxis);
        assert_eq!(
            (dimensions.size.width, dimensions.size.height),
            (8.0, 8.0),
            "the host measured itself instead of the live child"
        );
    }
}