//! GTK Application setup and lifecycle management.

use std::future::Future;
use std::num::NonZeroU32;

use executor_core::{
    LocalExecutor,
    async_task::{self, AsyncTask, Runnable},
    spawn_local, try_init_global_executor, try_init_local_executor,
};
use gtk4::Application;
use gtk4::prelude::*;
use native_executor::NativeExecutor;
use waterui::app::{App, AppParts, LastWindowPolicy};
use waterui_core::{Environment, View};

use crate::renderer::GtkRenderer;
use crate::util::{store_watcher_guards, subscribe_then_get};
#[cfg(feature = "webview-system")]
use crate::webview::ensure_webview_controller;
use crate::window::{apply_window_background, create_window, install_inspect_gesture};

#[derive(Debug, Clone, Copy, Default)]
struct GtkMainThreadExecutor;

impl LocalExecutor for GtkMainThreadExecutor {
    type Task<T: 'static> = AsyncTask<T>;

    fn spawn_local<Fut>(&self, fut: Fut) -> Self::Task<Fut::Output>
    where
        Fut: Future + 'static,
    {
        // Wakers fire on whatever thread completed the awaited work — GPU init
        // lands on `async-io` driver threads — so the hop back to the main
        // context must be the thread-safe `idle_add_once`; the `*_local`
        // variant asserts the caller already owns the context and panics
        // on foreign threads.
        let (runnable, task) = async_task::spawn_local(fut, |runnable: Runnable| {
            glib::idle_add_once(move || {
                runnable.run();
            });
        });
        runnable.schedule();
        task
    }
}

/// The fastest refresh rate GDK reports on the default display — the frame
/// budget the monitored executor paces main-thread task polls against.
fn display_refresh_rate() -> waterui::task::RefreshRate {
    use waterui::task::RefreshRate;

    let Some(display) = gtk4::gdk::Display::default() else {
        tracing::debug!("no GDK display; executor frame budget uses the headless rate");
        return RefreshRate::HEADLESS;
    };
    let monitors = display.monitors();
    let millihertz = (0..monitors.n_items())
        .filter_map(|index| monitors.item(index))
        .filter_map(|object| object.downcast::<gtk4::gdk::Monitor>().ok())
        .map(|monitor| monitor.refresh_rate())
        .filter(|rate| *rate > 0)
        .max()
        .and_then(|rate| u32::try_from(rate).ok())
        .and_then(NonZeroU32::new);
    // A display that reports no rate paces frames at the nominal 60 Hz.
    millihertz.map_or_else(
        || {
            tracing::debug!(
                "no GDK monitor reports a refresh rate; executor frame budget uses the headless rate"
            );
            RefreshRate::HEADLESS
        },
        RefreshRate::from_millihertz,
    )
}

/// Initialize executors for GTK apps on the main thread.
///
/// Returns the inspector endpoint, which the caller must keep alive and install
/// into the environment: dropping it shuts the endpoint down and withdraws the
/// advertisement that lets `water inspect` find this application.
#[must_use]
pub fn init_main_thread_executors() -> Option<waterui::inspector::InspectorRuntime> {
    // The backend instruments its rendering paths with `tracing`, but a
    // generated application has no subscriber unless one is installed here.
    // `try_init` leaves an application-installed subscriber alone, and the
    // env filter keeps the log quiet unless `RUST_LOG` opts into more.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();

    // GTK apps run UI rendering on the main thread. Initialize executors there so
    // spawn/spawn_local paths used by reactive bindings are always available.
    let _ = try_init_global_executor(NativeExecutor::new());
    let inspector = waterui::inspector::maybe_init_from_env("gtk");
    let inspector_probe = inspector
        .as_ref()
        .map(waterui::inspector::InspectorRuntime::runtime_probe);
    // `activate` fires after `gtk4::init` opened the display, so GDK's
    // monitor refresh rate is queryable here.
    let _ = try_init_local_executor(waterui::task::monitored_local_executor_with_probes(
        GtkMainThreadExecutor,
        display_refresh_rate(),
        inspector_probe,
    ));

    // Locale changes reach views through a mailbox, whose pump needs the
    // executor installed just above.
    waterui_locale::start_system_locale_listener();
    inspector
}

/// Makes the staged application icon resolvable through GTK icon-name
/// lookup.
///
/// The water CLI installs a hicolor icon tree named after the application id
/// next to the staged asset bundle; adding that tree to the display's icon
/// theme search path and using the id as the default window icon name lets
/// GTK pick the right size everywhere the icon appears. Without a staged
/// bundle (bare `cargo run`, tests) the theme simply has no icon with that
/// name and GTK falls back to its generic window icon.
fn install_app_icon(app_id: &str, resources: &waterui_core::ResourceContext) {
    gtk4::Window::set_default_icon_name(app_id);
    let bundle_root = waterui_assets::bundle_root(resources);
    let Some(resources_root) = bundle_root.parent() else {
        return;
    };
    let icons_dir = resources_root.join("icons");
    if !icons_dir.is_dir() {
        return;
    }
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::IconTheme::for_display(&display).add_search_path(icons_dir);
    }
}

/// GTK4 application wrapper for `WaterUI`.
#[derive(Debug)]
pub struct GtkApp {
    app: Application,
}

impl GtkApp {
    /// Creates a new GTK application.
    ///
    /// # Arguments
    ///
    /// * `app_id` - The application identifier (e.g., "com.example.myapp")
    #[must_use]
    pub fn new(app_id: &str) -> Self {
        let app = Application::builder().application_id(app_id).build();

        Self { app }
    }

    /// Runs the application with the provided root view.
    ///
    /// This method blocks until the application exits.
    ///
    /// # Panics
    ///
    /// Panics if the GPU runtime cannot be created.
    #[must_use = "the returned value is the process exit status"]
    pub fn run<V: View + Clone + 'static>(self, view: V, env: Environment) -> i32 {
        let mut env = env;
        waterui_core::install_application_resources(&mut env);
        // GTK draws text with Pango and owns no `parley` collection, so a
        // component that typesets text itself gets the system's fonts here,
        // once for the application rather than once per view.
        waterui_text::install_system_font_collection(&mut env);
        #[cfg(feature = "webview-system")]
        ensure_webview_controller(&mut env);
        let env = env;

        self.app.connect_activate(move |app| {
            if let Some(app_id) = app.application_id() {
                install_app_icon(
                    app_id.as_str(),
                    waterui_core::ResourceContext::from_environment(&env),
                );
            }
            let inspector = init_main_thread_executors();
            let app = app.clone();
            let view = view.clone();
            let mut env = env.clone();
            waterui::inspector::install(&mut env, inspector);
            // `activate` returning with a zero use count shuts the run loop
            // down before the deferred window work runs; hold the application
            // until the window itself holds it via `add_window`.
            let hold = app.hold();
            spawn_local(async move {
                let runtime = waterui_graphics::GpuRuntime::new()
                    .await
                    .unwrap_or_else(|error| panic!("GTK GPU runtime creation failed: {error}"));
                env.insert(runtime);
                let window = create_window(&app, "WaterUI App", 800, 600);
                crate::theme::install(&mut env, window.upcast_ref());
                install_inspect_gesture(&window, &env);
                let mut renderer = GtkRenderer::new();
                let widget = renderer.render(view, &env);
                window.set_child(Some(&widget));
                window.present();
                drop(hold);
            })
            .detach();
        });

        self.app.run().into()
    }

    /// Runs a `WaterUI` `App` as a GTK application.
    ///
    /// The app's first window is rendered with GTK; an app may declare none.
    /// The app's [`LastWindowPolicy`] decides what happens once no window is
    /// open: under [`LastWindowPolicy::Quit`] the application ends with its
    /// last window, and at startup when it declares none; under
    /// [`LastWindowPolicy::StayResident`] it holds the GTK application for as
    /// long as it runs.
    ///
    /// # Panics
    ///
    /// Panics if the GPU runtime cannot be created.
    #[must_use = "the returned value is the process exit status"]
    pub fn run_app(self, waterui_app: App) -> i32 {
        let AppParts {
            windows,
            env,
            last_window,
            ..
        } = waterui_app.into_parts();
        let mut env = env;
        waterui_core::install_application_resources(&mut env);
        // GTK draws text with Pango and owns no `parley` collection, so a
        // component that typesets text itself gets the system's fonts here,
        // once for the application rather than once per view.
        waterui_text::install_system_font_collection(&mut env);
        // Mounts one WaterUI `Window` as a GTK `ApplicationWindow`, pulling
        // the parts the mount needs out of the window's own record — every
        // path that shows a window (the app's declared windows, windows
        // opened later through the `WindowManager`, a window shown again
        // after closing) lands here, so `placement` and `activation`
        // resolve fresh on every show.
        let mount_window = std::rc::Rc::new({
            let app = self.app.clone();
            move |window: &waterui::window::Window, mut env: Environment| {
                let app = app.clone();
                let content = window.content.build();
                let title = window.display_title();
                let background = window.background.clone();
                let state = window.state.clone();
                let attention = window.attention.clone();
                let style = window.style.clone();
                // `WindowPlacement` is not `Clone` (it owns `Rc<dyn Fn>`),
                // but the record stays the owner — the mount clones the
                // shareable `place` handle and the copyable selector.
                let placement =
                    window
                        .placement
                        .as_ref()
                        .map(|placement| waterui::window::WindowPlacement {
                            monitor: placement.monitor,
                            place: std::rc::Rc::clone(&placement.place),
                        });
                let activation = window.activation;
                // See the `hold` rationale in `run`: the window takes over
                // the application reference once it is presented.
                let hold = app.hold();
                spawn_local(async move {
                    let runtime = waterui_graphics::GpuRuntime::new()
                        .await
                        .unwrap_or_else(|error| panic!("GTK GPU runtime creation failed: {error}"));
                    env.insert(runtime);
                    if let Some(app_id) = app.application_id() {
                        install_app_icon(
                            app_id.as_str(),
                            waterui_core::ResourceContext::from_environment(&env),
                        );
                    }
                    let gtk_window = create_window(&app, "", 800, 600);
                    crate::theme::install(&mut env, gtk_window.upcast_ref());
                    install_inspect_gesture(&gtk_window, &env);
                    // Resolved after the theme is installed: an opaque
                    // window paints the theme's background colour.
                    apply_window_background(
                        &gtk_window,
                        &waterui::window::resolve_background(&background, &env),
                    );
                    crate::window::apply_window_style(&gtk_window, &style);
                    // Placement resolves its selector now — mount time —
                    // and only its size applies: GTK4 gives toplevels no
                    // position API.
                    if let Some(placement) = placement.as_ref() {
                        crate::window::apply_window_placement(&gtk_window, placement, &app);
                    }
                    crate::window::apply_window_activation(&gtk_window, activation);
                    crate::window::install_window_state(gtk_window.upcast_ref(), &state);
                    crate::window::install_attention_settle(gtk_window.upcast_ref(), &attention);

                    let (initial_title, title_guard) = subscribe_then_get(&title, {
                        let gtk_window = gtk_window.clone();
                        move |ctx| {
                            let title_text = ctx.into_value().as_str().to_owned();
                            let gtk_window = gtk_window.clone();
                            glib::idle_add_local_once(move || {
                                gtk_window.set_title(Some(&title_text));
                            });
                        }
                    });
                    gtk_window.set_title(Some(initial_title.as_str()));
                    store_watcher_guards(&gtk_window, vec![title_guard]);

                    let mut renderer = GtkRenderer::new();
                    let widget = renderer.render_any(content, &env);
                    gtk_window.set_child(Some(&widget));
                    gtk_window.present();
                    drop(hold);
                })
                .detach();
            }
        });
        // Windows shown after startup — `conditional_window` cycles, second
        // windows — arrive through the `WindowManager` hook the view tree
        // calls `Window::show` on.
        let manager_env = env.clone();
        env.insert(waterui::window::WindowManager::new({
            let mount_window = std::rc::Rc::clone(&mount_window);
            move |window| mount_window(&window, manager_env.clone())
        }));
        #[cfg(feature = "webview-system")]
        ensure_webview_controller(&mut env);

        // GTK ends the application once nothing holds it — no window, no
        // hold — which is exactly `Quit`, at startup included. Staying
        // resident is one hold for as long as the application runs.
        let _resident = match last_window {
            LastWindowPolicy::Quit => None,
            LastWindowPolicy::StayResident => Some(self.app.hold()),
        };

        self.app.connect_activate(move |app| {
            let inspector = init_main_thread_executors();
            let _ = app;
            if windows.is_empty() {
                return;
            }
            let mut env = env.clone();
            waterui::inspector::install(&mut env, inspector);
            // Every declared window mounts here; each one's own `placement`
            // and `activation` apply on its own mount.
            for window in &windows {
                mount_window(window, env.clone());
            }
        });

        self.app.run().into()
    }

    /// Returns a reference to the underlying GTK Application.
    #[must_use]
    pub const fn application(&self) -> &Application {
        &self.app
    }
}

impl Default for GtkApp {
    fn default() -> Self {
        Self::new("com.waterui.app")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A staged bundle puts `icons/` beside `waterui_assets/`; the install
    /// must add that directory to the display's icon theme and name the app
    /// id as the default window icon — the same path `run` and `run_app`
    /// both take.
    #[test]
    fn install_app_icon_registers_the_staged_icons_dir() {
        gtk4::init().expect("GTK tests need a display; run them under xvfb-run");
        let staging = std::env::temp_dir().join(format!("gtk-app-icon-{}", std::process::id()));
        let resources_root = staging.join("resources");
        let icons_dir = resources_root.join("icons");
        let assets_dir = resources_root.join("waterui_assets");
        std::fs::create_dir_all(&icons_dir).unwrap();
        std::fs::create_dir_all(&assets_dir).unwrap();
        let context = waterui_core::ResourceContext::new(&assets_dir, resources_root.join("fonts"));

        install_app_icon("com.example.gtk_test", &context);

        assert_eq!(
            gtk4::Window::default_icon_name().as_deref(),
            Some("com.example.gtk_test")
        );
        let display = gtk4::gdk::Display::default().expect("display");
        assert!(
            gtk4::IconTheme::for_display(&display)
                .search_path()
                .contains(&icons_dir)
        );
        std::fs::remove_dir_all(&staging).ok();
    }
}
