//! Real-engine `WebKitGTK` checks for the shared asset-origin contract.
//!
//! `harness = false` puts `main` on the process's real main thread, which GTK
//! and `WebKitGTK` require: their `RunLoop` and IPC machinery bind to the
//! initializing thread, and `WebKit`'s TLS teardown (`RunLoop::threadWillExit`)
//! destructs `WebProcessProxy` → `AuxiliaryProcessProxy` →
//! `IPC::Connection::invalidate`, taking the connection-state lock the IPC
//! receive thread may hold while it dispatches an incoming message onto the
//! dying run loop — an ABBA deadlock. Under libtest every `#[test]` ran on a
//! throwaway worker thread, so the process wedged at worker exit *after*
//! every assertion had already passed — the hang tracked as issue #120.
//! `main` initialises GTK on the main thread and runs each case there
//! sequentially; the main thread exits only with the process, so the
//! deadlock's precondition cannot occur.
//!
//! `cargo nextest` cannot enumerate `harness = false` cases, so CI invokes
//! this binary directly under the same xvfb/dbus wrapper. `main` runs every
//! case even after a failure, prints each case's name and outcome, and exits
//! non-zero when any case fails — a test that cannot run must not look like
//! one that passed.

use std::process::ExitCode;

#[cfg(all(
    feature = "webkitgtk",
    gtk_webkitgtk_link_available,
    unix,
    not(target_os = "macos")
))]
mod imp {
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::time::Duration;

    use waterui_webview::{BackendEvent, WebViewEvent};

    /// Awaits the first backend event matching `wanted`. The watcher feeds an
    /// unbounded channel, so `recv` resolves on the event itself rather than
    /// on a poll interval.
    // `BackendEvent` is not `Send`; this binary is single-threaded, so the
    // future never needs to cross a thread boundary.
    #[allow(clippy::future_not_send)]
    async fn wait_for(
        events: &async_channel::Receiver<BackendEvent>,
        wanted: impl Fn(&BackendEvent) -> bool,
    ) {
        loop {
            match events.recv().await {
                Ok(event) if wanted(&event) => return,
                Ok(_) => {}
                Err(err) => {
                    panic!(
                        "the backend event channel closed before the expected event arrived: {err}"
                    );
                }
            }
        }
    }

    /// `pushState` and in-page anchors add back-forward entries without ever
    /// emitting `load-changed`, so `NavigationState` must come from the
    /// back-forward list's own `changed` signal. Drives exactly that edge.
    ///
    /// The inner async bound covers only work the main context can reach; a
    /// synchronous native stall would hang past it, so CI wraps this binary
    /// in a process watchdog — a teardown abort is observed as a failure,
    /// never masked as a pass.
    fn same_document_history_change_reports_navigation_state() {
        let controller =
            waterui_webview::WebViewController::new(waterui_gtk::webview::GtkWebViewController);
        let webview = controller.open_with(waterui_webview::WebViewConfig {
            asset_server: Some(Arc::new(
                |_request: &waterui_webview::assets::AssetRequest| {
                    waterui_webview::assets::AssetResponse::ok(
                        "text/html",
                        b"<title>history</title>".to_vec(),
                    )
                },
            )),
        });
        let handle = webview.handle().clone();
        let (events_tx, events_rx) = async_channel::unbounded();
        let _guard = handle.watch(move |event| {
            // The receiver may already be gone once teardown events flush
            // through; dropping is fine — the assertion reads only what the
            // bounded section observed.
            let _ = events_tx.try_send(event);
        });
        glib::MainContext::default()
            .block_on(glib::future_with_timeout(
                Duration::from_secs(30),
                async move {
                    handle
                        .run_javascript("'installed'")
                        .await
                        .expect("the engine evaluates on its initial document");
                    handle.go_to(
                        &"waterui://localhost/index.html"
                            .parse()
                            .expect("the asset entry URL parses"),
                    );
                    wait_for(&events_rx, |event| {
                        matches!(event, BackendEvent::Event(WebViewEvent::Loaded))
                    })
                    .await;
                    // Discard anything queued by the load itself: only the
                    // entry `pushState` adds may satisfy the wait below.
                    while events_rx.try_recv().is_ok() {}
                    handle
                        .call_async_javascript("history.pushState({}, '', '/pushed');")
                        .await
                        .expect("pushState succeeds on the asset origin");
                    wait_for(&events_rx, |event| {
                        matches!(
                            event,
                            BackendEvent::NavigationState {
                                can_go_back: true,
                                ..
                            }
                        )
                    })
                    .await;
                },
            ))
            .expect("a same-document history entry must report NavigationState");
        // `webview`, `handle`'s watcher and the dedicated `WebContext` /
        // `WebKitNetworkSession` (and with them the ephemeral
        // `WebsiteDataStore`) drop here, on the main thread.
    }

    fn asset_origin_serves_the_shared_bundled_site() {
        let controller =
            waterui_webview::WebViewController::new(waterui_gtk::webview::GtkWebViewController);
        glib::MainContext::default()
            .block_on(glib::future_with_timeout(
                Duration::from_secs(30),
                waterui_webview::conformance::asset_origin_serves_bundled_content(&controller),
            ))
            .expect("WebKitGTK must finish the bundled-site conformance flow");
    }

    /// Every case, in the order they run.
    const CASES: &[(&str, fn())] = &[
        (
            "asset_origin_serves_the_shared_bundled_site",
            asset_origin_serves_the_shared_bundled_site,
        ),
        (
            "same_document_history_change_reports_navigation_state",
            same_document_history_change_reports_navigation_state,
        ),
    ];

    fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> &str {
        if let Some(message) = payload.downcast_ref::<&'static str>() {
            message
        } else if let Some(message) = payload.downcast_ref::<String>() {
            message
        } else {
            "<non-string panic payload>"
        }
    }

    /// Runs every case on the main thread, in order.
    pub fn run() -> ExitCode {
        // WebKitGTK sandboxes its web and network processes through
        // bubblewrap, which the CI kernel denies
        // (`apparmor_restrict_unprivileged_userns`); the documented escape
        // runs them unsandboxed, the same mode packagers use for test
        // environments.
        //
        // SAFETY: this process has not started GTK, WebKit, or any other
        // thread that could read the environment concurrently.
        unsafe { std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1") };
        gtk4::init().expect("WebKitGTK tests require a display");
        let _inspector = waterui_gtk::init_main_thread_executors();

        let mut failures = Vec::new();
        for &(name, case) in CASES {
            eprintln!("webview_system: {name} — running");
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(case)) {
                Ok(()) => eprintln!("webview_system: {name} — ok"),
                Err(payload) => {
                    eprintln!(
                        "webview_system: {name} — FAILED: {}",
                        panic_message(&payload)
                    );
                    failures.push(name);
                }
            }
            // Queued main-context work — web view disposal, `WebContext` and
            // data-store shutdown — is dispatched, not immediate: flushing it
            // lets each case's teardown finish on the live main thread before
            // the next case starts.
            let context = glib::MainContext::default();
            while context.pending() {
                context.iteration(false);
            }
        }

        if failures.is_empty() {
            eprintln!("webview_system: all {} cases passed", CASES.len());
            ExitCode::SUCCESS
        } else {
            eprintln!(
                "webview_system: {} case(s) failed: {}",
                failures.len(),
                failures.join(", ")
            );
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    #[cfg(all(
        feature = "webkitgtk",
        gtk_webkitgtk_link_available,
        unix,
        not(target_os = "macos")
    ))]
    {
        imp::run()
    }
    #[cfg(not(all(
        feature = "webkitgtk",
        gtk_webkitgtk_link_available,
        unix,
        not(target_os = "macos")
    )))]
    {
        eprintln!("webview_system: WebKitGTK is not linkable in this build; no cases ran");
        ExitCode::SUCCESS
    }
}
