//! GTK dynamic view component implementation.

use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Orientation, Widget};
use nami::watcher::Context;
use waterui_core::dynamic::Dynamic;
use waterui_core::layout::StretchAxis;
use waterui_core::{AnyView, Environment, Native};

use crate::component::GtkComponent;
use crate::layout::proposal::{
    deliver_proposal, install_axis_provider, install_measure_provider, install_priority_provider,
    install_proposal_sink, query_axis, query_priority, retained_proposal,
};
use crate::layout::subview::measure_view;
use crate::renderer::GtkRenderer;

impl GtkComponent for Native<Dynamic> {
    fn render(self, env: &Environment, _renderer: &mut GtkRenderer) -> Widget {
        let dynamic = self.into_inner();

        // The host forwards the layout contract's child-facing answers to
        // whatever it currently holds: stretch axis and layout priority read
        // through to the live child, and a delivered selected proposal lands
        // on the live child — then is replayed to its replacement, so a swap
        // mid-layout keeps the packet the parent negotiated in force.
        let container = GtkBox::new(Orientation::Vertical, 0);
        install_axis_provider(container.upcast_ref(), |w| {
            w.first_child()
                .and_then(|child| query_axis(&child))
                .unwrap_or(StretchAxis::None)
        });
        install_priority_provider(container.upcast_ref(), |w| {
            w.first_child()
                .and_then(|child| query_priority(&child))
                .unwrap_or(0)
        });
        install_proposal_sink(container.upcast_ref(), |w, proposal| {
            if let Some(child) = w.first_child() {
                deliver_proposal(&child, proposal);
            }
        });
        // The host measures as whatever it currently holds: the raw probe —
        // `None`/non-finite extents and explicit guides included — lands on
        // the live child, and `None` while it holds nothing leaves GTK's own
        // measure to answer for the empty box.
        install_measure_provider(container.upcast_ref(), |w, proposal, resolved| {
            w.first_child()
                .map(|child| measure_view(&child, proposal, resolved))
        });

        // Dynamic updates can happen later; render updates using a fresh renderer to avoid
        // keeping a raw pointer to the original `GtkRenderer` (which is short-lived).
        let env = env.clone();
        let container_clone = container.clone();

        dynamic.connect(move |ctx: Context<AnyView>| {
            let view = ctx.into_value();
            let env = env.clone();
            let container = container_clone.clone();
            glib::idle_add_local_once(move || {
                // Clear existing children
                while let Some(child) = container.first_child() {
                    container.remove(&child);
                }

                // Render the new view, recording its resolved stretch axis on
                // the widget so the providers above read it.
                let mut renderer = GtkRenderer::new();
                let (widget, _axis) = renderer.render_any_with_axis(view, &env);
                container.append(&widget);

                // A replacement inherits the packet still in force; the
                // parent's next delivery refreshes it.
                if let Some(proposal) = retained_proposal(container.upcast_ref()) {
                    deliver_proposal(&widget, proposal);
                }
                // Swapping the child can change every answer the providers
                // give — axis, priority, size — so the host renegotiates.
                container.queue_resize();
            });
        });

        container.upcast()
    }
}
