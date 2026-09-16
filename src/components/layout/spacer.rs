//! GTK Spacer component implementation.

use gtk4::Widget;
use gtk4::prelude::*;
use waterui_core::{Environment, Native};
use waterui_layout::spacer::{Spacer, SpacerLayout};

use crate::component::GtkComponent;
use crate::components::fixed_container_widget::WuiFixedContainer;
use crate::layout::proposal::{note_reported_axis, set_layout_priority};
use crate::renderer::GtkRenderer;

impl GtkComponent for Native<Spacer> {
    /// Renders a `WaterUI` Spacer through its own [`SpacerLayout`].
    ///
    /// The container is the vehicle that runs the layout's measurement:
    /// `SpacerLayout::size_that_fits` reports `min_length` on both axes, and
    /// the parent stack owns the expansion — the container's placement frame
    /// is whatever the stack granted. `Spacer` reports the lowest default
    /// priority so a stack squeezes it before touching real content; an
    /// explicit `Metadata<LayoutPriority>` renders after this and overrides
    /// it. The stretch axis itself is the view's declared
    /// [`StretchAxis::MainAxis`](waterui_core::layout::StretchAxis::MainAxis),
    /// recorded on the widget at render time — a leaf `SpacerLayout` has no
    /// children to derive an axis from.
    fn render(self, _env: &Environment, _renderer: &mut GtkRenderer) -> Widget {
        let spacer = self.into_inner();

        let widget = WuiFixedContainer::new(Box::new(SpacerLayout::from(spacer)), Vec::new());
        note_reported_axis(
            widget.upcast_ref(),
            waterui_core::layout::StretchAxis::MainAxis,
        );
        set_layout_priority(widget.upcast_ref(), Spacer::DEFAULT_LAYOUT_PRIORITY);
        widget.upcast()
    }
}
