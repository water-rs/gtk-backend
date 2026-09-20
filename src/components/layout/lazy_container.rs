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
use crate::layout::proposal::{install_axis_provider, set_scroll_axes};
use crate::renderer::GtkRenderer;
use crate::util::store_watcher_guard;

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
        let (scrolls_h, scrolls_v) = match orientation {
            Orientation::Vertical => (false, true),
            _ => (true, false),
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
        wire_factory(
            &factory,
            contents.clone(),
            env,
            orientation,
            spacing_px,
            (scrolls_h, scrolls_v),
        );

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

        // A `ListView` fills its cross axis by construction: tiles span
        // `max(natural, allocated)` across it and each row's `BinLayout`
        // fills its tile, which is also what the `hexpand`/`vexpand` above
        // already tell GTK parents. `LazyContainer::stretch_axis` reports
        // `None` because it cannot enumerate lazy children — left unclaimed,
        // a `.leading()` stack allocates the list only its intrinsic cross
        // extent and rows that asked to fill (`max_width(f32::INFINITY)`)
        // shrink inside it.
        let fill_axis = match orientation {
            Orientation::Vertical => StretchAxis::Horizontal,
            _ => StretchAxis::Vertical,
        };
        install_axis_provider(list_view.upcast_ref(), move |_| fill_axis);

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

/// Wires the factory's row lifecycle: an empty placeholder box on setup, the
/// `WaterUI` view for the row's position on bind, and the child released on
/// unbind.
fn wire_factory(
    factory: &gtk4::SignalListItemFactory,
    contents: SharedAnyViews<AnyView>,
    env: Environment,
    orientation: Orientation,
    spacing_px: i32,
    scroll_axes: (bool, bool),
) {
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
        let current_id = contents
            .get_id(index)
            .expect("GTK LazyContainer position must exist in WaterUI contents");
        assert_eq!(
            i32::from(*current_id),
            id,
            "GTK LazyContainer model position must match WaterUI contents"
        );

        // Reconstruct view lazily
        if let Some(view) = contents.get_view(index) {
            // Render with a fresh renderer to avoid holding a raw pointer.
            let mut renderer = GtkRenderer::new();
            let widget = renderer.render_any(view, &env);
            if spacing_px > 0 {
                match orientation {
                    Orientation::Vertical => widget.set_margin_bottom(spacing_px),
                    _ => widget.set_margin_end(spacing_px),
                }
            }
            list_item.set_child(Some(&widget));
            // The list owns the row's scrolling: a layout container in
            // the row reconstructs the raw scroll offer from this marker
            // and its own allocation inside its `size_allocate` vfunc —
            // strictly before it allocates its children, on the first
            // pass and every pass after. There is no `size-allocate`
            // signal in GTK4 to hook from the outside.
            set_scroll_axes(&widget, scroll_axes.0, scroll_axes.1);
        } else {
            list_item.set_child(Option::<&Widget>::None);
        }
    });

    factory.connect_unbind(|_, item| {
        if let Some(list_item) = item.downcast_ref::<gtk4::ListItem>() {
            list_item.set_child(Option::<&Widget>::None);
        }
    });
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
            let mut renderer = GtkRenderer::new();
            renderer.render_any_with_axis(view, env)
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

#[cfg(test)]
mod tests {
    use nami::Computed;
    use waterui_core::Str;
    use waterui_layout::stack::{HorizontalAlignment, VStackLayout};

    use super::*;
    use crate::components::fixed_container_widget::WuiFixedContainer;
    use crate::layout::proposal::query_axis;

    fn init() {
        gtk4::init().expect("GTK tests need a display; run them under xvfb-run");
    }

    /// A `VStack::for_each` drawer in a scroll view: the `ListView` the lazy
    /// stack realizes to fills its cross axis by construction (tiles span
    /// `max(natural, allocated)` and each row's `BinLayout` fills its tile),
    /// but `LazyContainer` cannot enumerate its children, so it declares
    /// `StretchAxis::None`. Without the realization's claim a leading column
    /// allocates the list only its intrinsic width, and rows that asked to
    /// fill — `max_width(f32::INFINITY)` — shrink inside it.
    #[test]
    fn lazy_stack_fills_scroll_hosted_column_cross_axis() {
        init();
        let env = Environment::new();
        let mut renderer = GtkRenderer::new();
        let list = Native::new(LazyContainer::new(
            VStackLayout {
                alignment: HorizontalAlignment::Leading,
                spacing: Computed::constant(0.0),
            },
            vec![Str::from("row")],
        ))
        .render(&env, &mut renderer);
        assert_eq!(
            query_axis(&list),
            Some(StretchAxis::Horizontal),
            "a vertical lazy list did not claim the cross-axis fill its tiles perform"
        );

        // The parent's `render_any_with_axis` records `LazyContainer`'s
        // declared axis — `None` — in its child list; the provider is what a
        // query answers. `set_scroll_axes` marks the column as vertical
        // scroll content, the marker a scroll view installs on its child.
        let column = WuiFixedContainer::new(
            Box::new(VStackLayout {
                alignment: HorizontalAlignment::Leading,
                spacing: Computed::constant(0.0),
            }),
            vec![(list.clone(), StretchAxis::None)],
        );
        set_scroll_axes(column.upcast_ref(), false, true);

        column.allocate(296, 480, -1, None);

        assert_eq!(
            list.width(),
            296,
            "the lazy stack was allocated its intrinsic width instead of the viewport's"
        );
    }
}
