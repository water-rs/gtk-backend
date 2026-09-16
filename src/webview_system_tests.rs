//! Native WebKitGTK checks for the shared asset-origin contract.

use std::time::Duration;

#[test]
fn asset_origin_serves_the_shared_bundled_site() {
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
