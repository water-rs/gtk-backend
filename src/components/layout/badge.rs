//! GTK4 Badge component implementation.
//!
//! A badge is a numeric indicator overlaid on top of another view: a zero
//! value renders a small dot, any other value renders a pill with the count.

use gtk4::Widget;
use gtk4::prelude::*;
use nami::{Signal, SignalExt};
use waterui::component::badge::BadgeConfig;
use waterui_core::{Environment, Native};
use waterui_graphics::color::ResolvedColor;

use crate::component::GtkComponent;
use crate::renderer::GtkRenderer;
use crate::util::{ScopedCss, resolved_color_to_css_rgba, store_watcher_guard, subscribe_then_get};

const BADGE_CSS_CLASS: &str = "waterui-badge";

/// Side length of the dot indicator shown for a zero value.
const DOT_SIZE: i32 = 6;

/// Height and minimum width of the labeled pill indicator.
const PILL_SIZE: i32 = 16;

fn badge_declarations(value: i32, color: ResolvedColor) -> String {
    let background = resolved_color_to_css_rgba(color);
    if value == 0 {
        format!("border-radius: 999px; padding: 0; background-color: {background};")
    } else {
        format!(
            "border-radius: 999px; padding: 0 5px; min-width: {PILL_SIZE}px; min-height: {PILL_SIZE}px; font-size: 11px; font-weight: bold; color: #ffffff; background-color: {background};"
        )
    }
}

fn apply_badge(label: &gtk4::Label, css: &ScopedCss, value: i32, color: ResolvedColor) {
    if value == 0 {
        label.set_text("");
        label.set_size_request(DOT_SIZE, DOT_SIZE);
    } else {
        label.set_text(&value.to_string());
        label.set_size_request(-1, -1);
    }
    css.set_declarations(&badge_declarations(value, color));
}

impl GtkComponent for Native<BadgeConfig> {
    /// Renders a `WaterUI` Badge as a `GtkOverlay`: the wrapped content is the
    /// main child so the badge sizes to it, and the indicator is a `GtkLabel`
    /// pinned to the top-trailing corner as an overlay that does not affect
    /// the overlay's own size request.
    fn render(self, env: &Environment, renderer: &mut GtkRenderer) -> Widget {
        let BadgeConfig {
            value,
            content,
            color,
        } = self.into_inner();

        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&renderer.render_any(content.build(), env)));

        let badge = gtk4::Label::new(None);
        badge.set_halign(gtk4::Align::End);
        badge.set_valign(gtk4::Align::Start);
        badge.set_margin_end(-DOT_SIZE / 2);
        badge.set_margin_top(-DOT_SIZE / 2);
        overlay.add_overlay(&badge);

        let css = ScopedCss::attach(
            &badge,
            BADGE_CSS_CLASS,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let env = env.clone();
        let combined = value.zip(&color.map(move |c| c.resolve(&env).get()));
        let (initial, guard) = subscribe_then_get(&combined, {
            let badge = badge.clone();
            let css = css.clone();
            move |ctx| {
                let (value, color) = ctx.into_value();
                let badge = badge.clone();
                let css = css.clone();
                glib::idle_add_local_once(move || apply_badge(&badge, &css, value, color));
            }
        });
        let (value, color) = initial;
        apply_badge(&badge, &css, value, color);
        store_watcher_guard(&overlay, Box::new(guard));

        overlay.upcast()
    }
}
