//! GTK4 Menu component implementation.

use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{MenuButton, Popover, Widget};
use nami::Signal;
use waterui::app::Quit;
use waterui_controls::menu::{
    CommandExt as _, ResolvedCommand, ResolvedMenu, ResolvedMenuItem, Shortcut,
};
use waterui_core::{Environment, Native};

use crate::component::GtkComponent;
use crate::menu_shortcuts::{arm_while_mapped, dispatch_in_popover, shortcut_label};
use crate::renderer::GtkRenderer;
use crate::util::{store_watcher_guard, store_watcher_guards};

impl GtkComponent for Native<ResolvedMenu> {
    fn render(self, env: &Environment, renderer: &mut GtkRenderer) -> Widget {
        let menu = self.into_inner();

        let button = MenuButton::new();
        let label = renderer.render_any(menu.label, env);
        button.set_child(Some(&label));

        let popover = Popover::new();
        button.set_popover(Some(&popover));
        dispatch_in_popover(&popover, env);

        rebuild_menu_popover(&popover, menu.items.snapshot(), env);
        arm_while_mapped(&button, menu.items.clone(), env);

        let guard = menu.items.watch({
            let popover = popover;
            let env = env.clone();
            move |ctx| {
                let items = ctx.into_value();
                let popover = popover.clone();
                let env = env.clone();
                glib::idle_add_local_once(move || {
                    rebuild_menu_popover(&popover, items, &env);
                });
            }
        });

        store_watcher_guard(&button, Box::new(guard));
        button.upcast()
    }
}

pub(crate) fn rebuild_menu_popover(
    popover: &Popover,
    items: Vec<ResolvedMenuItem>,
    env: &Environment,
) {
    let list = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let on_activate: Rc<dyn Fn()> = Rc::new({
        let popover = popover.clone();
        move || popover.popdown()
    });
    append_menu_items(&list, items, env, &on_activate);
    popover.set_child(Some(&list));
}

pub(crate) fn append_menu_items(
    list: &gtk4::Box,
    items: Vec<ResolvedMenuItem>,
    env: &Environment,
    on_activate: &Rc<dyn Fn()>,
) {
    for item in items {
        match item {
            ResolvedMenuItem::Command(command) => {
                append_command_button(list, &command, env, on_activate);
            }
            ResolvedMenuItem::Quit => {
                if let Some(command) = quit_command(env) {
                    append_command_button(list, &command, env, on_activate);
                }
            }
            ResolvedMenuItem::Divider => {
                list.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
            }
            ResolvedMenuItem::Menu(menu) => {
                let button = MenuButton::new();
                let title = gtk4::Label::new(Some(&menu.label.content.snapshot().to_plain()));
                title.set_xalign(0.0);
                button.set_child(Some(&title));
                button.add_css_class("flat");

                let popover = Popover::new();
                button.set_popover(Some(&popover));
                dispatch_in_popover(&popover, env);
                rebuild_menu_popover(&popover, menu.items.snapshot(), env);

                let guard = menu.items.watch({
                    let popover = popover.clone();
                    let env = env.clone();
                    move |ctx| {
                        let items = ctx.into_value();
                        let popover = popover.clone();
                        let env = env.clone();
                        glib::idle_add_local_once(move || {
                            rebuild_menu_popover(&popover, items, &env);
                        });
                    }
                });
                store_watcher_guard(&button, Box::new(guard));
                list.append(&button);
            }
        }
    }
}

/// Renders one resolved command as a flat labelled button, trailed by the
/// accelerator label GTK prints for its shortcut; clicking runs the resolved
/// action and pops the menu down.
fn append_command_button(
    list: &gtk4::Box,
    command: &ResolvedCommand,
    env: &Environment,
    on_activate: &Rc<dyn Fn()>,
) {
    let button = gtk4::Button::new();
    button.add_css_class("flat");
    let title = command.label.content.snapshot().to_plain();
    match &command.shortcut {
        None => button.set_label(&title),
        Some(shortcut) => {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 24);
            let title = gtk4::Label::new(Some(&title));
            title.set_xalign(0.0);
            title.set_hexpand(true);
            let accelerator_text = shortcut_label(shortcut);
            let accelerator = gtk4::Label::new(Some(&accelerator_text));
            accelerator.set_xalign(1.0);
            accelerator.add_css_class("dim-label");
            row.append(&title);
            row.append(&accelerator);
            button.set_child(Some(&row));
            // As GtkModelButton does: the title alone names the row, and
            // the accelerator is announced as its key shortcut.
            button.update_relation(&[gtk4::accessible::Relation::LabelledBy(
                &[title.upcast_ref()],
            )]);
            button.update_property(&[gtk4::accessible::Property::KeyShortcuts(&accelerator_text)]);
        }
    }
    button.set_sensitive(!command.disabled.snapshot());

    let disabled_guard = command.disabled.watch({
        let button = button.clone();
        move |ctx: nami::watcher::Context<bool>| {
            let disabled = ctx.into_value();
            let button = button.clone();
            glib::idle_add_local_once(move || {
                button.set_sensitive(!disabled);
            });
        }
    });

    let action = command.action.clone();
    let env = env.clone();
    let on_activate = on_activate.clone();
    button.connect_clicked(move |_| {
        let () = action.call(&env);
        on_activate();
    });

    store_watcher_guards(&button, vec![disabled_guard]);
    list.append(&button);
}

/// The platform's Quit row for a declared `MenuItem::Quit` — the platform's
/// word, the Ctrl+Q chord, and a cancellable request through `Quit`, so
/// `App::on_quit_request` still decides. `None` where `env` has no `Quit`:
/// hosts without an application quit never install one.
pub(crate) fn quit_command(env: &Environment) -> Option<ResolvedCommand> {
    let quit = env.get::<Quit>()?.clone();
    Some(
        "Quit"
            .action(move || quit.request())
            .shortcut(Shortcut::new("q").command())
            .resolve(env),
    )
}
