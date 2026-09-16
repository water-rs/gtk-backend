//! GTK4 Text component implementation.

use gtk4::prelude::*;
use gtk4::{Label, Widget};
use nami::Signal;
use std::fmt::Write;
use waterui::theme::{color::Foreground, installed_color_signal};
use waterui_core::layout::HorizontalAlignment;
use waterui_core::{Environment, Native};
use waterui_graphics::color::ResolvedColor;
use waterui_text::TextConfig;
use waterui_text::font::{FontDesign, FontWeight, ResolvedFont};
use waterui_text::styled::{Style, StyledStr};

use crate::component::GtkComponent;
use crate::renderer::GtkRenderer;
use crate::util::{resolved_color_to_hex, resolved_color_to_rgba8, store_watcher_guards};

impl GtkComponent for Native<TextConfig> {
    /// Renders a `WaterUI` Text component as a GTK4 Label.
    fn render(self, env: &Environment, _renderer: &mut GtkRenderer) -> Widget {
        let config = self.into_inner();
        let content = config.content;
        let paragraph_alignment = config.paragraph_alignment;

        let label = Label::new(None);
        apply_styled_content(&label, content.get(), paragraph_alignment.get(), env);

        // Match native behavior: read-only text should not be selection-active by default.
        label.set_selectable(false);
        // TextConfig is used for content text which may need wrapping
        label.set_wrap(true);
        label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        if let Some(limit) = config.line_limit {
            // TextConfig::line_limit: cap the laid-out lines and truncate the
            // last visible one with an ellipsis, the platform label baseline.
            label.set_lines(i32::try_from(limit.get()).unwrap_or(i32::MAX));
            label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        }

        // Set up reactive updates. The watcher guards are stored in the
        // label's own qdata, so the callbacks must hold the label weakly: a
        // strong capture would close a reference cycle and keep every text
        // widget alive until its signals last rather than until it is
        // released. An `upgrade` that fails means the label is gone and the
        // update is dropped with it.
        let mut guards = Vec::new();
        {
            let weak_label = label.downgrade();
            let env = env.clone();
            let paragraph_alignment = paragraph_alignment.clone();
            guards.push(content.watch(move |ctx| {
                let Some(label) = weak_label.upgrade() else {
                    return;
                };
                let content = ctx.into_value();
                let env = env.clone();
                let alignment = paragraph_alignment.get();
                // Schedule update on GTK main thread
                glib::idle_add_local_once(move || {
                    apply_styled_content(&label, content, alignment, &env);
                });
            }));
        }

        {
            let weak_label = label.downgrade();
            guards.push(paragraph_alignment.watch(move |ctx| {
                let Some(label) = weak_label.upgrade() else {
                    return;
                };
                let alignment = ctx.into_value();
                glib::idle_add_local_once(move || {
                    apply_paragraph_alignment(&label, alignment);
                });
            }));
        }

        // Repaint when the environment `Foreground` token changes: the theme
        // mutates the slot's signal on scheme switches and `.foreground()`
        // overrides install into the same slot.
        if let Some(foreground) = installed_color_signal::<Foreground>(env) {
            let weak_label = label.downgrade();
            let env = env.clone();
            let content = content.clone();
            let paragraph_alignment = paragraph_alignment.clone();
            guards.push(foreground.watch(move |_ctx| {
                let Some(label) = weak_label.upgrade() else {
                    return;
                };
                let env = env.clone();
                let content = content.clone();
                let paragraph_alignment = paragraph_alignment.clone();
                glib::idle_add_local_once(move || {
                    apply_styled_content(&label, content.get(), paragraph_alignment.get(), &env);
                });
            }));
        }
        store_watcher_guards(&label, guards);

        // GTK draws insensitive widgets with the theme's disabled color, which
        // an emitted `foreground` attribute would mask. The `sensitive`
        // property only reports the widget's own flag; inherited insensitivity
        // arrives through the `INSENSITIVE` state flag, so rebuild the markup
        // on that flag's transitions (hover/focus flips leave it unchanged)
        // and let the theme dimming apply while the label is insensitive.
        {
            let env = env.clone();
            label.connect_state_flags_changed(move |label, previous| {
                let insensitive = label.state_flags().contains(gtk4::StateFlags::INSENSITIVE);
                if insensitive == previous.contains(gtk4::StateFlags::INSENSITIVE) {
                    return;
                }
                apply_styled_content(label, content.get(), paragraph_alignment.get(), &env);
            });
        }

        label.upcast()
    }
}

fn apply_styled_content(
    label: &Label,
    content: StyledStr,
    alignment: HorizontalAlignment,
    env: &Environment,
) {
    // `INSENSITIVE` is the flag GTK's disabled styling keys on; it reflects
    // effective sensitivity including ancestors, unlike `notify::sensitive`.
    let sensitive = !label.state_flags().contains(gtk4::StateFlags::INSENSITIVE);
    let markup = styled_to_markup(content, env, sensitive);
    label.set_markup(&markup);
    apply_paragraph_alignment(label, alignment);
}

fn apply_paragraph_alignment(label: &Label, alignment: HorizontalAlignment) {
    if alignment == HorizontalAlignment::Leading {
        label.set_justify(gtk4::Justification::Left);
        label.set_xalign(0.0);
    } else if alignment == HorizontalAlignment::Trailing {
        label.set_justify(gtk4::Justification::Right);
        label.set_xalign(1.0);
    } else {
        label.set_justify(gtk4::Justification::Center);
        label.set_xalign(0.5);
    }
}

fn styled_to_markup(content: StyledStr, env: &Environment, sensitive: bool) -> String {
    let mut markup = String::new();

    for (text, style) in content.into_chunks() {
        let escaped_text = escape_markup_text(text.as_str());
        if escaped_text.is_empty() {
            continue;
        }

        let attrs = style_to_markup_attrs(&style, env, sensitive);
        if attrs.is_empty() {
            markup.push_str(&escaped_text);
            continue;
        }

        markup.push_str("<span");
        markup.push_str(&attrs);
        markup.push('>');
        markup.push_str(&escaped_text);
        markup.push_str("</span>");
    }

    markup
}

fn style_to_markup_attrs(style: &Style, env: &Environment, sensitive: bool) -> String {
    let mut attrs = String::new();
    let resolved_font: ResolvedFont = style.font.resolve(env).get();
    let font_size = resolved_font.size.max(1.0);
    let _ = write!(attrs, " size=\"{font_size:.2}pt\"");
    let _ = write!(
        attrs,
        " weight=\"{}\"",
        font_weight_to_pango_value(resolved_font.weight)
    );

    // A named family verbatim; otherwise the design, as the fontconfig generic
    // Pango resolves to the platform's own face. The proportional default is
    // what Pango uses when no family is given at all.
    if let Some(family) = resolved_font.family
        && !family.is_empty()
    {
        let family = escape_markup_attr(family.as_str());
        let _ = write!(attrs, " font_family=\"{family}\"");
    } else {
        match resolved_font.design {
            FontDesign::Default => {}
            FontDesign::Monospaced => attrs.push_str(" font_family=\"monospace\""),
        }
    }

    if style.italic {
        attrs.push_str(" style=\"italic\"");
    }

    if style.underline {
        attrs.push_str(" underline=\"single\"");
    }

    if style.strikethrough {
        attrs.push_str(" strikethrough=\"true\"");
    }

    // `Foreground` is the environment's default text color: the theme installs
    // it from the platform palette and `.foreground()` overrides the same slot,
    // so chunks without an explicit color must still honor it. GTK's CSS knows
    // nothing of the environment — the resolved color must be emitted, except
    // while the label is insensitive: an emitted attribute would mask the
    // theme's disabled color, exactly like a label whose markup carries no
    // color at all. When no slot is installed the label keeps GTK's own
    // default color.
    let foreground = style
        .foreground
        .as_ref()
        .map(|foreground| foreground.resolve(env).get())
        .or_else(|| {
            if sensitive {
                installed_color_signal::<Foreground>(env).map(|signal| signal.get())
            } else {
                None
            }
        });
    if let Some(foreground) = foreground {
        emit_color_attrs(&mut attrs, "foreground", foreground);
    }

    if let Some(background) = &style.background {
        emit_color_attrs(&mut attrs, "background", background.resolve(env).get());
    }

    attrs
}

/// Emits a Pango color attribute. Opacity is carried inside the color spec
/// as `#RRGGBBAA` (Pango 1.38+): the `alpha`/`background_alpha` attributes
/// reject zero, so an 8-digit hex is the only markup form covering the full
/// `[0, 1]` range.
fn emit_color_attrs(attrs: &mut String, name: &str, color: ResolvedColor) {
    let (red, green, blue, alpha) = resolved_color_to_rgba8(color);
    if alpha < 1.0 {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the alpha channel is clamped to [0.0, 1.0] before scaling"
        )]
        let alpha = (alpha * 255.0).round() as u8;
        let _ = write!(
            attrs,
            " {name}=\"#{red:02X}{green:02X}{blue:02X}{alpha:02X}\""
        );
    } else {
        let _ = write!(attrs, " {name}=\"{}\"", resolved_color_to_hex(color));
    }
}

const fn font_weight_to_pango_value(weight: FontWeight) -> u16 {
    match weight {
        FontWeight::Thin => 100,
        FontWeight::UltraLight => 200,
        FontWeight::Light => 300,
        FontWeight::Normal => 400,
        FontWeight::Medium => 500,
        FontWeight::SemiBold => 600,
        FontWeight::Bold => 700,
        FontWeight::UltraBold => 800,
        FontWeight::Black => 900,
    }
}

fn escape_markup_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn escape_markup_attr(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use nami::Computed;
    use waterui::theme::{install_color_signal, install_font_signal};
    use waterui_graphics::color::{Color, Srgb};
    use waterui_text::font::{Body, FontSlot};

    use super::*;

    fn init() {
        gtk4::init().expect("GTK tests need a display; run them under xvfb-run");
    }

    fn resolved_u8(red: u8, green: u8, blue: u8, alpha: f32) -> ResolvedColor {
        ResolvedColor::from_srgb(Srgb::new_u8(red, green, blue)).with_opacity(alpha)
    }

    /// A minimal faithful environment: `Style::default().font` resolves the
    /// `Body` slot and panics when no font signal is installed, so tests get
    /// the same default the theme installs (`theme::install_fonts`).
    fn test_env() -> Environment {
        let mut env = Environment::new();
        install_font_signal::<Body>(&mut env, Computed::constant(Body::DEFAULT));
        env
    }

    fn env_with_foreground(color: ResolvedColor) -> Environment {
        let mut env = test_env();
        install_color_signal::<Foreground>(&mut env, Computed::constant(color));
        env
    }

    fn render_label(env: &Environment, content: &str) -> Label {
        let mut renderer = GtkRenderer::new();
        let widget = Native::new(TextConfig::new(Computed::constant(StyledStr::from(
            String::from(content),
        ))))
        .render(env, &mut renderer);
        widget
            .downcast::<Label>()
            .expect("Native<TextConfig> renders a Label")
    }

    /// Parses the generated markup with the real Pango parser and returns the
    /// first attribute of `ty`, proving the emitted attributes are not only
    /// syntactically valid but land on the intended attribute type.
    fn parsed_attr(markup: &str, ty: gtk4::pango::AttrType) -> Option<gtk4::pango::Attribute> {
        let (attrs, _, _) = gtk4::pango::parse_markup(markup, '\0')
            .expect("generated markup must parse through Pango");
        attrs
            .iterator()
            .attrs()
            .into_iter()
            .find(|attr| attr.type_() == ty)
    }

    fn parsed_rgb(markup: &str, ty: gtk4::pango::AttrType) -> Option<(u16, u16, u16)> {
        let attr = parsed_attr(markup, ty)?;
        let color = attr
            .downcast_ref::<gtk4::pango::AttrColor>()
            .expect("color attribute")
            .color();
        Some((color.red(), color.green(), color.blue()))
    }

    fn parsed_int(markup: &str, ty: gtk4::pango::AttrType) -> Option<i32> {
        parsed_attr(markup, ty).map(|attr| {
            attr.downcast_ref::<gtk4::pango::AttrInt>()
                .expect("integer attribute")
                .value()
        })
    }

    #[test]
    fn environment_foreground_reaches_unstyled_chunks() {
        let env = env_with_foreground(resolved_u8(255, 255, 255, 1.0));
        let markup = styled_to_markup(StyledStr::from("body"), &env, true);
        assert_eq!(
            parsed_rgb(&markup, gtk4::pango::AttrType::Foreground),
            Some((0xFFFF, 0xFFFF, 0xFFFF))
        );
        assert_eq!(
            parsed_int(&markup, gtk4::pango::AttrType::ForegroundAlpha),
            None
        );
    }

    #[test]
    fn environment_foreground_preserves_alpha() {
        let env = env_with_foreground(resolved_u8(255, 255, 255, 0.5));
        let markup = styled_to_markup(StyledStr::from("body"), &env, true);
        assert_eq!(
            parsed_int(&markup, gtk4::pango::AttrType::ForegroundAlpha),
            Some(0x8080)
        );

        let env = env_with_foreground(resolved_u8(255, 255, 255, 0.0));
        let markup = styled_to_markup(StyledStr::from("body"), &env, true);
        assert_eq!(
            parsed_int(&markup, gtk4::pango::AttrType::ForegroundAlpha),
            Some(0)
        );
    }

    #[test]
    fn explicit_span_foreground_keeps_precedence_and_alpha() {
        let env = env_with_foreground(resolved_u8(255, 255, 255, 1.0));
        let mut content = StyledStr::from("");
        content.push(
            "hi",
            Style::default().foreground(Color::new(resolved_u8(255, 0, 0, 0.25))),
        );
        let markup = styled_to_markup(content, &env, true);
        assert_eq!(
            parsed_rgb(&markup, gtk4::pango::AttrType::Foreground),
            Some((0xFFFF, 0, 0))
        );
        assert_eq!(
            parsed_int(&markup, gtk4::pango::AttrType::ForegroundAlpha),
            Some(0x4040)
        );
    }

    #[test]
    fn explicit_span_background_preserves_alpha() {
        let env = test_env();
        let mut content = StyledStr::from("");
        content.push(
            "hi",
            Style::default().background(Color::new(resolved_u8(0, 128, 0, 0.5))),
        );
        let markup = styled_to_markup(content, &env, true);
        assert_eq!(
            parsed_rgb(&markup, gtk4::pango::AttrType::Background),
            Some((0, 0x8080, 0))
        );
        assert_eq!(
            parsed_int(&markup, gtk4::pango::AttrType::BackgroundAlpha),
            Some(0x8080)
        );
    }

    #[test]
    fn insensitive_label_drops_environment_foreground() {
        let env = env_with_foreground(resolved_u8(255, 255, 255, 1.0));
        let sensitive = styled_to_markup(StyledStr::from("body"), &env, true);
        let insensitive = styled_to_markup(StyledStr::from("body"), &env, false);
        assert!(parsed_attr(&sensitive, gtk4::pango::AttrType::Foreground).is_some());
        assert!(parsed_attr(&insensitive, gtk4::pango::AttrType::Foreground).is_none());
    }

    /// The rendered label paints with the installed `Foreground` while
    /// sensitive and falls back to GTK's disabled styling while insensitive —
    /// including when an ancestor, not the label itself, is the disabled one.
    #[test]
    fn rendered_label_follows_effective_sensitivity() {
        init();
        let env = env_with_foreground(resolved_u8(255, 255, 255, 1.0));
        let label = render_label(&env, "body");
        assert!(parsed_attr(&label.label(), gtk4::pango::AttrType::Foreground).is_some());

        let parent = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        parent.append(&label);
        parent.set_sensitive(false);
        assert!(label.state_flags().contains(gtk4::StateFlags::INSENSITIVE));
        assert!(parsed_attr(&label.label(), gtk4::pango::AttrType::Foreground).is_none());

        parent.set_sensitive(true);
        assert!(parsed_attr(&label.label(), gtk4::pango::AttrType::Foreground).is_some());
    }

    /// The watcher guards live in the label's own qdata, so their callbacks
    /// must not hold the label strongly — otherwise every text widget leaks
    /// through a reference cycle. Releasing the widget must free it.
    #[test]
    fn label_is_released_with_its_watchers() {
        init();
        let env = env_with_foreground(resolved_u8(255, 255, 255, 1.0));
        let label = render_label(&env, "body");
        let weak = label.downgrade();
        drop(label);
        while glib::MainContext::default().iteration(false) {}
        assert!(weak.upgrade().is_none());
    }
}
