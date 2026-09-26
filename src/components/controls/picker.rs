//! GTK4 Picker (DropDown/ComboBox) component implementation.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::Widget;
use gtk4::prelude::*;
use nami::{Signal, SignalExt};
use waterui_core::id::Id;
use waterui_core::{Environment, Native};
use waterui_form::picker::{PickerConfig, PickerItem, PickerStyle};

use crate::component::GtkComponent;
use crate::renderer::GtkRenderer;
use crate::util::store_watcher_guards;

/// The plain-text label of a picker item.
fn item_label(item: &PickerItem<Id>, env: &Environment) -> String {
    item.content
        .resolve(env)
        .content
        .snapshot()
        .to_plain()
        .to_string()
}

impl GtkComponent for Native<PickerConfig> {
    /// Renders a `WaterUI` `Picker` as a GTK4 `DropDown`, or as a grouped
    /// set of `CheckButton` radios under [`PickerStyle::Radio`].
    #[allow(
        clippy::cast_possible_truncation,
        reason = "widget-model indices are bounded by the collection length"
    )]
    fn render(self, env: &Environment, _renderer: &mut GtkRenderer) -> Widget {
        let config = self.into_inner();
        if config.style == PickerStyle::Radio {
            return render_radio_group(env, config);
        }
        let items = config.items;
        let selection = config.selection;

        // Create dropdown (model is installed reactively below).
        let dropdown = gtk4::DropDown::new(None::<gtk4::StringList>, gtk4::Expression::NONE);

        // The picker's label names the control. Without it a screen reader
        // announces only the selected option, which says what was chosen but
        // never what it was choosing — and the label is mandatory at
        // construction precisely so that cannot happen.
        {
            let label = config
                .label
                .resolve(env)
                .accessibility_label()
                .snapshot()
                .to_plain();
            dropdown.update_property(&[gtk4::accessible::Property::Label(label.as_str())]);
        }

        // Shared IDs for two-way selection syncing.
        let ids = Rc::new(RefCell::new(Vec::new()));

        let refresh_items: Rc<dyn Fn(Vec<PickerItem<Id>>)> = {
            let dropdown = dropdown.clone();
            let ids = ids.clone();
            let selection = selection.clone();
            let env = env.clone();
            Rc::new(move |item_list| {
                let labels: Vec<String> = item_list
                    .iter()
                    .map(|item| item_label(item, &env))
                    .collect();
                let new_ids: Vec<_> = item_list.iter().map(|item| item.tag).collect();
                let current_id = selection.snapshot();

                *ids.borrow_mut() = new_ids;

                let label_refs = labels.iter().map(String::as_str).collect::<Vec<_>>();
                let string_list = gtk4::StringList::new(&label_refs);
                dropdown.set_model(Some(&string_list));

                let ids_ref = ids.borrow();
                if ids_ref.is_empty() {
                    dropdown.set_selected(gtk4::INVALID_LIST_POSITION);
                } else if let Some(index) = ids_ref.iter().position(|id| *id == current_id) {
                    dropdown.set_selected(index as u32);
                } else {
                    dropdown.set_selected(0);
                }
            })
        };

        refresh_items(items.snapshot());

        let ids_for_handler = ids.clone();
        let selection_for_handler = selection.clone();

        // Watch dropdown selection changes -> update binding
        dropdown.connect_selected_notify(move |dropdown| {
            let selected_idx = dropdown.selected() as usize;
            let ids_ref = ids_for_handler.borrow();
            if let Some(selected_id) = ids_ref.as_slice().get(selected_idx).copied()
                && selection_for_handler.snapshot() != selected_id
            {
                selection_for_handler.set(selected_id);
            }
        });

        // Watch binding changes -> update dropdown
        let selection_guard = selection.computed().watch({
            let dropdown = dropdown.clone();
            move |ctx| {
                let value = ctx.into_value();
                let dropdown = dropdown.clone();
                let ids = ids.clone();
                glib::idle_add_local_once(move || {
                    if let Some(idx) = ids.borrow().iter().position(|id| *id == value)
                        && dropdown.selected() != idx as u32
                    {
                        dropdown.set_selected(idx as u32);
                    }
                });
            }
        });

        // Watch items changes -> update dropdown model and selection
        let items_guard = items.watch({
            let refresh_items = refresh_items.clone();
            move |ctx| {
                let item_list = ctx.into_value();
                let refresh_items = refresh_items.clone();
                glib::idle_add_local_once(move || {
                    refresh_items(item_list);
                });
            }
        });

        store_watcher_guards(&dropdown, vec![selection_guard, items_guard]);

        dropdown.upcast()
    }
}

/// Renders a [`PickerStyle::Radio`] picker as a vertical group of
/// `CheckButton`s. Grouped check buttons adopt the radio appearance and
/// enforce single selection through GTK itself.
fn render_radio_group(env: &Environment, config: PickerConfig) -> Widget {
    let container = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    container.set_accessible_role(gtk4::AccessibleRole::RadioGroup);
    {
        let label = config
            .label
            .resolve(env)
            .accessibility_label()
            .snapshot()
            .to_plain();
        container.update_property(&[gtk4::accessible::Property::Label(label.as_str())]);
    }

    let items = config.items;
    let selection = config.selection;
    let buttons = Rc::new(RefCell::new(Vec::new()));
    let ids = Rc::new(RefCell::new(Vec::new()));

    let rebuild: Rc<dyn Fn(Vec<PickerItem<Id>>)> = {
        let container = container.clone();
        let buttons = buttons.clone();
        let ids = ids.clone();
        let selection = selection.clone();
        let env = env.clone();
        Rc::new(move |item_list| {
            while let Some(child) = container.first_child() {
                container.remove(&child);
            }

            let mut leader: Option<gtk4::CheckButton> = None;
            let mut new_buttons = Vec::new();
            let mut new_ids = Vec::new();
            for item in &item_list {
                let button = gtk4::CheckButton::with_label(&item_label(item, &env));
                if let Some(leader) = &leader {
                    button.set_group(Some(leader));
                } else {
                    leader = Some(button.clone());
                }
                let id = item.tag;
                button.connect_toggled({
                    let selection = selection.clone();
                    move |button| {
                        if button.is_active() && selection.snapshot() != id {
                            selection.set(id);
                        }
                    }
                });
                container.append(&button);
                new_buttons.push(button);
                new_ids.push(id);
            }

            if let Some(index) = new_ids.iter().position(|id| *id == selection.snapshot()) {
                new_buttons[index].set_active(true);
            }
            *buttons.borrow_mut() = new_buttons;
            *ids.borrow_mut() = new_ids;
        })
    };

    rebuild(items.snapshot());

    // Watch binding changes -> activate the matching radio button. GTK's
    // group semantics clear the previous button, and the toggled handler
    // above is a no-op for the already-current selection.
    let selection_guard = selection.computed().watch({
        move |ctx| {
            let value = ctx.into_value();
            let buttons = buttons.clone();
            let ids = ids.clone();
            glib::idle_add_local_once(move || {
                let ids = ids.borrow();
                let buttons = buttons.borrow();
                if let Some(index) = ids.iter().position(|id| *id == value)
                    && index < buttons.len()
                    && !buttons[index].is_active()
                {
                    buttons[index].set_active(true);
                }
            });
        }
    });

    // Watch items changes -> rebuild the group.
    let items_guard = items.watch({
        let rebuild = rebuild.clone();
        move |ctx| {
            let item_list = ctx.into_value();
            let rebuild = rebuild.clone();
            glib::idle_add_local_once(move || {
                rebuild(item_list);
            });
        }
    });

    store_watcher_guards(&container, vec![selection_guard, items_guard]);

    container.upcast()
}
