//! GTK4 List component implementation.
//!
//! Renders a `WaterUI` List as a GTK4 `ListView` inside a `ScrolledWindow`
//! for efficient handling of large lists.

use std::rc::Rc;

use gtk4::Widget;
use gtk4::prelude::*;
use gtk4::subclass::prelude::ObjectSubclassIsExt;
use nami::{Signal, SignalExt};
use waterui::component::list::ListConfig;
use waterui_core::views::Views;
use waterui_core::{Environment, Native};

use crate::component::GtkComponent;
use crate::components::layout::keyed_model::{KeyedModel, list_item_id};
use crate::layout::proposal::{forward_box_content_slot, reoffer_proposal, set_scroll_axes};
use crate::renderer::GtkRenderer;
use crate::util::{store_watcher_guard, store_watcher_guards};

impl GtkComponent for Native<ListConfig> {
    /// Renders a `WaterUI` `List` as a GTK4 scrollable list.
    ///
    /// Uses GTK `ListView` recycling so rows are created lazily for visible items.
    /// Each `ListItem` is rendered as a row with optional delete functionality.
    #[allow(
        clippy::too_many_lines,
        reason = "one cohesive widget-construction pass; splitting it would scatter GTK setup order"
    )]
    fn render(self, env: &Environment, _renderer: &mut GtkRenderer) -> Widget {
        let config = self.into_inner();
        let contents = config.contents;
        let on_delete = config.on_delete.map(Rc::new);
        let editing = config.editing;
        let scroll_controller = config.scroll_controller;
        let env = env.clone();

        // Create the scrolled container
        let scrolled_window = gtk4::ScrolledWindow::new();
        scrolled_window.set_hexpand(true);
        scrolled_window.set_vexpand(true);
        scrolled_window.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);

        let model = Rc::new(KeyedModel::new());
        let initial_ids = (0..contents.len().snapshot())
            .map(|index| {
                let id = contents
                    .get_id(index)
                    .expect("List contents must provide an ID for every row");
                i32::from(*id)
            })
            .collect::<Vec<_>>();
        model.reconcile(&initial_ids);

        let factory = gtk4::SignalListItemFactory::new();
        {
            let contents = contents.clone();
            let editing = editing;
            let on_delete = on_delete.clone();
            let env = env;
            factory.connect_bind(move |_, item| {
                let Some(list_item) = item.downcast_ref::<gtk4::ListItem>() else {
                    return;
                };
                let id = list_item_id(list_item);
                let index = usize::try_from(list_item.position())
                    .expect("GTK List position must fit in usize");
                let current_id = contents
                    .get_id(index)
                    .expect("GTK List position must exist in WaterUI contents");
                assert_eq!(
                    i32::from(*current_id),
                    id,
                    "GTK List model position must match WaterUI contents"
                );
                let Some(item) = contents.get_view(index) else {
                    list_item.set_child(Option::<&Widget>::None);
                    return;
                };

                // `WuiListRow` is the row's own allocation boundary: its
                // `size_allocate` delivers the list's offer to the content
                // slot before the box's layout allocates the children.
                let row_box = WuiListRow::new();
                row_box.set_margin_top(8);
                row_box.set_margin_bottom(8);
                row_box.set_margin_start(12);
                row_box.set_margin_end(12);

                let mut row_renderer = GtkRenderer::new();
                let content_widget = row_renderer.render_any(item.content, &env);
                content_widget.set_hexpand(true);
                row_box.append(&content_widget);
                // The row's chrome (margins, the delete button) does not
                // participate in the content's layout: a delivered proposal
                // negotiates the slot the content is actually allocated —
                // the offer minus what the visible siblings and the box's
                // spacing occupy.
                forward_box_content_slot(&row_box.content_box(), &content_widget);

                if let Some(on_delete) = on_delete.as_ref() {
                    let delete_btn = gtk4::Button::from_icon_name("edit-delete-symbolic");
                    delete_btn.add_css_class("destructive-action");
                    delete_btn.add_css_class("flat");

                    let env_clone = env.clone();
                    let on_delete = on_delete.clone();
                    let contents = contents.clone();
                    let list_item = list_item.downgrade();
                    delete_btn.connect_clicked(move |_| {
                        let list_item = list_item
                            .upgrade()
                            .expect("GTK deleted List row must remain bound while visible");
                        let current_index = usize::try_from(list_item.position())
                            .expect("GTK List position must fit in usize");
                        let current_id = contents
                            .get_id(current_index)
                            .expect("GTK deleted List row must still exist");
                        assert_eq!(
                            i32::from(*current_id),
                            id,
                            "GTK deleted List row identity changed before its action"
                        );
                        on_delete(&env_clone, current_index);
                    });

                    let show_delete = editing
                        .clone()
                        .zip(&item.deletable)
                        .map(|(is_editing, deletable)| is_editing && deletable)
                        .computed();

                    delete_btn.set_visible(show_delete.snapshot());
                    let visibility_guard = show_delete.watch({
                        let delete_btn = delete_btn.clone();
                        // A weak handle: the guard lives on `row_box` itself,
                        // so a strong capture would keep the row alive forever.
                        let row_weak = row_box.downgrade();
                        move |ctx| {
                            let visible = ctx.into_value();
                            let delete_btn = delete_btn.clone();
                            let row_weak = row_weak.clone();
                            glib::idle_add_local_once(move || {
                                delete_btn.set_visible(visible);
                                // The button's visibility is part of the slot
                                // negotiation — re-run it against the packet
                                // already in force.
                                if let Some(row_box) = row_weak.upgrade() {
                                    reoffer_proposal(row_box.content_box().upcast_ref());
                                }
                            });
                        }
                    });
                    store_watcher_guard(&row_box, Box::new(visibility_guard));

                    row_box.append(&delete_btn);
                }

                list_item.set_child(Some(&row_box));
                // The list owns the row's scrolling: a layout container
                // inside reconstructs the raw scroll axis from this marker —
                // installed at bind, before the `ListView`'s first allocation
                // pass over the row.
                set_scroll_axes(row_box.upcast_ref(), false, true);
            });
        }
        factory.connect_unbind(|_, item| {
            if let Some(list_item) = item.downcast_ref::<gtk4::ListItem>() {
                list_item.set_child(Option::<&Widget>::None);
            }
        });

        let selection = gtk4::NoSelection::new(Some(model.store()));
        let list_view = gtk4::ListView::new(Some(selection), Some(factory));
        list_view.set_hexpand(true);
        list_view.set_vexpand(true);
        list_view.add_css_class("boxed-list");

        // Reconcile the GTK model by stable WaterUI row identity.
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
        let mut guards = vec![contents_guard];
        if let Some(controller) = scroll_controller {
            let target = controller.target();
            let contents = contents.clone();
            let list_view_for_scroll = list_view.clone();
            guards.push(controller.generation().watch(move |_| {
                let index = target.snapshot();
                let len = contents.len().snapshot();
                assert!(
                    index < len,
                    "List scroll target {index} exceeds collection length {len}"
                );
                let index =
                    u32::try_from(index).expect("List scroll target exceeds the GTK index range");
                let list_view = list_view_for_scroll.clone();
                glib::idle_add_local_once(move || {
                    list_view.scroll_to(index, gtk4::ListScrollFlags::NONE, None);
                });
            }));
        }
        store_watcher_guards(&list_view, guards);

        scrolled_window.set_child(Some(&list_view));
        scrolled_window.upcast()
    }
}

mod imp {
    use std::cell::RefCell;

    use gtk4::prelude::*;
    use gtk4::subclass::prelude::*;

    use crate::layout::proposal::{deliver_proposal, row_offer};

    /// A host without a layout manager, so GTK invokes its allocation vfunc.
    /// The inner box retains GTK's native chrome and content allocation.
    #[derive(Debug, Default)]
    pub struct WuiListRow {
        pub content_box: RefCell<Option<gtk4::Box>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for WuiListRow {
        const NAME: &'static str = "WuiListRow";
        type Type = super::WuiListRow;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for WuiListRow {
        fn constructed(&self) {
            self.parent_constructed();
            let content_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            content_box.set_parent(&*self.obj());
            self.content_box.replace(Some(content_box));
        }

        fn dispose(&self) {
            if let Some(content_box) = self.content_box.borrow_mut().take() {
                content_box.unparent();
            }
        }
    }

    impl WidgetImpl for WuiListRow {
        fn request_mode(&self) -> gtk4::SizeRequestMode {
            self.obj().content_box().request_mode()
        }

        fn measure(&self, orientation: gtk4::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            self.obj().content_box().measure(orientation, for_size)
        }

        #[allow(
            clippy::cast_precision_loss,
            reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
        )]
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let content_box = self.obj().content_box();
            // GTK already removed the outer row's margins. The inner box
            // receives this entire slot, with an unspecified scrolling axis.
            deliver_proposal(
                content_box.upcast_ref(),
                row_offer(gtk4::Orientation::Vertical, width as f32),
            );
            content_box.allocate(width, height, baseline, None);
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            self.obj()
                .snapshot_child(&self.obj().content_box(), snapshot);
        }
    }
}

glib::wrapper! {
    /// A list-row host that delivers the list's offer to its inner box
    /// before its children are allocated.
    pub struct WuiListRow(ObjectSubclass<imp::WuiListRow>)
        @extends Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl WuiListRow {
    fn new() -> Self {
        glib::Object::new()
    }

    fn content_box(&self) -> gtk4::Box {
        self.imp()
            .content_box
            .borrow()
            .as_ref()
            .expect("constructed list row has a content box")
            .clone()
    }

    fn append(&self, child: &impl IsA<Widget>) {
        self.content_box().append(child);
    }
}

#[cfg(test)]
mod tests {
    use gtk4::Label;
    use waterui_core::layout::{ProposalSize, StretchAxis};
    use waterui_layout::stack::VStackLayout;

    use super::*;
    use crate::components::fixed_container_widget::WuiFixedContainer;
    use crate::layout::proposal::retained_proposal;

    fn init() {
        gtk4::init().expect("GTK tests need a display; run them under xvfb-run");
    }

    /// The row's `size_allocate` vfunc must deliver the negotiated slot —
    /// the list extent minus the row's margins, the box's spacing and the
    /// visible chrome — to the content *before* the box's layout allocates
    /// the children, so the very first pass runs under it.
    #[test]
    fn list_row_delivers_slot_offer_before_children_allocate() {
        init();
        let content = WuiFixedContainer::new(
            Box::new(VStackLayout::default()),
            vec![(Label::new(Some("row")).upcast(), StretchAxis::None)],
        );
        let row = WuiListRow::new();
        row.set_margin_start(12);
        row.set_margin_end(12);
        row.append(&content);
        let button = gtk4::Button::from_icon_name("edit-delete-symbolic");
        row.append(&button);
        forward_box_content_slot(&row.content_box(), content.upcast_ref());

        row.allocate(400, 50, -1, None);

        let (_, button_natural, ..) = button.measure(gtk4::Orientation::Horizontal, -1);
        assert_eq!(
            retained_proposal(content.upcast_ref()),
            Some(ProposalSize::new(
                Some(
                    400.0
                        - 24.0
                        - 8.0
                        - num_traits::cast::<i32, f32>(button_natural)
                            .expect("GTK integer geometry fits f32")
                ),
                None
            )),
            "the content's packet was not the slot the row actually allocated"
        );
    }
}
