//! GTK Spacer component implementation.

use gtk4::Widget;
use gtk4::prelude::*;
use waterui_core::layout::{Size, StretchAxis, ViewDimensions};
use waterui_core::{Environment, Native};
use waterui_layout::spacer::Spacer;
use waterui_layout::stack::Axis;

use crate::component::GtkComponent;
use crate::components::fixed_container_widget::WuiFixedContainer;
use crate::layout::proposal::{install_measure_provider, note_reported_axis, set_layout_priority};
use crate::renderer::GtkRenderer;

/// The spacer's answer to a measure probe: `min_length` on the hosting
/// stack's main axis, zero everywhere else.
///
/// The layout that owns the answer is the nearest `WuiFixedContainer`
/// ancestor whose own stretch claim does not echo `MainAxis` back: modifier
/// containers (`Padding`, `Background`, `Overlay`, …) are transparent — they
/// report their content child's axis — so the walk climbs through them to
/// the container that actually resolves a flexible child. A `VStack` or
/// `HStack` answers on its axis; anything else — a `ZStack`, a scroll host,
/// or no `WaterUI` container at all — leaves the spacer claiming nothing, so
/// it answers zero.
fn spacer_size(widget: &Widget, min_length: f32) -> Size {
    let mut node = widget.parent();
    while let Some(parent) = node {
        if let Some(container) = parent.downcast_ref::<WuiFixedContainer>()
            && !container.relays_stretch_axis(StretchAxis::MainAxis)
        {
            return match container.stack_main_axis() {
                Some(Axis::Vertical) => Size::new(0.0, min_length),
                Some(Axis::Horizontal) => Size::new(min_length, 0.0),
                _ => Size::zero(),
            };
        }
        node = parent.parent();
    }
    Size::zero()
}

impl GtkComponent for Native<Spacer> {
    /// Renders a `WaterUI` Spacer as an empty drawing area.
    ///
    /// A spacer draws nothing; the widget only carries the measure answers
    /// the layout protocol asks for. The minimum length is answered on the
    /// hosting stack's main axis whatever the proposal — the floor the stack
    /// keeps under compression before it hands the spacer any surplus — and
    /// every other axis and non-stack host answers zero. `Spacer` reports
    /// the lowest default priority so a stack squeezes it before touching
    /// real content; an explicit `Metadata<LayoutPriority>` renders after
    /// this and overrides it. The stretch claim is the view's declared
    /// [`StretchAxis::MainAxis`], recorded on the widget at render time.
    fn render(self, _env: &Environment, _renderer: &mut GtkRenderer) -> Widget {
        let spacer = self.into_inner();
        let min_length = spacer.min_length();

        let widget = gtk4::DrawingArea::new();
        install_measure_provider(
            widget.upcast_ref(),
            move |widget, _proposal, _reported, _memo| {
                Some(ViewDimensions::new(spacer_size(widget, min_length)))
            },
        );
        note_reported_axis(widget.upcast_ref(), StretchAxis::MainAxis);
        set_layout_priority(widget.upcast_ref(), Spacer::DEFAULT_LAYOUT_PRIORITY);
        widget.upcast()
    }
}
