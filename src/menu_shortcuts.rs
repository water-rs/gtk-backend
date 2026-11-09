//! Menu command shortcuts (water-rs/gtk-backend#145), dispatched with
//! Hydrolysis' `menu_shortcuts` semantics.
//!
//! Every window owns a [`MenuShortcutRegistry`] and a capture-phase key
//! controller, so a chord is consulted before the focused widget — a text
//! entry included — sees the key, and only the focused window's registry
//! ever sees it. GTK delivers a key pressed while a popover is open to the
//! popover's own surface and never to the window behind it, so every menu
//! popover carries a controller on its window's registry too.
//!
//! Chords come from three kinds of source: the app's `menu_bar` (armed for
//! the window's lifetime and registered first, with no menu-bar surface
//! drawn), a mounted `Menu` (armed while its button is mapped) and an open
//! context or text-selection menu (armed while its popover is open, which a
//! fired chord closes). The most recently registered source wins a
//! conflict, and a disabled command claims its chord without firing.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use glib::translate::FromGlib as _;
use gtk4::prelude::*;
use nami::{Computed, Signal as _};
use waterui_controls::menu::{
    Menu, ResolvedCommand, ResolvedMenuItem, Shortcut, resolve_menu_bar_items,
};
use waterui_core::handler::SharedAction;
use waterui_core::{Environment, Str};

use crate::components::menu::quit_command;

const MISSING_MENU_SHORTCUT_REGISTRY: &str = "menu shortcuts require the window's environment to \
     carry a MenuShortcutRegistry: GtkApp::run and GtkApp::run_app install one per window, and a \
     host rendering into its own gtk4::Window calls waterui_gtk::install_menu_shortcuts first";

/// A [`Shortcut`] as the GDK keyval and modifier mask GTK matches key events
/// against and prints accelerator labels for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accelerator {
    /// The lowercase keyval of the shortcut's key: a chord matches its key
    /// case-insensitively, Shift being a modifier of its own.
    pub key: gdk4::Key,
    /// The command modifier is Linux's menu accelerator, Ctrl — the same
    /// mask an explicit control modifier sets.
    pub modifiers: gdk4::ModifierType,
}

impl Accelerator {
    /// The chord `shortcut` arms, `None` when its key is not exactly one
    /// character. Hydrolysis dispatches only character keys, so a named key
    /// such as `"Delete"` or `"Enter"` arms nothing there either; what a
    /// named key means is not settled by the framework, and its name is
    /// deliberately not resolved through GDK.
    pub fn from_shortcut(shortcut: &Shortcut) -> Option<Self> {
        let mut characters = shortcut.key.chars();
        let (Some(character), None) = (characters.next(), characters.next()) else {
            return None;
        };
        // SAFETY: a GDK keyval is a plain `guint` that `Key` wraps without
        // further invariants, and `gdk_unicode_to_keyval` returns a keyval
        // for every code point.
        let key = unsafe { gdk4::Key::from_glib(gdk4::unicode_to_keyval(u32::from(character))) }
            .to_lower();
        let flags = shortcut.modifiers;
        let mut modifiers = gdk4::ModifierType::empty();
        modifiers.set(
            gdk4::ModifierType::CONTROL_MASK,
            flags.control() || flags.command(),
        );
        modifiers.set(gdk4::ModifierType::ALT_MASK, flags.option());
        modifiers.set(gdk4::ModifierType::SHIFT_MASK, flags.shift());
        Some(Self { key, modifiers })
    }

    /// The accelerator text GTK prints for this chord, e.g. `Ctrl+Q`.
    pub fn label(self) -> glib::GString {
        gtk4::accelerator_get_label(self.key, self.modifiers)
    }
}

/// The text a menu row shows for `shortcut`: GTK's accelerator label for a
/// chord, and for a key that arms none the hint Hydrolysis renders on
/// Linux — its modifiers, then the key text uppercased (`Ctrl+DELETE`).
pub fn shortcut_label(shortcut: &Shortcut) -> String {
    if let Some(accelerator) = Accelerator::from_shortcut(shortcut) {
        return accelerator.label().into();
    }
    let flags = shortcut.modifiers;
    let mut hint = String::new();
    if flags.control() || flags.command() {
        hint.push_str("Ctrl+");
    }
    if flags.option() {
        hint.push_str("Alt+");
    }
    if flags.shift() {
        hint.push_str("Shift+");
    }
    hint.push_str(&shortcut.key.to_uppercase());
    hint
}

/// Arms menu command shortcuts on `window`.
///
/// Installs the window's menu shortcut registry into `env` — the
/// environment the window's content renders under — and the capture-phase
/// key controller that dispatches on it. A fired command runs under its
/// menu's environment layered over this one, so call it once `env` carries
/// every window-scoped value, the theme included: an app `menu_bar` action
/// then sees the window it fired in. Rendering a `Menu`, or opening a
/// context or text-selection menu, under an environment without a registry
/// panics.
///
/// # Panics
///
/// Panics when `env` already carries a registry: each window owns its own.
pub fn install_menu_shortcuts(window: &gtk4::Window, env: &mut Environment) {
    assert!(
        env.get::<MenuShortcutRegistry>().is_none(),
        "install_menu_shortcuts called with an environment that already carries a window's \
         MenuShortcutRegistry; each window installs its own from the app environment"
    );
    env.insert(MenuShortcutRegistry::default());
    window.add_controller(chord_controller(env.clone()));
}

/// Dispatches the chords of the window whose registry `env` carries while
/// `popover` — a menu's popover, open over that window — holds the keyboard.
/// `env` is the menu's, which carries the window's own environment.
pub fn dispatch_in_popover(popover: &gtk4::Popover, env: &Environment) {
    popover.add_controller(chord_controller(env.clone()));
}

/// The key controller dispatching on `window_env`'s registry; the fired
/// command's source environment is layered over `window_env`.
fn chord_controller(window_env: Environment) -> gtk4::EventControllerKey {
    let registry = registry(&window_env).clone();
    let controller = gtk4::EventControllerKey::new();
    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    controller.connect_key_pressed({
        move |controller, _, _, _| {
            let event = controller
                .current_event()
                .and_then(|event| event.downcast::<gdk4::KeyEvent>().ok())
                .expect("GtkEventControllerKey emits key-pressed while dispatching a GdkKeyEvent");
            let claimed = registry.dispatch(&window_env, |accelerator| {
                event.matches(accelerator.key, accelerator.modifiers) == gdk4::KeyMatch::Exact
            });
            if claimed {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    });
    controller
}

/// Arms the app's `menu_bar` chords in the window whose registry
/// `window_env` carries. `app_env` resolves the menus, and their actions run
/// with it layered over the dispatching window's environment; it must not
/// carry the window's registry, since this source lives as long as the
/// registry itself.
pub fn arm_menu_bar(
    window_env: &Environment,
    menu_bar: &Computed<Vec<Menu>>,
    app_env: &Environment,
) {
    let _app_bar = registry(window_env).register(
        SourceItems::Live(resolve_menu_bar_items(menu_bar, app_env)),
        app_env.clone(),
        None,
    );
}

/// Arms a mounted `Menu`'s chords while `widget` is mapped: mapping
/// registers the source, unmapping — which unmounting always does —
/// unregisters it. The items are read at dispatch, so item edits apply.
pub fn arm_while_mapped(
    widget: &impl IsA<gtk4::Widget>,
    items: Computed<Vec<ResolvedMenuItem>>,
    env: &Environment,
) {
    let registry = registry(env).clone();
    let armed: Rc<Cell<Option<SourceId>>> = Rc::default();
    widget.connect_map({
        let registry = registry.clone();
        let armed = armed.clone();
        let env = env.clone();
        move |_| {
            let id = registry.register(SourceItems::Live(items.clone()), env.clone(), None);
            assert!(
                armed.replace(Some(id)).is_none(),
                "a mounted menu mapped again without unmapping"
            );
        }
    });
    widget.connect_unmap(move |_| {
        let id = armed
            .take()
            .expect("a mounted menu unmapped without having been mapped");
        registry.unregister(id);
    });
}

/// Arms a context or text-selection menu's chords while `popover` is open,
/// and a fired chord pops it down first. Open means mapped: every map arms
/// the items and every unmap disarms them. GTK emits `closed` only when the
/// popover itself hides, not when an unmapping anchor takes the popover
/// down with it, and that anchor's remap maps the popover again.
///
/// # Panics
///
/// Panics when `popover` is already mapped: call this before `popup()`,
/// which maps it synchronously under a mapped anchor.
pub fn arm_while_open(popover: &gtk4::Popover, items: Vec<ResolvedMenuItem>, env: &Environment) {
    assert!(
        !popover.is_mapped(),
        "arm_while_open must run before the menu popover pops up, so its first map arms it"
    );
    dispatch_in_popover(popover, env);
    let registry = registry(env).clone();
    let items: Rc<[ResolvedMenuItem]> = items.into();
    let armed: Rc<Cell<Option<SourceId>>> = Rc::default();
    popover.connect_map({
        let registry = registry.clone();
        let armed = armed.clone();
        let env = env.clone();
        move |popover| {
            let id = registry.register(
                SourceItems::Open(items.clone()),
                env.clone(),
                Some(popover.clone()),
            );
            assert!(
                armed.replace(Some(id)).is_none(),
                "a menu popover mapped again without unmapping"
            );
        }
    });
    popover.connect_unmap(move |_| {
        let id = armed
            .take()
            .expect("a menu popover unmapped without having been mapped");
        registry.unregister(id);
    });
}

fn registry(env: &Environment) -> &MenuShortcutRegistry {
    env.get::<MenuShortcutRegistry>()
        .expect(MISSING_MENU_SHORTCUT_REGISTRY)
}

/// One window's chord sources, in registration order.
#[derive(Clone, Default)]
pub struct MenuShortcutRegistry(Rc<RefCell<RegistryState>>);

#[derive(Default)]
struct RegistryState {
    sources: Vec<Source>,
    next_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceId(u64);

/// A source's menu items, the environment its commands resolved under and
/// run with, and — for an open menu — the popover a fired chord closes.
struct Source {
    id: SourceId,
    items: SourceItems,
    env: Environment,
    popover: Option<gtk4::Popover>,
}

#[derive(Clone)]
enum SourceItems {
    /// A mounted `Menu` or the app's `menu_bar`, read at dispatch.
    Live(Computed<Vec<ResolvedMenuItem>>),
    /// An open menu's items, as they were when it opened.
    Open(Rc<[ResolvedMenuItem]>),
}

/// One command's chord: what it runs, whether it is disabled now, and the
/// label a conflict warning names it by.
struct Chord {
    accelerator: Accelerator,
    action: SharedAction<()>,
    disabled: Computed<bool>,
    label: Str,
}

impl MenuShortcutRegistry {
    fn register(
        &self,
        items: SourceItems,
        env: Environment,
        popover: Option<gtk4::Popover>,
    ) -> SourceId {
        let mut state = self.0.borrow_mut();
        let id = SourceId(state.next_id);
        state.next_id += 1;
        state.sources.push(Source {
            id,
            items,
            env,
            popover,
        });
        id
    }

    fn unregister(&self, id: SourceId) {
        let mut state = self.0.borrow_mut();
        let index = state
            .sources
            .iter()
            .position(|source| source.id == id)
            .unwrap_or_else(|| panic!("menu shortcut source {id:?} is not registered"));
        state.sources.remove(index);
    }

    /// Dispatches a key press in the window whose environment is
    /// `window_env`, `matches` deciding which accelerators it is. Returns
    /// whether a chord claimed it — fired or, when its command is disabled,
    /// not.
    fn dispatch(&self, window_env: &Environment, matches: impl Fn(Accelerator) -> bool) -> bool {
        // Copied out, newest first, so no borrow is held while user code
        // runs: reading a mounted menu's items runs its map closures and
        // building the Quit command resolves it, then popping a popover
        // down unregisters it and an action may mount or unmount menus.
        let sources: Vec<(SourceItems, Environment, Option<gtk4::Popover>)> = self
            .0
            .borrow()
            .sources
            .iter()
            .rev()
            .map(|source| {
                (
                    source.items.clone(),
                    source.env.clone(),
                    source.popover.clone(),
                )
            })
            .collect();
        let mut winner: Option<(Chord, Environment, Option<gtk4::Popover>)> = None;
        let mut losers: Vec<Str> = Vec::new();
        for (items, env, popover) in sources {
            let mut chords = Vec::new();
            match items {
                SourceItems::Live(items) => collect_chords(&items.snapshot(), &env, &mut chords),
                SourceItems::Open(items) => collect_chords(&items, &env, &mut chords),
            }
            for chord in chords {
                if !matches(chord.accelerator) {
                    continue;
                }
                if winner.is_some() {
                    losers.push(chord.label);
                } else {
                    winner = Some((chord, env.clone(), popover.clone()));
                }
            }
        }
        let Some((chord, env, popover)) = winner else {
            return false;
        };
        if !losers.is_empty() {
            tracing::warn!(
                chord = %chord.accelerator.label(),
                winner = %chord.label,
                losers = ?losers,
                "menu shortcut chord registered by multiple menus; the most recently registered wins",
            );
        }
        if chord.disabled.snapshot() {
            return true;
        }
        if let Some(popover) = popover {
            popover.popdown();
        }
        let () = chord.action.call(&env.layered_on(window_env));
        true
    }
}

/// Flattens menu items into their chords, nested menus included. A
/// declared Quit arms the chord of [`quit_command`], and nothing where
/// `env` has no application quit.
fn collect_chords(items: &[ResolvedMenuItem], env: &Environment, out: &mut Vec<Chord>) {
    for item in items {
        match item {
            ResolvedMenuItem::Command(command) => collect_command_chord(command, out),
            ResolvedMenuItem::Quit => {
                if let Some(command) = quit_command(env) {
                    collect_command_chord(&command, out);
                }
            }
            ResolvedMenuItem::Menu(menu) => collect_chords(&menu.items.snapshot(), env, out),
            ResolvedMenuItem::Divider => {}
        }
    }
}

fn collect_command_chord(command: &ResolvedCommand, out: &mut Vec<Chord>) {
    if let Some(accelerator) = command
        .shortcut
        .as_ref()
        .and_then(Accelerator::from_shortcut)
    {
        out.push(Chord {
            accelerator,
            action: command.action.clone(),
            disabled: command.disabled.clone(),
            label: command.label.content.snapshot().to_plain(),
        });
    }
}

#[cfg(test)]
mod tests {
    //! Conversion, row labels and dispatch semantics. GDK key events are not
    //! constructible, so dispatch is driven with the accelerator a press
    //! matches; `windowed_tests` covers GTK's own key delivery.

    use std::cell::Cell;
    use std::rc::Rc;

    use gdk4::ModifierType;
    use waterui::app::{App, TerminationHost};
    use waterui_controls::menu::{CommandExt as _, MenuItem};

    use super::*;
    use crate::components::menu::rebuild_menu_popover;

    fn init() {
        gtk4::init().expect("GTK tests need a display; run them under xvfb-run");
    }

    fn accelerator(shortcut: &Shortcut) -> Accelerator {
        Accelerator::from_shortcut(shortcut)
            .unwrap_or_else(|| panic!("{:?} arms a chord", shortcut.key.as_str()))
    }

    fn ctrl(key: &'static str) -> Accelerator {
        accelerator(&Shortcut::new(key).command())
    }

    fn press(registry: &MenuShortcutRegistry, pressed: Accelerator) -> bool {
        registry.dispatch(&Environment::new(), |accelerator| accelerator == pressed)
    }

    fn counting_command(
        label: &'static str,
        shortcut: Shortcut,
        fired: &Rc<Cell<u32>>,
    ) -> waterui_controls::menu::Command {
        let fired = fired.clone();
        label
            .action(move || fired.set(fired.get() + 1))
            .shortcut(shortcut)
    }

    fn live(items: Vec<ResolvedMenuItem>) -> SourceItems {
        SourceItems::Live(Computed::constant(items))
    }

    #[test]
    fn command_is_ctrl_on_linux() {
        init();
        let quit = ctrl("q");
        assert_eq!(
            quit,
            Accelerator {
                key: gdk4::Key::q,
                modifiers: ModifierType::CONTROL_MASK,
            }
        );
        assert_eq!(
            quit.label(),
            gtk4::accelerator_get_label(gdk4::Key::q, ModifierType::CONTROL_MASK)
        );
    }

    #[test]
    fn shortcut_modifiers_map_to_gdk_masks() {
        init();
        let cases = [
            (Shortcut::new("k"), ModifierType::empty()),
            (Shortcut::new("k").control(), ModifierType::CONTROL_MASK),
            (Shortcut::new("k").option(), ModifierType::ALT_MASK),
            (Shortcut::new("k").shift(), ModifierType::SHIFT_MASK),
            (
                Shortcut::new("k").command().control(),
                ModifierType::CONTROL_MASK,
            ),
            (
                Shortcut::new("k").command().shift().option(),
                ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK | ModifierType::ALT_MASK,
            ),
        ];
        for (shortcut, modifiers) in cases {
            assert_eq!(
                accelerator(&shortcut),
                Accelerator {
                    key: gdk4::Key::k,
                    modifiers,
                },
                "{shortcut:?}"
            );
        }
    }

    #[test]
    fn shortcut_keys_map_to_lowercase_keyvals() {
        init();
        let cases = [
            ("Q", gdk4::Key::q),
            ("1", gdk4::Key::_1),
            (",", gdk4::Key::comma),
            (" ", gdk4::Key::space),
        ];
        for (key, keyval) in cases {
            assert_eq!(ctrl(key).key, keyval, "{key:?}");
        }
    }

    /// As on Hydrolysis: a named key arms nothing, whatever the name, and
    /// its row shows the key text uppercased after its modifiers.
    #[test]
    fn a_named_key_arms_no_chord_and_shows_its_key_text() {
        init();
        let env = Environment::new();
        let fired = Rc::new(Cell::new(0));
        let items = vec![
            ResolvedMenuItem::Command(
                counting_command("Remove", Shortcut::new("Delete").command(), &fired).resolve(&env),
            ),
            ResolvedMenuItem::Command(
                counting_command("Confirm", Shortcut::new("Enter").shift(), &fired).resolve(&env),
            ),
        ];
        for key in ["Delete", "Enter", ""] {
            assert_eq!(
                Accelerator::from_shortcut(&Shortcut::new(key)),
                None,
                "{key:?}"
            );
        }

        let registry = MenuShortcutRegistry::default();
        let _source = registry.register(SourceItems::Open(items.clone().into()), env.clone(), None);
        assert!(
            !registry.dispatch(&env, |_| true),
            "no chord is armed for a named key"
        );
        assert_eq!(fired.get(), 0);

        let popover = gtk4::Popover::new();
        rebuild_menu_popover(&popover, items, &env);
        let mut labels = Vec::new();
        label_texts(popover.upcast_ref(), &mut labels);
        assert_eq!(labels, ["Remove", "Ctrl+DELETE", "Confirm", "Shift+ENTER"]);
    }

    fn label_texts(widget: &gtk4::Widget, out: &mut Vec<String>) {
        if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
            out.push(label.text().into());
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            label_texts(&current, out);
            child = current.next_sibling();
        }
    }

    fn buttons(widget: &gtk4::Widget, out: &mut Vec<gtk4::Button>) {
        if let Some(button) = widget.downcast_ref::<gtk4::Button>() {
            out.push(button.clone());
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            buttons(&current, out);
            child = current.next_sibling();
        }
    }

    /// The accelerator label is compared against GTK's own output, which is
    /// translated: "Ctrl+O" only under an English or C locale.
    #[test]
    fn a_command_row_shows_the_label_gtk_prints_for_its_chord() {
        init();
        let env = Environment::new();
        let fired = Rc::new(Cell::new(0));
        let popover = gtk4::Popover::new();
        rebuild_menu_popover(
            &popover,
            vec![
                ResolvedMenuItem::Command(
                    counting_command("Open", Shortcut::new("o").command(), &fired).resolve(&env),
                ),
                ResolvedMenuItem::Command("Plain".action(|| {}).resolve(&env)),
            ],
            &env,
        );
        let ctrl_o = gtk4::accelerator_get_label(gdk4::Key::o, ModifierType::CONTROL_MASK);
        let mut labels = Vec::new();
        label_texts(popover.upcast_ref(), &mut labels);
        assert_eq!(labels, ["Open", ctrl_o.as_str(), "Plain"]);

        let mut rows = Vec::new();
        buttons(popover.upcast_ref(), &mut rows);
        let [open, plain] = rows.as_slice() else {
            panic!("expected two command rows, found {}", rows.len());
        };
        assert!(
            gtk4::test_accessible_has_relation(open, gtk4::AccessibleRelation::LabelledBy),
            "the title label alone names a row with a shortcut"
        );
        assert!(
            gtk4::test_accessible_has_property(open, gtk4::AccessibleProperty::KeyShortcuts),
            "the accelerator is announced as the row's key shortcut"
        );
        assert!(!gtk4::test_accessible_has_property(
            plain,
            gtk4::AccessibleProperty::KeyShortcuts
        ));
    }

    #[test]
    fn a_disabled_command_claims_its_chord_without_firing() {
        init();
        let env = Environment::new();
        let registry = MenuShortcutRegistry::default();
        let fired = Rc::new(Cell::new(0));
        let disabled = nami::binding(true);
        let save = counting_command("Save", Shortcut::new("s").command(), &fired)
            .disabled(disabled.clone())
            .resolve(&env);
        let _source = registry.register(live(vec![ResolvedMenuItem::Command(save)]), env, None);

        assert!(press(&registry, ctrl("s")), "a disabled command claims");
        assert_eq!(fired.get(), 0, "a disabled command does not fire");
        assert!(!press(&registry, ctrl("x")), "an unarmed chord passes");

        disabled.set(false);
        assert!(press(&registry, ctrl("s")));
        assert_eq!(fired.get(), 1, "the re-enabled command fires");
    }

    #[test]
    fn the_most_recently_registered_source_wins_a_conflict() {
        init();
        let env = Environment::new();
        let registry = MenuShortcutRegistry::default();
        let first = Rc::new(Cell::new(0));
        let second = Rc::new(Cell::new(0));
        let older = registry.register(
            live(vec![ResolvedMenuItem::Command(
                counting_command("First", Shortcut::new("k").command(), &first).resolve(&env),
            )]),
            env.clone(),
            None,
        );
        let newer = registry.register(
            live(vec![ResolvedMenuItem::Command(
                counting_command("Second", Shortcut::new("k").control(), &second).resolve(&env),
            )]),
            env,
            None,
        );

        assert!(press(&registry, ctrl("k")));
        assert_eq!((first.get(), second.get()), (0, 1), "the newer source wins");

        registry.unregister(newer);
        assert!(press(&registry, ctrl("k")));
        assert_eq!(
            (first.get(), second.get()),
            (1, 1),
            "unregistering hands the chord back"
        );

        registry.unregister(older);
        assert!(!press(&registry, ctrl("k")), "no source arms the chord");
    }

    #[test]
    fn a_mounted_menu_wins_the_app_bars_chord_while_mounted() {
        init();
        let registry = MenuShortcutRegistry::default();
        let mut window_env = Environment::new();
        window_env.insert(registry.clone());
        let bar = Rc::new(Cell::new(0));
        let mounted = Rc::new(Cell::new(0));
        let menu_bar = Computed::constant(vec![Menu::new(
            "App",
            counting_command("Close", Shortcut::new("w").control(), &bar),
        )]);
        arm_menu_bar(&window_env, &menu_bar, &Environment::new());

        assert!(press(&registry, ctrl("w")));
        assert_eq!(
            bar.get(),
            1,
            "the app bar's chord fires with no Menu mounted"
        );

        let menu = registry.register(
            live(vec![ResolvedMenuItem::Command(
                counting_command("Close Tab", Shortcut::new("w").command(), &mounted)
                    .resolve(&window_env),
            )]),
            window_env.clone(),
            None,
        );
        assert!(press(&registry, ctrl("w")));
        assert_eq!((bar.get(), mounted.get()), (1, 1), "the mounted menu wins");

        registry.unregister(menu);
        assert!(press(&registry, ctrl("w")));
        assert_eq!((bar.get(), mounted.get()), (2, 1), "the app bar resumes");
    }

    /// A window-scoped value, inserted after the app env the menu bar
    /// resolves under.
    #[derive(Clone)]
    struct WindowMark(&'static str);

    #[test]
    fn a_menu_bar_action_runs_over_the_dispatching_windows_environment() {
        init();
        let app_env = Environment::new();
        let mut window_env = app_env.clone();
        window_env.insert(MenuShortcutRegistry::default());
        window_env.insert(WindowMark("main"));
        let seen: Rc<Cell<Option<&'static str>>> = Rc::default();
        let menu_bar = Computed::constant(vec![Menu::new(
            "App",
            "Mark"
                .action({
                    let seen = seen.clone();
                    move |waterui_core::extract::Use(mark): waterui_core::extract::Use<
                        WindowMark,
                    >| seen.set(Some(mark.0))
                })
                .shortcut(Shortcut::new("m").command()),
        )]);
        arm_menu_bar(&window_env, &menu_bar, &app_env);

        let pressed = ctrl("m");
        assert!(registry(&window_env).dispatch(&window_env, |accelerator| accelerator == pressed));
        assert_eq!(
            seen.get(),
            Some("main"),
            "the action extracts the window's value"
        );
    }

    #[derive(Clone, Default)]
    struct TerminateCount(Rc<Cell<u32>>);

    impl TerminationHost for TerminateCount {
        fn terminate(&self) {
            self.0.set(self.0.get() + 1);
        }

        fn refuse(&self) {}
    }

    #[test]
    fn a_declared_quit_chord_requests_termination() {
        init();
        let terminated = TerminateCount::default();
        let mut app_env = Environment::new();
        let _machine = App::new_with_windows(Vec::new(), Environment::new())
            .into_parts()
            .termination
            .start(&mut app_env, terminated.clone());
        let registry = MenuShortcutRegistry::default();
        let mut window_env = Environment::new();
        window_env.insert(registry.clone());
        let menu_bar = Computed::constant(vec![Menu::new("App", MenuItem::Quit)]);
        arm_menu_bar(&window_env, &menu_bar, &app_env);

        assert!(press(&registry, ctrl("q")));
        assert_eq!(
            terminated.0.get(),
            1,
            "Ctrl+Q reaches the termination machine"
        );
    }

    #[test]
    fn a_declared_quit_arms_no_chord_where_nothing_can_quit() {
        init();
        let registry = MenuShortcutRegistry::default();
        let mut window_env = Environment::new();
        window_env.insert(registry.clone());
        let menu_bar = Computed::constant(vec![Menu::new("App", MenuItem::Quit)]);
        arm_menu_bar(&window_env, &menu_bar, &Environment::new());

        assert!(!press(&registry, ctrl("q")));
    }
}

#[cfg(test)]
mod windowed_tests {
    //! GTK's own key delivery, through real XTEST key presses (`xdotool
    //! key`): the window's capture-phase controller sees a chord while an
    //! entry holds focus, only in the focused window, and only while the
    //! source is armed. The `windowed` test group serializes these, since
    //! they need the display's single input focus.

    use std::cell::Cell;
    use std::process::Command;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use glib::MainContext;
    use waterui_controls::menu::CommandExt as _;

    use super::*;
    use crate::components::menu::rebuild_menu_popover;
    use crate::renderer::GtkRenderer;

    fn init() {
        gtk4::init().expect("GTK tests need a display; run them under xvfb-run");
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let context = MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for GTK state");
            if !context.iteration(false) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Ctrl+M on its own counting source in the window whose registry `env`
    /// carries, so a press that must do nothing can be awaited: X delivers
    /// key presses in order, so once the sentinel fires, every earlier press
    /// has been handled.
    struct Sentinel(Rc<Cell<u32>>);

    impl Sentinel {
        fn arm(env: &Environment) -> Self {
            let fired = Rc::new(Cell::new(0));
            let item = ResolvedMenuItem::Command(
                "Sentinel"
                    .action(counter_action(&fired))
                    .shortcut(Shortcut::new("m").command())
                    .resolve(env),
            );
            let _source =
                registry(env).register(SourceItems::Open(Rc::from([item])), env.clone(), None);
            Self(fired)
        }

        /// Presses `chord` and returns once the window has handled it.
        fn press_and_drain(&self, chord: &str) {
            let before = self.0.get();
            press_chord(chord);
            press_chord("ctrl+m");
            wait_until(|| self.0.get() == before + 1);
        }
    }

    fn press_chord(chord: &str) {
        let status = Command::new("xdotool")
            .args(["key", chord])
            .status()
            .expect("xdotool must be on PATH to synthesize key input");
        assert!(status.success(), "xdotool key {chord} failed");
    }

    fn toplevel_focused(window: &gtk4::Window) -> bool {
        window
            .surface()
            .and_then(|surface| surface.downcast::<gdk4::Toplevel>().ok())
            .is_some_and(|toplevel| toplevel.state().contains(gdk4::ToplevelState::FOCUSED))
    }

    /// Presents `window` and hands it the input focus. Openbox's focus
    /// stealing prevention leaves a second toplevel unfocused, so the
    /// window is activated by its unique `title` through EWMH.
    fn activate(window: &gtk4::Window, title: &str) {
        window.set_title(Some(title));
        window.present();
        wait_until(|| window.is_mapped());
        let pattern = format!("^{title}$");
        let status = Command::new("xdotool")
            .args([
                "search",
                "--sync",
                "--name",
                &pattern,
                "windowactivate",
                "--sync",
            ])
            .status()
            .expect("xdotool must be on PATH to activate the window");
        assert!(status.success(), "xdotool could not activate {title:?}");
        wait_until(|| toplevel_focused(window));
    }

    fn counter_action(fired: &Rc<Cell<u32>>) -> impl Fn() + 'static {
        let fired = fired.clone();
        move || fired.set(fired.get() + 1)
    }

    fn armed_sources(env: &Environment) -> usize {
        registry(env).0.borrow().sources.len()
    }

    #[test]
    fn a_mounted_menu_chord_fires_in_its_focused_window_until_unmounted() {
        init();
        let fired = Rc::new(Cell::new(0));
        let window = gtk4::Window::new();
        let mut env = Environment::new();
        install_menu_shortcuts(&window, &mut env);
        crate::theme::install(&mut env, window.upcast_ref());
        let menu = Menu::new(
            "File",
            "Open"
                .action(counter_action(&fired))
                .shortcut(Shortcut::new("k").command()),
        );
        let menu_widget = GtkRenderer::new().render(menu, &env);
        let entry = gtk4::Entry::new();
        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        column.append(&menu_widget);
        column.append(&entry);
        window.set_child(Some(&column));
        activate(&window, "menu-shortcuts-main");
        entry.grab_focus();
        // `gtk4::Entry` delegates focus to its private `GtkText`.
        wait_until(|| entry.state_flags().contains(gtk4::StateFlags::FOCUS_WITHIN));

        press_chord("ctrl+k");
        wait_until(|| fired.get() == 1);

        let other = gtk4::Window::new();
        let mut other_env = Environment::new();
        install_menu_shortcuts(&other, &mut other_env);
        other.set_child(Some(&gtk4::Entry::new()));
        let other_sentinel = Sentinel::arm(&other_env);
        activate(&other, "menu-shortcuts-other");
        other_sentinel.press_and_drain("ctrl+k");
        assert_eq!(
            fired.get(),
            1,
            "another window's focus does not fire the chord"
        );

        other.close();
        activate(&window, "menu-shortcuts-main");
        press_chord("ctrl+k");
        wait_until(|| fired.get() == 2);

        column.remove(&menu_widget);
        assert_eq!(armed_sources(&env), 0, "unmounting disarms the menu");
        Sentinel::arm(&env).press_and_drain("ctrl+k");
        assert_eq!(fired.get(), 2, "an unmounted menu's chord does not fire");
        window.close();
    }

    #[test]
    fn an_open_menu_chord_fires_only_while_open_and_closes_it() {
        init();
        let fired = Rc::new(Cell::new(0));
        let window = gtk4::Window::new();
        let mut env = Environment::new();
        install_menu_shortcuts(&window, &mut env);
        let sentinel = Sentinel::arm(&env);
        let anchor = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        anchor.set_size_request(200, 200);
        window.set_child(Some(&anchor));
        activate(&window, "menu-shortcuts-main");
        let items = vec![ResolvedMenuItem::Command(
            "Copy"
                .action(counter_action(&fired))
                .shortcut(Shortcut::new("j").command())
                .resolve(&env),
        )];

        sentinel.press_and_drain("ctrl+j");
        assert_eq!(fired.get(), 0, "a closed context menu arms nothing");

        let popover = gtk4::Popover::new();
        popover.set_parent(&anchor);
        rebuild_menu_popover(&popover, items.clone(), &env);
        arm_while_open(&popover, items, &env);
        popover.popup();
        wait_until(|| popover.is_mapped());
        assert_eq!(armed_sources(&env), 2, "mapping arms the open menu");

        // An unmapping anchor takes the open popover down without `closed`,
        // and its remap maps the popover again.
        anchor.set_visible(false);
        wait_until(|| !popover.is_mapped());
        assert_eq!(armed_sources(&env), 1, "an unmapped menu is disarmed");
        anchor.set_visible(true);
        wait_until(|| popover.is_mapped());
        assert_eq!(armed_sources(&env), 2, "a remapped menu is armed again");

        press_chord("ctrl+j");
        wait_until(|| fired.get() == 1);
        wait_until(|| !popover.is_visible());
        assert_eq!(armed_sources(&env), 1, "closing disarms the menu");

        sentinel.press_and_drain("ctrl+j");
        assert_eq!(
            fired.get(),
            1,
            "a closed context menu's chord does not fire"
        );
        popover.unparent();
        window.close();
    }
}
