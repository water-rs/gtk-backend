//! `SubView` implementation using GTK widget measurement.

use gtk4::Widget;
use gtk4::prelude::*;
use waterui_core::MainThreadBound;
use waterui_core::layout::{
    ProposalSize, Size, StretchAxis, SubView, VerticalAlignment, ViewDimensions,
};

use crate::components::fixed_container_widget::WuiFixedContainer;

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
    priority: i32,
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
            priority: 0,
        }
    }

    /// Creates a new `GtkSubView` with custom priority.
    #[must_use]
    pub fn with_priority(widget: Widget, stretch_axis: StretchAxis, priority: i32) -> Self {
        Self {
            widget: MainThreadBound::new(widget),
            stretch_axis,
            priority,
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

impl SubView for GtkSubView {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
    )]
    fn measure(&self, proposal: ProposalSize) -> ViewDimensions {
        if let Some(container) = self.widget.downcast_ref::<WuiFixedContainer>() {
            let margin_h = (self.widget.margin_start() + self.widget.margin_end()) as f32;
            let margin_v = (self.widget.margin_top() + self.widget.margin_bottom()) as f32;
            let inner_proposal = ProposalSize::new(
                proposal.width.map(|w| (w - margin_h).max(0.0)),
                proposal.height.map(|h| (h - margin_v).max(0.0)),
            );
            let inner_dimensions = container.layout_measure(inner_proposal);
            let size = Size::new(
                inner_dimensions.size.width + margin_h,
                inner_dimensions.size.height + margin_v,
            );
            if layout_debug_enabled() {
                tracing::debug!(
                    target: "waterui::gtk::layout",
                    widget_type = %self.widget.type_().name(),
                    proposal_width = ?proposal.width,
                    proposal_height = ?proposal.height,
                    inner_width = inner_dimensions.size.width,
                    inner_height = inner_dimensions.size.height,
                    margin_horizontal = margin_h,
                    margin_vertical = margin_v,
                    width = size.width,
                    height = size.height,
                    stretch_axis = ?self.stretch_axis,
                    "Measured GTK container subview"
                );
            }
            let mut dimensions = ViewDimensions::new(size);
            for (alignment, value) in inner_dimensions.explicit_horizontal_guides() {
                dimensions.set_horizontal(alignment, value + self.widget.margin_start() as f32);
            }
            for (alignment, value) in inner_dimensions.explicit_vertical_guides() {
                dimensions.set_vertical(alignment, value + self.widget.margin_top() as f32);
            }
            return dimensions;
        }

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
        // Width is therefore always measured unconstrained.
        let for_width = proposal.width.map_or(-1, |w| w as i32);

        // Measure horizontal (width)
        let (min_width, natural_width, _min_baseline, _nat_baseline) =
            self.widget.measure(gtk4::Orientation::Horizontal, -1);

        // Measure vertical (height)
        let (min_height, natural_height, min_baseline, nat_baseline) =
            self.widget.measure(gtk4::Orientation::Vertical, for_width);

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
        if self.stretch_axis.stretches_horizontal()
            && let Some(proposed) = proposal.width
        {
            width = proposed.max(0.0);
        }
        if self.stretch_axis.stretches_vertical()
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
                widget_type = %self.widget.type_().name(),
                proposal_width = ?proposal.width,
                proposal_height = ?proposal.height,
                for_width,
                min_width,
                min_height,
                natural_width,
                natural_height,
                width,
                height,
                stretch_axis = ?self.stretch_axis,
                "Measured GTK subview"
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

    fn stretch_axis(&self) -> StretchAxis {
        self.stretch_axis
    }

    fn priority(&self) -> i32 {
        self.priority
    }
}
