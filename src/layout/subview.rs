//! `SubView` implementation using GTK widget measurement.

use gtk4::Widget;
use gtk4::prelude::*;
use waterui_core::MainThreadBound;
use waterui_core::layout::{
    ProposalSize, Size, StretchAxis, SubView, VerticalAlignment, ViewDimensions,
};

use crate::components::fixed_container_widget::WuiFixedContainer;
use crate::layout::proposal::{measure_provider, query_axis, query_priority, shrink_proposal};

fn layout_debug_enabled() -> bool {
    std::env::var_os("WATERUI_GTK_LAYOUT_DEBUG").is_some()
}

/// A wrapper around a GTK widget that implements the `SubView` trait.
///
/// This allows `waterui-layout` algorithms to measure GTK widgets
/// without knowing about GTK internals.
///
/// GTK widget measurement is main-thread only, so the widget is confined in a
/// [`MainThreadBound`].
#[derive(Debug)]
pub struct GtkSubView {
    widget: MainThreadBound<Widget>,
    stretch_axis: StretchAxis,
}

/// A `SubView` that always reports a fixed size regardless of proposal.
///
/// Used by `WuiFixedContainer` to answer GTK's minimum-size query: each
/// child's GTK minimum is captured eagerly, and the layout aggregates those
/// floors into the container's honest minimum (an hstack sums them, a vstack
/// takes the widest, padding adds its insets).
#[derive(Debug)]
pub struct FixedSizeSubView {
    size: Size,
    stretch_axis: StretchAxis,
}

impl FixedSizeSubView {
    /// Wraps the given pre-computed size with the child's declared stretch axis.
    #[must_use]
    pub const fn new(size: Size, stretch_axis: StretchAxis) -> Self {
        Self { size, stretch_axis }
    }
}

impl SubView for FixedSizeSubView {
    fn measure(&self, _proposal: ProposalSize) -> ViewDimensions {
        ViewDimensions::new(self.size)
    }

    fn stretch_axis(&self) -> StretchAxis {
        self.stretch_axis
    }

    fn priority(&self) -> i32 {
        0
    }
}

impl GtkSubView {
    /// Creates a new `GtkSubView` wrapping the given widget.
    #[must_use]
    pub fn new(widget: Widget, stretch_axis: StretchAxis) -> Self {
        Self {
            widget: MainThreadBound::new(widget),
            stretch_axis,
        }
    }

    /// Returns a reference to the underlying GTK widget.
    ///
    /// # Panics
    ///
    /// Panics when called off the main thread.
    #[must_use]
    pub fn widget(&self) -> &Widget {
        &self.widget
    }
}

/// The `for_size` GTK should see for a proposal extent: only a finite extent
/// is a real cross-axis constraint. `Some(f32::INFINITY)` is the unbounded
/// maximum query — mapping it to `i32::MAX` would ask GTK for the narrowest
/// extent under a 2-billion-pixel ceiling, which is not the same question.
fn proposal_extent_to_for_size(extent: Option<f32>) -> i32 {
    extent.filter(|value| value.is_finite()).map_or(-1, |w| {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
        )]
        let for_size = w as i32;
        for_size
    })
}

/// Measures `widget` under the raw proposal — the channel `SubView::measure`
/// and every transparent host's measure provider share.
///
/// `proposal` is margin-box geometry, the terms the parent's layout
/// negotiated: the widget's margins are removed before the probe descends
/// and folded back into the answer, explicit guides included. A widget with
/// a measure provider answers through it — a transparent host forwards the
/// probe to its content untouched, so the `None`/non-finite extents and
/// guides survive where GTK's integer `measure` would drop them. A
/// `WuiFixedContainer` answers through `layout_measure`, and everything else
/// falls to GTK's `measure`, the honest channel for native leaves.
///
/// `fallback_axis` is the stretch-axis claim the probe inherits when the
/// widget itself answers none; a transparent host passes its own resolved
/// claim down, so it survives however many hops sit between the marker that
/// carries it and the markerless leaf that means it.
#[allow(
    clippy::cast_precision_loss,
    reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
)]
pub(crate) fn measure_view(
    widget: &Widget,
    proposal: ProposalSize,
    fallback_axis: StretchAxis,
) -> ViewDimensions {
    let margin_start = widget.margin_start() as f32;
    let margin_top = widget.margin_top() as f32;
    let margin_h = margin_start + widget.margin_end() as f32;
    let margin_v = margin_top + widget.margin_bottom() as f32;
    let inner_proposal = shrink_proposal(proposal, margin_h, margin_v);
    let resolved_axis = query_axis(widget).unwrap_or(fallback_axis);
    let inner_dimensions = if let Some(dimensions) = measure_provider(widget)
        .and_then(|provider| provider(widget, inner_proposal, resolved_axis))
    {
        dimensions
    } else if let Some(container) = widget.downcast_ref::<WuiFixedContainer>() {
        container.layout_measure(inner_proposal)
    } else {
        // GTK's public measurement API already consumes and returns margin-box
        // geometry, including baseline offsets. Only our raw providers need
        // the explicit content-box transformation above.
        return leaf_measure(widget, proposal, resolved_axis);
    };
    let size = Size::new(
        inner_dimensions.size.width + margin_h,
        inner_dimensions.size.height + margin_v,
    );
    if layout_debug_enabled() {
        tracing::debug!(
            target: "waterui::gtk::layout",
            widget_type = %widget.type_().name(),
            proposal_width = ?proposal.width,
            proposal_height = ?proposal.height,
            inner_width = inner_dimensions.size.width,
            inner_height = inner_dimensions.size.height,
            margin_horizontal = margin_h,
            margin_vertical = margin_v,
            width = size.width,
            height = size.height,
            stretch_axis = ?resolved_axis,
            "Measured GTK subview"
        );
    }
    let mut dimensions = ViewDimensions::new(size);
    for (alignment, value) in inner_dimensions.explicit_horizontal_guides() {
        dimensions.set_horizontal(alignment, value + margin_start);
    }
    for (alignment, value) in inner_dimensions.explicit_vertical_guides() {
        dimensions.set_vertical(alignment, value + margin_top);
    }
    dimensions
}

/// The leaf answer: GTK's own measurement under the margin-box
/// proposal, with the stretch fill and the GTK minimum floor applied.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
)]
fn leaf_measure(
    widget: &Widget,
    proposal: ProposalSize,
    stretch_axis: StretchAxis,
) -> ViewDimensions {
    // Use GTK's measurement API; -1 means "no constraint" in measure().
    //
    // `for_size` asks the opposite-axis question — "how small can you be
    // while still fitting that extent". It is meaningful only in the
    // height-for-width direction, where a wrapping GtkLabel's height
    // genuinely depends on the width it will get. Passing the height
    // proposal into the *width* measure asks the label for the narrowest
    // width whose wrapped text still fits that height, collapsing honest
    // text into a few columns (a "Clipped" label under an 80px-high
    // proposal measures ~15px wide and renders one character per line).
    // Width is therefore always measured unconstrained, and only a
    // finite width proposal constrains the vertical measure: an
    // unbounded maximum query is `-1`, never `i32::MAX`.
    let for_width = proposal_extent_to_for_size(proposal.width);

    // Measure horizontal (width)
    let (min_width, natural_width, _min_baseline, _nat_baseline) =
        widget.measure(gtk4::Orientation::Horizontal, -1);

    // Measure vertical (height)
    let (min_height, natural_height, min_baseline, nat_baseline) =
        widget.measure(gtk4::Orientation::Vertical, for_width);

    // Default behavior: intrinsic size clamped by proposal.
    let mut width = proposal.width.map_or(natural_width as f32, |proposed| {
        proposed.min(natural_width as f32)
    });

    let mut height = proposal.height.map_or(natural_height as f32, |proposed| {
        proposed.min(natural_height as f32)
    });

    // For stretch axes, fill the proposed extent instead of shrinking to
    // intrinsic size. This prevents feedback loops where a transient narrow
    // allocation becomes the next intrinsic width (e.g. GtkGLArea -> 18px lock-in).
    if stretch_axis.stretches_horizontal()
        && let Some(proposed) = proposal.width
    {
        width = proposed.max(0.0);
    }
    if stretch_axis.stretches_vertical()
        && let Some(proposed) = proposal.height
    {
        height = proposed.max(0.0);
    }

    // GTK's own minimum is a floor, not a suggestion: below it the widget
    // still draws at its minimum, so reporting less lies to the layout.
    // For a wrapping label this floor is the widest wrappable unit — and
    // it is also what `WuiFixedContainer`'s minimum-size answer is built
    // from, so it must surface here. (GTK occasionally reports a minimum
    // above the natural under degenerate for_size values; clamp it.)
    width = width.max((min_width.min(natural_width)) as f32);
    height = height.max((min_height.min(natural_height)) as f32);
    if layout_debug_enabled() {
        tracing::debug!(
            target: "waterui::gtk::layout",
            widget_type = %widget.type_().name(),
            proposal_width = ?proposal.width,
            proposal_height = ?proposal.height,
            for_width,
            min_width,
            min_height,
            natural_width,
            natural_height,
            width,
            height,
            stretch_axis = ?stretch_axis,
            "Measured GTK leaf"
        );
    }

    let mut dimensions = ViewDimensions::new(Size { width, height });
    if min_baseline >= 0 {
        dimensions.set_vertical(VerticalAlignment::FirstBaseline, min_baseline as f32);
    }
    if nat_baseline >= 0 {
        dimensions.set_vertical(VerticalAlignment::LastBaseline, nat_baseline as f32);
    }
    dimensions
}

impl SubView for GtkSubView {
    fn measure(&self, proposal: ProposalSize) -> ViewDimensions {
        measure_view(&self.widget, proposal, self.stretch_axis)
    }

    /// The widget's live stretch axis: a provider installed by a dynamic or
    /// layout host answers first, then the declaration recorded at render
    /// time, then the snapshot this wrapper was built with.
    fn stretch_axis(&self) -> StretchAxis {
        query_axis(&self.widget).unwrap_or(self.stretch_axis)
    }

    /// The widget's layout priority: the explicit `LayoutPriority` override
    /// or a host default like `Spacer`'s when one was recorded, else the
    /// contract's default of zero.
    fn priority(&self) -> i32 {
        query_priority(&self.widget).unwrap_or(0)
    }
}
