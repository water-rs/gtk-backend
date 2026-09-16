//! Widget-local selected-proposal transport and live layout traits.
//!
//! `Layout::place` returns [`SubviewPlacement`] values pairing each child's
//! frame with the [`ProposalSize`] the layout selected for it. GTK has no
//! slot for that packet, so it rides on the widgets themselves: parents
//! deliver through [`deliver_proposal`], and each widget's [`WidgetLayout`]
//! marker decides what a delivery does — a `WuiFixedContainer` stores it as
//! its selected proposal, a transparent wrapper forwards it to the content
//! it hosts, a scroll owner reshapes it per axis, and a plain leaf carries
//! no sink and ignores the delivery entirely.
//!
//! A packet always travels in the receiver's *margin-box* terms — the terms
//! a `SubView` reports sizes in — and each widget retains its *content-box*
//! packet, the margins removed the same way `measure` removes them from a
//! probe, so placement and measurement negotiate identical geometry.
//!
//! The same marker answers the two questions [`SubView`](waterui_core::layout::SubView)
//! asks of a rendered widget: its [`StretchAxis`] — the render-time
//! declaration, unless a live provider installed by a dynamic or layout host
//! answers first — and its layout priority — an explicit
//! `Metadata<LayoutPriority>` override or a host-defaulted value like
//! `Spacer`'s, then a live provider's answer. A third provider carries the
//! raw measurement probe across transparent hosts, where GTK's integer
//! `measure` would drop the `None`/non-finite extents and explicit guides.
//!
//! Marker state lives in per-widget qdata: no globals, and every query is a
//! plain `Option` read on the GTK main thread.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Orientation, Widget};
use waterui_core::layout::{ProposalSize, StretchAxis, ViewDimensions};

use crate::layout::subview::measure_view;

const WIDGET_LAYOUT_KEY: &str = "waterui-widget-layout";

/// The live stretch-axis answer a host installs on a widget.
type AxisProvider = Rc<dyn Fn(&Widget) -> StretchAxis>;
/// The live layout-priority answer a host installs on a widget.
type PriorityProvider = Rc<dyn Fn(&Widget) -> i32>;
/// The raw measurement channel a layout-transparent host installs on a
/// widget — see `measure_provider` on [`WidgetLayout`] for the contract.
type MeasureProvider = Rc<dyn Fn(&Widget, ProposalSize, StretchAxis) -> Option<ViewDimensions>>;
/// What a delivered selected proposal does on a widget.
type ProposalSink = Rc<dyn Fn(&Widget, ProposalSize)>;

/// What a widget reports to `SubView` queries and does with a delivered
/// selected proposal. Stored as widget qdata; every field starts empty and
/// only the parts a host installs ever get used.
#[derive(Default)]
struct WidgetLayout {
    /// Live stretch-axis answer for hosts whose content answer can change
    /// after render — dynamic content, layout containers. Consulted before
    /// `reported_axis`; absent, the recorded declaration stands.
    axis_provider: RefCell<Option<AxisProvider>>,
    /// The axis the rendered view resolved to at render time, recorded by
    /// `GtkRenderer::render_any_with_axis`.
    reported_axis: Cell<Option<StretchAxis>>,
    /// Live priority for hosts that forward their current child's answer.
    /// Consulted only when `priority` carries no explicit value.
    priority_provider: RefCell<Option<PriorityProvider>>,
    /// The widget's layout priority: `Spacer`'s low default or the
    /// `Metadata<LayoutPriority>` override — the last write wins.
    priority: Cell<Option<i32>>,
    /// The widget's raw measurement channel: a host that is layout-identical
    /// to content it holds answers a probe by forwarding it there, so the
    /// proposal's `None`/non-finite extents and the answer's explicit guides
    /// survive — neither fits GTK's integer `measure` round trip. The third
    /// argument is the widget's already-resolved stretch axis (a live answer
    /// or the caller's inherited fallback), handed down as the content's own
    /// claim so it reaches the leaf that means it. A `None` return falls
    /// back to the GTK measure.
    measure_provider: RefCell<Option<MeasureProvider>>,
    /// What a delivered selected proposal means to this widget.
    proposal_sink: RefCell<Option<ProposalSink>>,
    /// The content-box packet last delivered to `proposal_sink`, kept so a
    /// host whose child is replaced can replay the packet still in force.
    retained_proposal: Cell<Option<ProposalSize>>,
    /// The scrolling axes a scroll owner marked on this content widget:
    /// `(scrolls_horizontally, scrolls_vertically)`. A layout container it
    /// hosts reconstructs the owner's offer from this while no parent
    /// delivered a selected proposal — GTK delivers the real packet only
    /// after the viewport's first allocation pass already ran.
    scroll_axes: Cell<Option<(bool, bool)>>,
}

fn widget_layout(widget: &Widget) -> Option<NonNull<WidgetLayout>> {
    // SAFETY: `WIDGET_LAYOUT_KEY` is private to this module and is only ever
    // paired with `WidgetLayout`, so the read downcasts to the same type
    // `set_data` stored; widget data is only touched on the GTK main thread.
    unsafe { widget.data::<WidgetLayout>(WIDGET_LAYOUT_KEY) }
}

fn ensure_widget_layout(widget: &Widget) -> &WidgetLayout {
    if let Some(layout) = widget_layout(widget) {
        // SAFETY: the pointer targets widget qdata this module owns; it lives
        // as long as the widget, which outlives this main-thread call.
        return unsafe { layout.as_ref() };
    }
    // SAFETY: same key/type contract as `widget_layout`; widget data is only
    // touched on the GTK main thread.
    unsafe { widget.set_data(WIDGET_LAYOUT_KEY, WidgetLayout::default()) };
    let layout = widget_layout(widget).expect("widget layout marker was just stored");
    // SAFETY: as above.
    unsafe { layout.as_ref() }
}

/// Two proposals are the same packet only when both extents carry the same
/// bits: `None` never equals `Some`, `Some(0.0)` never equals `Some(-0.0)`,
/// infinities stay distinct from finite offers, and a NaN proposal compares
/// equal to itself — the same keying the contract's `MemoizedSubView` uses.
pub fn proposals_equal(a: ProposalSize, b: ProposalSize) -> bool {
    a.width.map(f32::to_bits) == b.width.map(f32::to_bits)
        && a.height.map(f32::to_bits) == b.height.map(f32::to_bits)
}

/// Removes `margin` from a finite proposal extent, leaving the non-finite
/// ones untouched: `f32::INFINITY` is an unbounded query, not a huge number
/// to subtract from, and a NaN proposal must survive the trip instead of
/// collapsing onto `0.0` through `f32::max`.
pub fn shrink_extent(extent: Option<f32>, margin: f32) -> Option<f32> {
    let value = extent?;
    Some(if value.is_finite() {
        (value - margin).max(0.0)
    } else {
        value
    })
}

/// [`shrink_extent`] applied per axis — margin-box to content-box geometry.
pub fn shrink_proposal(proposal: ProposalSize, horizontal: f32, vertical: f32) -> ProposalSize {
    ProposalSize::new(
        shrink_extent(proposal.width, horizontal),
        shrink_extent(proposal.height, vertical),
    )
}

/// The offer a scroll owner makes its content: `None` down every scrolling
/// axis, the finite viewport extent across each non-scrolling one.
pub fn scroll_offer(horizontal: bool, vertical: bool, width: f32, height: f32) -> ProposalSize {
    ProposalSize::new(
        if horizontal { None } else { Some(width) },
        if vertical { None } else { Some(height) },
    )
}

/// The offer a scrolling list makes each row it lays out natively: the
/// list's cross-axis extent, `None` down the scrolling axis.
pub fn row_offer(orientation: Orientation, cross_extent: f32) -> ProposalSize {
    match orientation {
        Orientation::Vertical => ProposalSize::new(Some(cross_extent), None),
        _ => ProposalSize::new(None, Some(cross_extent)),
    }
}

/// Marks `widget` as scroll-hosted content scrolling on the given axes.
///
/// GTK allocates a scroll view's content inside `GtkViewport`'s
/// `size_allocate` vfunc — before any `size-allocate` signal handler can run
/// — so a layout container hosted there reconstructs the owner's offer from
/// this annotation on the first pass rather than waiting for the viewport's
/// post-allocation delivery.
pub fn set_scroll_axes(widget: &Widget, horizontal: bool, vertical: bool) {
    ensure_widget_layout(widget)
        .scroll_axes
        .set(Some((horizontal, vertical)));
}

fn scroll_axes(widget: &Widget) -> Option<(bool, bool)> {
    widget_layout(widget).and_then(|layout| {
        // SAFETY: same soundness argument as `ensure_widget_layout`.
        unsafe { layout.as_ref() }.scroll_axes.get()
    })
}

/// The scroll owner's annotation on `widget` or the nearest ancestor that
/// hosts it: `Some((scrolls_horizontally, scrolls_vertically))`. The walk
/// crosses the transparent wrappers a scroll view's content may be wrapped
/// in and ends at the toplevel — natively hosted content outside a scroll
/// owner has no annotation.
pub fn scroll_axes_for(widget: &Widget) -> Option<(bool, bool)> {
    let mut current = Some(widget.clone());
    while let Some(w) = current {
        if let Some(axes) = scroll_axes(&w) {
            return Some(axes);
        }
        current = w.parent();
    }
    None
}

/// The stretch axis a `SubView` should report for `widget`: a live provider's
/// answer first, then the axis recorded at render time.
pub fn query_axis(widget: &Widget) -> Option<StretchAxis> {
    let layout = widget_layout(widget)?;
    // SAFETY: same soundness argument as `ensure_widget_layout`.
    let layout = unsafe { layout.as_ref() };
    if let Some(provider) = layout.axis_provider.borrow().as_ref().cloned() {
        return Some(provider(widget));
    }
    layout.reported_axis.get()
}

/// The layout priority a `SubView` should report for `widget`: the recorded
/// explicit or host-defaulted value first, then a live provider's answer.
pub fn query_priority(widget: &Widget) -> Option<i32> {
    let layout = widget_layout(widget)?;
    // SAFETY: same soundness argument as `ensure_widget_layout`.
    let layout = unsafe { layout.as_ref() };
    if let Some(priority) = layout.priority.get() {
        return Some(priority);
    }
    layout
        .priority_provider
        .borrow()
        .as_ref()
        .cloned()
        .map(|provider| provider(widget))
}

/// The axis recorded at render time, before any provider is consulted.
pub fn reported_axis(widget: &Widget) -> Option<StretchAxis> {
    widget_layout(widget).and_then(|layout| {
        // SAFETY: same soundness argument as `ensure_widget_layout`.
        unsafe { layout.as_ref() }.reported_axis.get()
    })
}

/// Records the axis `render_any_with_axis` resolved for `widget`'s view.
pub fn note_reported_axis(widget: &Widget, axis: StretchAxis) {
    ensure_widget_layout(widget).reported_axis.set(Some(axis));
}

/// Installs the live axis provider `SubView` queries consult before the
/// recorded declaration. Dynamic hosts and layout containers use it to
/// answer from live child state rather than a render-time snapshot.
pub fn install_axis_provider(widget: &Widget, provider: impl Fn(&Widget) -> StretchAxis + 'static) {
    *ensure_widget_layout(widget).axis_provider.borrow_mut() = Some(Rc::new(provider));
}

/// Installs the live priority provider consulted when no explicit or
/// host-defaulted `priority` was recorded.
pub fn install_priority_provider(widget: &Widget, provider: impl Fn(&Widget) -> i32 + 'static) {
    *ensure_widget_layout(widget).priority_provider.borrow_mut() = Some(Rc::new(provider));
}

/// Records the widget's layout priority — a `Spacer` default or a
/// `Metadata<LayoutPriority>` override; the last write wins, so a wrapper
/// rendered after its content overwrites the content's default.
pub fn set_layout_priority(widget: &Widget, priority: i32) {
    ensure_widget_layout(widget).priority.set(Some(priority));
}

/// Installs what a delivered selected proposal does on `widget`.
pub fn install_proposal_sink(widget: &Widget, sink: impl Fn(&Widget, ProposalSize) + 'static) {
    *ensure_widget_layout(widget).proposal_sink.borrow_mut() = Some(Rc::new(sink));
}

/// Installs the widget's raw measurement channel — see `measure_provider`
/// on [`WidgetLayout`] for the contract.
pub fn install_measure_provider(
    widget: &Widget,
    provider: impl Fn(&Widget, ProposalSize, StretchAxis) -> Option<ViewDimensions> + 'static,
) {
    *ensure_widget_layout(widget).measure_provider.borrow_mut() = Some(Rc::new(provider));
}

/// The widget's installed raw-measure provider, if any.
pub fn measure_provider(widget: &Widget) -> Option<MeasureProvider> {
    let layout = widget_layout(widget)?;
    // SAFETY: same soundness argument as `ensure_widget_layout`.
    unsafe { layout.as_ref() }
        .measure_provider
        .borrow()
        .as_ref()
        .cloned()
}

/// Marks `widget` transparent to proposals: a delivery is forwarded
/// unchanged to `child`, the content the widget hosts.
pub fn forward_proposals_to_child(widget: &Widget, child: &Widget) {
    let child = child.clone();
    install_proposal_sink(widget, move |_, proposal| {
        deliver_proposal(&child, proposal);
    });
}

/// Marks `widget` layout-transparent to `content`, the child it was built to
/// host: every layout channel reads through to it.
///
/// A delivered selected proposal forwards to `content`; a raw measurement
/// probe is answered by `content`, its `None`/non-finite extents and
/// explicit guides intact. The live axis and priority answers read through
/// as well — the wrapper's own recorded axis covers a markerless child, and
/// an explicit [`set_layout_priority`] on `widget` still wins over the
/// forwarded answer, the same precedence `SubView` queries use.
///
/// `content` is named, never discovered: chrome-bearing hosts (a badge's
/// overlay, a list row) hold siblings a first-child lookup could mistake.
pub fn transparent_to_content(widget: &Widget, content: &Widget) {
    forward_proposals_to_child(widget, content);

    let content_for_measure = content.clone();
    install_measure_provider(widget, move |_, proposal, resolved| {
        Some(measure_view(&content_for_measure, proposal, resolved))
    });

    let content_for_axis = content.clone();
    install_axis_provider(widget, move |w| {
        query_axis(&content_for_axis)
            .or_else(|| reported_axis(w))
            .unwrap_or(StretchAxis::None)
    });

    install_priority_provider(widget, {
        let content = content.clone();
        move |_| query_priority(&content).unwrap_or(0)
    });
}

/// Marks `row` — a `GtkBox` hosting `content` beside chrome siblings — as
/// the boundary where a delivered proposal becomes the content's slot.
///
/// The box's inner extent on its orientation axis shrinks by what every
/// visible fixed-size chrome sibling occupies (its natural margin-box extent) and one
/// spacing gap per adjacent pair; the cross axis forwards unchanged — a box
/// child always spans it. `content` is named explicitly because the chrome
/// is real children: a first-child assumption would re-target the delivery
/// the moment a sibling landed before it.
#[allow(
    clippy::cast_precision_loss,
    reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
)]
pub fn forward_box_content_slot(row: &gtk4::Box, content: &Widget) {
    let content = content.clone();
    install_proposal_sink(row.upcast_ref(), move |w, proposal| {
        let row_box = w
            .downcast_ref::<gtk4::Box>()
            .expect("content-slot sink belongs to a GtkBox");
        let orientation = row_box.orientation();
        let spacing = row_box.spacing() as f32;
        let mut occupied = 0.0_f32;
        let mut visible = 0_usize;
        let mut child = w.first_child();
        while let Some(sibling) = child {
            child = sibling.next_sibling();
            if !sibling.is_visible() {
                continue;
            }
            visible += 1;
            if sibling == content {
                continue;
            }
            let (_, natural, ..) = sibling.measure(orientation, -1);
            // GTK's public measure already includes the sibling's margins.
            occupied += natural.max(0) as f32;
        }
        let gaps = spacing * visible.saturating_sub(1) as f32;
        let forwarded = match orientation {
            Orientation::Horizontal => ProposalSize::new(
                shrink_extent(proposal.width, occupied + gaps),
                proposal.height,
            ),
            _ => ProposalSize::new(
                proposal.width,
                shrink_extent(proposal.height, occupied + gaps),
            ),
        };
        deliver_proposal(&content, forwarded);
    });
}

/// The proposal last delivered to `widget`'s sink, if any.
pub fn retained_proposal(widget: &Widget) -> Option<ProposalSize> {
    widget_layout(widget).and_then(|layout| {
        // SAFETY: same soundness argument as `ensure_widget_layout`.
        unsafe { layout.as_ref() }.retained_proposal.get()
    })
}

/// Re-runs `widget`'s sink with the packet already in force — no shrink, no
/// new retention: the retained packet *is* the sink's input. Chrome whose
/// occupancy changes without a new delivery (a sibling's visibility flip)
/// re-negotiates the content slot through this instead of waiting for the
/// parent's next allocation.
pub fn reoffer_proposal(widget: &Widget) {
    let Some(layout) = widget_layout(widget) else {
        return;
    };
    // SAFETY: same soundness argument as `ensure_widget_layout`.
    let layout = unsafe { layout.as_ref() };
    let packet = layout.retained_proposal.get();
    let sink = layout.proposal_sink.borrow().as_ref().cloned();
    if let (Some(packet), Some(sink)) = (packet, sink) {
        sink(widget, packet);
    }
}

/// Delivers the selected proposal `Layout::place` chose for `widget`.
///
/// The packet arrives in margin-box terms — the same terms the parent's
/// `SubView` negotiated sizes in — and is retained and sunk as the widget's
/// content-box packet: the margins are removed here, the same
/// [`shrink_proposal`] transformation `measure` applies to a probe, so
/// `Layout::place` and `size_that_fits` always run under identical geometry
/// however many wrapper hops the delivery crossed.
///
/// The packet is retained on the marker before the sink runs, so a sink that
/// replaces its child can replay it. Widgets without a sink — plain GTK
/// leaves — carry no packet and ignore the delivery.
#[allow(
    clippy::cast_precision_loss,
    reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
)]
pub fn deliver_proposal(widget: &Widget, proposal: ProposalSize) {
    let Some(layout) = widget_layout(widget) else {
        return;
    };
    // SAFETY: same soundness argument as `ensure_widget_layout`.
    let layout = unsafe { layout.as_ref() };
    let Some(sink) = layout.proposal_sink.borrow().as_ref().cloned() else {
        return;
    };
    let proposal = shrink_proposal(
        proposal,
        (widget.margin_start() + widget.margin_end()) as f32,
        (widget.margin_top() + widget.margin_bottom()) as f32,
    );
    layout.retained_proposal.set(Some(proposal));
    sink(widget, proposal);
}
