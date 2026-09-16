//! Native `WebKitGTK` checks for the shared asset-origin contract.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use waterui_webview::{BackendEvent, WebViewEvent};

#[test]
fn asset_origin_serves_the_shared_bundled_site() {
    // WebKitGTK sandboxes its web and network processes through bubblewrap,
    // which the CI kernel denies (`apparmor_restrict_unprivileged_userns`);
    // the documented escape runs them unsandboxed, the same mode packagers
    // use for test environments.
    //
    // SAFETY: this test process has not started GTK, WebKit, or any other
    // thread that could read the environment concurrently.
    unsafe { std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1") };
    gtk4::init().expect("WebKitGTK tests require a display");
    let _inspector = crate::init_main_thread_executors();
    let controller = waterui_webview::WebViewController::new(crate::webview::GtkWebViewController);
    glib::MainContext::default()
        .block_on(glib::future_with_timeout(
            Duration::from_secs(30),
            waterui_webview::conformance::asset_origin_serves_bundled_content(&controller),
        ))
        .expect("WebKitGTK must finish the bundled-site conformance flow");
}

/// `pushState` and in-page anchors add back-forward entries without ever
/// emitting `load-changed`, so `NavigationState` must come from the
/// back-forward list's own `changed` signal. Drives exactly that edge.
#[test]
fn same_document_history_change_reports_navigation_state() {
    // SAFETY: same CI sandbox escape as the conformance test above, set before
    // GTK, WebKit, or any concurrent environment reader starts.
    unsafe { std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1") };
    gtk4::init().expect("WebKitGTK tests require a display");
    let _inspector = crate::init_main_thread_executors();
    let controller = waterui_webview::WebViewController::new(crate::webview::GtkWebViewController);
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
    let events = Rc::new(RefCell::new(Vec::<BackendEvent>::new()));
    let _guard = handle.watch({
        let events = Rc::clone(&events);
        move |event| events.borrow_mut().push(event)
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
                loop {
                    if events
                        .borrow()
                        .iter()
                        .any(|event| matches!(event, BackendEvent::Event(WebViewEvent::Loaded)))
                    {
                        break;
                    }
                    glib::timeout_future(Duration::from_millis(10)).await;
                }
                handle
                    .call_async_javascript("history.pushState({}, '', '/pushed');")
                    .await
                    .expect("pushState succeeds on the asset origin");
                loop {
                    if events.borrow().iter().any(|event| {
                        matches!(
                            event,
                            BackendEvent::NavigationState {
                                can_go_back: true,
                                ..
                            }
                        )
                    }) {
                        break;
                    }
                    glib::timeout_future(Duration::from_millis(10)).await;
                }
            },
        ))
        .expect("a same-document history entry must report NavigationState");
}
