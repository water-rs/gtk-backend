//! Native `WebKitGTK` checks for the shared asset-origin contract.

use std::time::Duration;

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
