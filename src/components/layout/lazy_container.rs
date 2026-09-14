//! GTK4 `LazyContainer` component with virtual scrolling.
//!
//! Uses GTK4's `ListView` with `SignalListItemFactory` for lazy view reconstruction.

use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Orientation, Widget};
use nami::Signal;
use waterui_core::layout::{Layout, StretchAxis};
use waterui_core::views::{SharedAnyViews, Views};
use waterui_core::{AnyView, Environment, Native};
use waterui_layout::container::LazyContainer;
use waterui_layout::stack::{LazyStackAxis, lazy_stack_axis};

use crate::component::GtkComponent;
use crate::components::fixed_container_widget::WuiFixedContainer;
use crate::components::layout::keyed_model::{KeyedModel, list_item_id};
use crate::renderer::GtkRenderer;
use crate::util::{effective_stretch_axis, store_watcher_guard};

impl GtkComponent for Native<LazyContainer> {
    fn render(self, env: &Environment, _renderer: &mut GtkRenderer) -> Widget {
        let (layout, contents) = self.into_inner().into_inner();
        let contents = SharedAnyViews::from(contents);
        let env = env.clone();

        let model = Rc::new(KeyedModel::new());
        let initial_ids = (0..contents.len().get())
            .map(|index| {
                let id = contents
                    .get_id(index)
                    .expect("LazyContainer contents must provide an ID for every child");
                i32::from(*id)
            })
            .collect::<Vec<_>>();
        model.reconcile(&initial_ids);

        // The axis, spacing and cross-axis alignment all come from the layout the
        // container was built with. Deriving the axis from `Layout::stretch_axis`
        // instead — a different question — is what laid every lazy `HStack` out
        // vertically once the stacks became content-sized. Layouts that do not
        // virtualize (the snackbar overlay's `AbsoluteLayout` layer) materialize
        // into a `WuiFixedContainer` instead.
        let Some(axis) = lazy_stack_axis(layout.as_ref()) else {
            return render_fixed(layout, &contents, &env);
        };
        let (orientation, spacing, cross_alignment) = match &axis {
            LazyStackAxis::Vertical { spacing, alignment } => (
                Orientation::Vertical,
                spacing.get(),
                gtk_align_from_horizontal(*alignment),
            ),
            LazyStackAxis::Horizontal { spacing, alignment } => (
                Orientation::Horizontal,
                spacing.get(),
                gtk_align_from_vertical(*alignment),
            ),
        };

        // GtkListView exposes no inter-row spacing property (that one is
        // GridView's); the stack's spacing becomes a margin on the bound child
        // instead, which GTK's list machinery honours as the row gap.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "GTK spacing is integer pixels while WaterUI layout is f32"
        )]
        let spacing_px = spacing.max(0.0) as i32;

        // Create factory for lazy binding
        let factory = gtk4::SignalListItemFactory::new();
        let contents_clone = contents.clone();
        let env_clone = env;

        factory.connect_setup(|_, item| {
            let list_item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let placeholder = gtk4::Box::new(Orientation::Vertical, 0);
            list_item.set_child(Some(&placeholder));
        });

        factory.connect_bind(move |_, item| {
            let list_item = item.downcast_ref::<gtk4::ListItem>().unwrap();
            let id = list_item_id(list_item);
            let index = usize::try_from(list_item.position())
                .expect("GTK LazyContainer position must fit in usize");
            let current_id = contents_clone
                .get_id(index)
                .expect("GTK LazyContainer position must exist in WaterUI contents");
            assert_eq!(
                i32::from(*current_id),
                id,
                "GTK LazyContainer model position must match WaterUI contents"
            );

            // Reconstruct view lazily
            if let Some(view) = contents_clone.get_view(index) {
                // Render with a fresh renderer to avoid holding a raw pointer.
                let mut renderer = GtkRenderer::new();
                let widget = renderer.render_any(view, &env_clone);
                if spacing_px > 0 {
                    match orientation {
                        Orientation::Vertical => widget.set_margin_bottom(spacing_px),
                        _ => widget.set_margin_end(spacing_px),
                    }
                }
                list_item.set_child(Some(&widget));
            } else {
                list_item.set_child(Option::<&Widget>::None);
            }
        });

        factory.connect_unbind(|_, item| {
            if let Some(list_item) = item.downcast_ref::<gtk4::ListItem>() {
                list_item.set_child(Option::<&Widget>::None);
            }
        });

        // Create ListView (NO ScrolledWindow - parent handles scrolling)
        let selection = gtk4::NoSelection::new(Some(model.store()));
        let list_view = gtk4::ListView::new(Some(selection), Some(factory));
        list_view.set_orientation(orientation);
        match orientation {
            Orientation::Vertical => list_view.set_halign(cross_alignment),
            _ => list_view.set_valign(cross_alignment),
        }
        list_view.set_hexpand(true);
        list_view.set_vexpand(true);

        // Reconcile the GTK model by stable WaterUI child identity.
        let contents_guard = contents.watch(.., {
            let model = Rc::clone(&model);
            move |context| {
                let ids = context
                    .value()
                    .iter()
                    .map(|id| i32::from(**id))
                    .collect::<Vec<_>>();
                let model = Rc::clone(&model);
                glib::idle_add_local_once(move || {
                    model.reconcile(&ids);
                });
            }
        });
        store_watcher_guard(&list_view, Box::new(contents_guard));

        list_view.upcast()
    }
}

/// Realizes a `LazyContainer` whose layout is not a virtualizable stack —
/// today `AbsoluteLayout`, which the snackbar overlay uses for its
/// full-window layer. Membership changes rebuild the whole child set: these
/// containers carry a handful of self-positioning children, so list
/// virtualization would buy nothing.
fn render_fixed(
    layout: Box<dyn Layout>,
    contents: &SharedAnyViews<AnyView>,
    env: &Environment,
) -> Widget {
    let container = WuiFixedContainer::new(layout, materialize_children(contents, env));
    container.set_hexpand(true);
    container.set_vexpand(true);
    let contents_guard = contents.watch(.., {
        let contents = contents.clone();
        let container = container.clone();
        let env = env.clone();
        move |_| {
            let contents = contents.clone();
            let container = container.clone();
            let env = env.clone();
            glib::idle_add_local_once(move || {
                container.set_children(materialize_children(&contents, &env));
            });
        }
    });
    store_watcher_guard(&container, Box::new(contents_guard));
    container.upcast()
}

fn materialize_children(
    contents: &SharedAnyViews<AnyView>,
    env: &Environment,
) -> Vec<(Widget, StretchAxis)> {
    (0..contents.len().get())
        .filter_map(|index| contents.get_view(index))
        .map(|view| {
            let axis = effective_stretch_axis(&view);
            let mut renderer = GtkRenderer::new();
            (renderer.render_any(view, env), axis)
        })
        .collect()
}

/// Maps a `WaterUI` cross-axis alignment onto GTK's, for a vertical stack.
fn gtk_align_from_horizontal(alignment: waterui_layout::HorizontalAlignment) -> gtk4::Align {
    use waterui_layout::HorizontalAlignment;
    if alignment == HorizontalAlignment::Leading {
        gtk4::Align::Start
    } else if alignment == HorizontalAlignment::Trailing {
        gtk4::Align::End
    } else {
        gtk4::Align::Center
    }
}

/// Maps a `WaterUI` cross-axis alignment onto GTK's, for a horizontal stack.
fn gtk_align_from_vertical(alignment: waterui_layout::VerticalAlignment) -> gtk4::Align {
    use waterui_layout::VerticalAlignment;
    if alignment == VerticalAlignment::Top {
        gtk4::Align::Start
    } else if alignment == VerticalAlignment::Bottom {
        gtk4::Align::End
    } else {
        gtk4::Align::Center
    }
}
