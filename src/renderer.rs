//! View renderer that dispatches `WaterUI` views to GTK widgets.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use glib::object::ObjectExt;
use glib::value::ToValue;
use gtk4::Align;
use gtk4::Widget;
use gtk4::glib::Propagation;
use gtk4::prelude::*;
use nami::Signal;
use waterui::Url;
use waterui::accessibility::{
    AccessibilityChecked, AccessibilityChildren, AccessibilityHidden, AccessibilityLabel,
    AccessibilityRole, AccessibilityState, AccessibilityStateSignal, AccessibilityValue,
};
use waterui::background::{Background, MaterialBackground};
use waterui::border::Border;
use waterui::component::badge::BadgeConfig;
use waterui::component::list::ListConfig;
use waterui::component::progress::ProgressConfig;
use waterui::cursor::{Cursor, CursorStyle};
use waterui::drag_drop::{
    DragPayload, Draggable, DropDestination, Files, PlatformRepresentation, TransferKind,
};
use waterui::gesture::PointerButton;
use waterui::interaction::Hittable;
use waterui::metadata::anchored_overlay::AnchoredOverlay;
use waterui::metadata::context_menu::ResolvedContextMenu;
use waterui::metadata::secure::{HighDynamicRange, Secure, StandardDynamicRange};
use waterui::prelude::Divider;
use waterui::style::{Offset, Rotation, Scale};
use waterui_backend_core::ViewDispatcher;
use waterui_controls::button::ButtonConfig;
use waterui_controls::menu::ResolvedMenu;
use waterui_controls::slider::SliderConfig;
use waterui_controls::stepper::StepperConfig;
use waterui_controls::text_field::ResolvedTextFieldConfig;
use waterui_controls::toggle::ToggleConfig;
use waterui_core::Binding;
use waterui_core::dynamic::Dynamic;
use waterui_core::event::{Event, HoverEvent, LifeCycle, LifeCycleHook, OnEvent};
use waterui_core::handler::BoxedAction;
use waterui_core::key::{KeyHandling, KeyPress, OnKeyPress};
use waterui_core::layout::{LayoutPriority, StretchAxis};
use waterui_core::metadata::MetadataKey;
use waterui_core::{AnyView, Environment, Native, View};
use waterui_core::{IgnorableMetadata, Metadata, Retain, Str};
use waterui_form::picker::PickerConfig;
use waterui_form::picker::color::ColorPickerConfig;
use waterui_form::picker::date::DatePickerConfig;
use waterui_form::picker::multi_date::MultiDatePickerConfig;
use waterui_form::secure::SecureFieldConfig;
use waterui_graphics::{
    ExternalFrameView, FilteredView, GpuContentView, Gradient, Picture, SceneView,
    color::{Color, WorkingColor},
};
use waterui_icon::SystemIcon;
use waterui_layout::container::{FixedContainer, LazyContainer};
use waterui_layout::safe_area::IgnoreSafeArea;
use waterui_layout::scroll::ScrollView;
use waterui_layout::spacer::Spacer;
use waterui_navigation::tab::TabsLayout;
use waterui_navigation::{
    NavigationSplitLayout, NavigationStack, NavigationTransitionDestination,
    NavigationTransitionSource, NavigationView,
};
use waterui_shape::{ClipShape, ResolvedShape};
use waterui_text::TextConfig;
#[cfg(feature = "webview-system")]
use waterui_webview::WebView;

use crate::browser_input;
use crate::component::GtkComponent;
use crate::components::graphics::clip_shape_widget::WuiClipShape;
use crate::components::menu::rebuild_menu_popover;
use crate::layout::proposal::{note_reported_axis, set_layout_priority, transparent_to_content};
use crate::util::{ScopedCss, store_watcher_guard, subscribe_then_get};

pub(crate) const CSS_CLASS_DYNAMIC_RANGE_SDR: &str = "waterui-dynamic-range-sdr";
pub(crate) const CSS_CLASS_DYNAMIC_RANGE_HDR: &str = "waterui-dynamic-range-hdr";

const FOCUS_ANCHOR_DATA_KEY: &str = "waterui_focus_anchor";
const FOCUS_REQUEST_PENDING_DATA_KEY: &str = "waterui_focus_request_pending";
const FOCUS_MAP_HANDLER_INSTALLED_DATA_KEY: &str = "waterui_focus_map_handler_installed";
const RETAIN_DATA_KEY: &str = "waterui-retain-metadata";

fn gtk_accessibility_role(role: &AccessibilityRole) -> gtk4::AccessibleRole {
    match role {
        AccessibilityRole::Button => gtk4::AccessibleRole::Button,
        AccessibilityRole::Link => gtk4::AccessibleRole::Link,
        AccessibilityRole::Image => gtk4::AccessibleRole::Img,
        AccessibilityRole::Text => gtk4::AccessibleRole::Label,
        AccessibilityRole::Header => gtk4::AccessibleRole::Heading,
        // GTK has no landmark role for a footer; `Section` is the closest
        // container role, which is also what a plain section maps to.
        AccessibilityRole::Footer | AccessibilityRole::Section => gtk4::AccessibleRole::Section,
        AccessibilityRole::Navigation => gtk4::AccessibleRole::Navigation,
        AccessibilityRole::Main => gtk4::AccessibleRole::Main,
        AccessibilityRole::Search => gtk4::AccessibleRole::Search,
        AccessibilityRole::Article => gtk4::AccessibleRole::Document,
        AccessibilityRole::List => gtk4::AccessibleRole::List,
        AccessibilityRole::ListItem => gtk4::AccessibleRole::ListItem,
        AccessibilityRole::Checkbox => gtk4::AccessibleRole::Checkbox,
        AccessibilityRole::RadioButton => gtk4::AccessibleRole::Radio,
        AccessibilityRole::Switch => gtk4::AccessibleRole::Switch,
        AccessibilityRole::Slider => gtk4::AccessibleRole::Slider,
        AccessibilityRole::ProgressBar => gtk4::AccessibleRole::ProgressBar,
        AccessibilityRole::Tab => gtk4::AccessibleRole::Tab,
        AccessibilityRole::TabList => gtk4::AccessibleRole::TabList,
        AccessibilityRole::TabPanel => gtk4::AccessibleRole::TabPanel,
        AccessibilityRole::Menu => gtk4::AccessibleRole::Menu,
        AccessibilityRole::MenuItem => gtk4::AccessibleRole::MenuItem,
        AccessibilityRole::MenuBar => gtk4::AccessibleRole::MenuBar,
        AccessibilityRole::MenuItemCheckbox => gtk4::AccessibleRole::MenuItemCheckbox,
        AccessibilityRole::MenuItemRadio => gtk4::AccessibleRole::MenuItemRadio,
        AccessibilityRole::Combobox => gtk4::AccessibleRole::ComboBox,
        AccessibilityRole::Option => gtk4::AccessibleRole::Option,
        AccessibilityRole::Group => gtk4::AccessibleRole::Group,
        _ => panic!("unsupported accessibility role in GTK backend"),
    }
}

fn apply_gtk_accessibility_state(widget: &Widget, state: &AccessibilityState) {
    widget.update_state(&[
        gtk4::accessible::State::Disabled(state.is_disabled()),
        gtk4::accessible::State::Selected(Some(state.is_selected())),
        gtk4::accessible::State::Expanded(state.expanded_state()),
        gtk4::accessible::State::Busy(state.is_busy()),
        gtk4::accessible::State::Hidden(state.is_hidden()),
    ]);
    // `GtkAccessibleTristate` has no "unset" member: false/true/mixed are the
    // only values the state can hold. A view that is not checkable expresses
    // that by the state being absent, which is `gtk_accessible_reset_state`.
    // Resetting rather than skipping matters because widgets are reused: a row
    // that was checked and is now plain must lose the attribute, not keep a
    // stale one.
    match state.checked_state() {
        None => widget.reset_state(gtk4::AccessibleState::Checked),
        Some(checked) => {
            let tristate = match checked {
                AccessibilityChecked::False => gtk4::AccessibleTristate::False,
                AccessibilityChecked::True => gtk4::AccessibleTristate::True,
                AccessibilityChecked::Mixed => gtk4::AccessibleTristate::Mixed,
            };
            widget.update_state(&[gtk4::accessible::State::Checked(tristate)]);
        }
    }
}

#[derive(Debug)]
pub(crate) struct FocusAnchorMarker;

#[derive(Debug)]
struct PendingFocusRequest;

#[derive(Debug)]
struct FocusMapHandlerInstalled;

#[derive(Debug, Default)]
struct ReactiveAxisPair {
    x: Cell<Option<f32>>,
    y: Cell<Option<f32>>,
}

impl ReactiveAxisPair {
    fn update_x(&self, value: f32) {
        self.x.set(Some(value));
    }

    fn update_y(&self, value: f32) {
        self.y.set(Some(value));
    }

    fn values(&self) -> Option<(f32, f32)> {
        Some((self.x.get()?, self.y.get()?))
    }
}

pub(crate) fn mark_focus_anchor(widget: &impl IsA<Widget>) {
    // SAFETY: `FOCUS_ANCHOR_DATA_KEY` is private to this module and is only
    // ever paired with `FocusAnchorMarker`, so every read of the key downcasts
    // to the type stored here; widget data is only touched on the GTK main
    // thread.
    unsafe {
        widget
            .as_ref()
            .set_data(FOCUS_ANCHOR_DATA_KEY, FocusAnchorMarker);
    }
}

/// Whether `anchor` or one of its descendants holds keyboard focus.
///
/// Composite widgets delegate focus to an inner widget — `gtk4::Entry`
/// forwards `grab_focus` to its private `GtkText`, so `has_focus` never turns
/// true on the entry itself. `FOCUS_WITHIN` covers both the direct and the
/// delegated case.
fn anchor_holds_focus(anchor: &Widget) -> bool {
    anchor
        .state_flags()
        .contains(gtk4::StateFlags::FOCUS_WITHIN)
}

fn attach_focus_metadata(widget: Widget, binding: &Binding<bool>) -> Widget {
    let anchor = resolve_single_focus_anchor(&widget);

    anchor.connect_state_flags_changed({
        let binding = binding.clone();
        move |anchor, _| {
            let focused = anchor_holds_focus(anchor);
            if binding.snapshot() != focused {
                binding.set(focused);
            }
        }
    });

    let (focused, guard) = subscribe_then_get(binding, {
        let anchor = anchor.clone();
        move |ctx| {
            if ctx.into_value() {
                request_focus(&anchor);
            } else {
                clear_focus(&anchor);
            }
        }
    });
    if focused {
        request_focus(&anchor);
    }
    store_watcher_guard(&widget, guard);

    widget
}

fn resolve_single_focus_anchor(widget: &Widget) -> Widget {
    let mut anchors = Vec::new();
    collect_focus_anchors(widget, &mut anchors);
    assert!(
        anchors.len() == 1,
        "GTK Focused metadata requires exactly one TextField or SecureField anchor in its subtree, found {}",
        anchors.len()
    );
    anchors
        .pop()
        .expect("focus anchor count was asserted to be exactly one")
}

fn collect_focus_anchors(widget: &Widget, anchors: &mut Vec<Widget>) {
    // SAFETY: `FOCUS_ANCHOR_DATA_KEY` only ever stores `FocusAnchorMarker`
    // (see `mark_focus_anchor`), and the returned pointer is discarded after
    // the presence check, so no reference outlives this main-thread call.
    if unsafe { widget.data::<FocusAnchorMarker>(FOCUS_ANCHOR_DATA_KEY) }.is_some() {
        anchors.push(widget.clone());
    }

    let mut child = widget.first_child();
    while let Some(current) = child {
        collect_focus_anchors(&current, anchors);
        child = current.next_sibling();
    }
}

fn request_focus(anchor: &Widget) {
    if anchor_holds_focus(anchor) {
        return;
    }

    if anchor.is_mapped() {
        assert!(
            anchor.grab_focus(),
            "GTK Focused metadata failed to focus its resolved TextField/SecureField anchor"
        );
        return;
    }

    // SAFETY: `FOCUS_REQUEST_PENDING_DATA_KEY` only ever stores
    // `PendingFocusRequest`, and the returned pointer is discarded after the
    // presence check, so no reference outlives this main-thread call.
    if unsafe { anchor.data::<PendingFocusRequest>(FOCUS_REQUEST_PENDING_DATA_KEY) }.is_some() {
        return;
    }

    // SAFETY: same key/type pairing as the check above; widget data is only
    // touched on the GTK main thread.
    unsafe { anchor.set_data(FOCUS_REQUEST_PENDING_DATA_KEY, PendingFocusRequest) };
    // SAFETY: `FOCUS_MAP_HANDLER_INSTALLED_DATA_KEY` only ever stores
    // `FocusMapHandlerInstalled`, and the returned pointer is discarded after
    // the presence check, so no reference outlives this main-thread call.
    if unsafe {
        anchor
            .data::<FocusMapHandlerInstalled>(FOCUS_MAP_HANDLER_INSTALLED_DATA_KEY)
            .is_none()
    } {
        // SAFETY: same key/type pairing as the check above; widget data is
        // only touched on the GTK main thread.
        unsafe {
            anchor.set_data(
                FOCUS_MAP_HANDLER_INSTALLED_DATA_KEY,
                FocusMapHandlerInstalled,
            );
        };
        anchor.connect_map(|widget| {
            // SAFETY: `FOCUS_REQUEST_PENDING_DATA_KEY` only ever stores
            // `PendingFocusRequest`; stealing transfers ownership of the
            // marker to this main-thread call, and no other reference to it
            // exists because presence checks discard their pointers.
            if unsafe { widget.steal_data::<PendingFocusRequest>(FOCUS_REQUEST_PENDING_DATA_KEY) }
                .is_none()
            {
                return;
            }

            assert!(
                widget.grab_focus(),
                "GTK Focused metadata failed to focus its resolved TextField/SecureField anchor when it was mapped"
            );
        });
    }
}

fn clear_focus(anchor: &Widget) {
    // SAFETY: `FOCUS_REQUEST_PENDING_DATA_KEY` only ever stores
    // `PendingFocusRequest`; stealing transfers ownership of the marker to
    // this main-thread call, and no other reference to it exists because
    // presence checks discard their pointers.
    let _ = unsafe { anchor.steal_data::<PendingFocusRequest>(FOCUS_REQUEST_PENDING_DATA_KEY) };

    if !anchor_holds_focus(anchor) {
        return;
    }

    let Some(root) = anchor.root() else {
        panic!("focused anchor lost its GTK root while still holding focus");
    };
    root.set_focus(None::<&Widget>);
}

/// Context passed to component renderers.
///
/// Only [`GtkRenderer::render`] and [`GtkRenderer::render_any`] construct
/// this, always from a live `&mut GtkRenderer` immediately before a
/// synchronous dispatch, so the pointer is never null and outlives every
/// handler invocation it is passed to.
#[derive(Debug, Clone)]
pub struct RenderContext {
    /// Reference to the renderer for recursive rendering.
    /// This is a raw pointer because we can't have self-referential borrows.
    renderer_ptr: *mut GtkRenderer,
}

impl RenderContext {
    /// Creates a context with a renderer reference.
    const fn with_renderer(renderer: &mut GtkRenderer) -> Self {
        Self {
            renderer_ptr: std::ptr::from_mut::<GtkRenderer>(renderer),
        }
    }

    /// Gets a mutable reference to the renderer.
    ///
    /// # Safety
    ///
    /// The caller must ensure the renderer pointer is valid.
    #[allow(
        clippy::mut_from_ref,
        reason = "RenderContext is a raw-pointer handle threaded through GTK callbacks; \
                  the caller upholds exclusivity, which the signature cannot express"
    )]
    pub(crate) unsafe fn renderer(&self) -> &mut GtkRenderer {
        // SAFETY: Caller guarantees the renderer pointer is initialized and valid.
        unsafe { &mut *self.renderer_ptr }
    }
}

const GTK_METADATA_CSS_PRIORITY: u32 = gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION;
const CSS_CLASS_BORDER: &str = "waterui-metadata-border";
const CSS_CLASS_SCALE: &str = "waterui-metadata-scale";
const CSS_CLASS_ROTATION: &str = "waterui-metadata-rotation";
const CSS_CLASS_OFFSET: &str = "waterui-metadata-offset";

fn wrap_for_metadata(child: &Widget) -> gtk4::Box {
    let wrapper = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    wrapper.set_halign(Align::Fill);
    wrapper.set_valign(Align::Fill);
    wrapper.append(child);
    // The box exists only to carry the metadata's GTK realization; layout-wise
    // it *is* the content, so every layout channel reads through to it.
    transparent_to_content(wrapper.upcast_ref(), child);
    wrapper
}

fn attach_css_provider(widget: &impl IsA<gtk4::Widget>, class_name: &str) -> ScopedCss {
    ScopedCss::attach(widget, class_name, GTK_METADATA_CSS_PRIORITY)
}

fn cursor_style_to_gtk_name(style: CursorStyle) -> &'static str {
    match style {
        CursorStyle::Arrow => "default",
        CursorStyle::PointingHand => "pointer",
        CursorStyle::IBeam => "text",
        CursorStyle::Crosshair => "crosshair",
        CursorStyle::OpenHand => "grab",
        CursorStyle::ClosedHand => "grabbing",
        CursorStyle::NotAllowed => "not-allowed",
        CursorStyle::ResizeLeft => "w-resize",
        CursorStyle::ResizeRight => "e-resize",
        CursorStyle::ResizeUp => "n-resize",
        CursorStyle::ResizeDown => "s-resize",
        CursorStyle::ResizeLeftRight => "ew-resize",
        CursorStyle::ResizeUpDown => "ns-resize",
        CursorStyle::Move => "move",
        CursorStyle::Wait => "wait",
        CursorStyle::Copy => "copy",
        _ => panic!("unsupported CursorStyle variant on GTK backend"),
    }
}

fn apply_border_css(
    css: &ScopedCss,
    resolved: WorkingColor,
    width: f32,
    corner_radius: f32,
    edges: waterui_layout::EdgeSet,
) {
    let color = crate::util::resolved_color_to_css_rgba(resolved);
    let top = if edges.top { width } else { 0.0 };
    let left = if edges.leading { width } else { 0.0 };
    let bottom = if edges.bottom { width } else { 0.0 };
    let right = if edges.trailing { width } else { 0.0 };
    css.set_declarations(&format!(
        "border-style: solid; border-color: {color}; \
         border-top-width: {top:.2}px; border-left-width: {left:.2}px; \
         border-bottom-width: {bottom:.2}px; border-right-width: {right:.2}px; \
         border-radius: {corner_radius:.2}px;"
    ));
}

fn apply_scale_css(css: &ScopedCss, x: f32, y: f32, anchor: waterui::style::Anchor) {
    let origin_x = anchor.x * 100.0;
    let origin_y = anchor.y * 100.0;
    css.set_declarations(&format!(
        "transform: scale({x:.5}, {y:.5}); transform-origin: {origin_x:.3}% {origin_y:.3}%;"
    ));
}

fn apply_rotation_css(css: &ScopedCss, angle_degrees: f32, anchor: waterui::style::Anchor) {
    let origin_x = anchor.x * 100.0;
    let origin_y = anchor.y * 100.0;
    css.set_declarations(&format!(
        "transform: rotate({angle_degrees:.5}deg); transform-origin: {origin_x:.3}% {origin_y:.3}%;"
    ));
}

fn apply_offset_css(css: &ScopedCss, x: f32, y: f32) {
    css.set_declarations(&format!("transform: translate({x:.2}px, {y:.2}px);"));
}

/// MIME type an in-process drag puts on the wire. The value has no pasteboard
/// form; the real payload lives in `DRAG_PAYLOADS` behind the `GdkDrag`, so the
/// bytes only exist to give same-process drop targets a format to match.
const IN_PROCESS_DRAG_MIME: &str = "application/x-waterui-in-process";

thread_local! {
    /// The typed payload behind each `DragSource` this process started, keyed
    /// by its `GdkDrag`. GTK hands drop targets a value deserialized from MIME
    /// data, which cannot carry an arbitrary `Transferable`; for a local drag
    /// the destination takes the payload from here instead, so every
    /// `TransferKind` — `InProcess` included — matches the destination's
    /// accepted kind exactly.
    static DRAG_PAYLOADS: RefCell<HashMap<gdk4::Drag, DragPayload>> =
        RefCell::new(HashMap::new());
}

fn register_drag_payload(drag: &gdk4::Drag, payload: DragPayload) {
    DRAG_PAYLOADS.with(|payloads| {
        payloads.borrow_mut().insert(drag.clone(), payload);
    });
}

fn unregister_drag_payload(drag: &gdk4::Drag) {
    DRAG_PAYLOADS.with(|payloads| {
        payloads.borrow_mut().remove(drag);
    });
}

/// The typed payload a `DragSource` in this process attached to `drop`'s drag,
/// if the drag is local and originated from a `WaterUI` source.
fn local_drag_payload(drop: &gdk4::Drop) -> Option<DragPayload> {
    let drag = drop.drag()?;
    DRAG_PAYLOADS.with(|payloads| payloads.borrow().get(&drag).cloned())
}

fn drag_content_provider(payload: &DragPayload) -> gdk4::ContentProvider {
    match payload.platform_representation() {
        PlatformRepresentation::Text(text) => {
            gdk4::ContentProvider::for_value(&text.to_string().to_value())
        }
        PlatformRepresentation::Url(url) => {
            let text_provider = gdk4::ContentProvider::for_value(&url.to_string().to_value());
            let uri_list = format!("{url}\r\n");
            let uri_provider = gdk4::ContentProvider::for_bytes(
                "text/uri-list",
                &glib::Bytes::from(uri_list.as_bytes()),
            );
            gdk4::ContentProvider::new_union(&[text_provider, uri_provider])
        }
        PlatformRepresentation::Files(files) => {
            let mut uri_list = String::new();
            for url in files.urls() {
                uri_list.push_str(url.as_str());
                uri_list.push_str("\r\n");
            }
            gdk4::ContentProvider::for_bytes(
                "text/uri-list",
                &glib::Bytes::from(uri_list.as_bytes()),
            )
        }
        PlatformRepresentation::InProcess => {
            gdk4::ContentProvider::for_bytes(IN_PROCESS_DRAG_MIME, &glib::Bytes::from_static(&[]))
        }
    }
}

/// The MIME types a foreign drag offers that a destination of `kind` can turn
/// into its payload. GTK only reports the drop's `DropTarget` `GTypes` through,
/// so this list is the backend's half of that gate: a kind accepts a foreign
/// drag exactly when the formats can deliver it.
fn remote_drop_mime_matches(formats: &gdk4::ContentFormats, kind: TransferKind) -> bool {
    const TEXT_MIMES: &[&str] = &[
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
        "STRING",
        "TEXT",
        "COMPOUND_TEXT",
    ];
    match kind {
        TransferKind::Text => TEXT_MIMES
            .iter()
            .any(|mime| formats.contain_mime_type(mime)),
        TransferKind::Url => {
            formats.contain_mime_type("text/uri-list")
                || TEXT_MIMES
                    .iter()
                    .any(|mime| formats.contain_mime_type(mime))
        }
        TransferKind::Files => formats.contain_mime_type("text/uri-list"),
        // `InProcess` values never reach the pasteboard: a foreign drag can
        // never produce one. Local drags are answered from `DRAG_PAYLOADS`.
        TransferKind::InProcess(_) => false,
    }
}

/// Whether the destination accepts this drop: exactly by payload kind when the
/// drag is a local `WaterUI` one (its typed payload is registered by `GdkDrag`),
/// and by MIME offer for a foreign drag.
fn drop_destination_accepts(drop: &gdk4::Drop, destination: &DropDestination) -> bool {
    local_drag_payload(drop).map_or_else(
        || remote_drop_mime_matches(&drop.formats(), destination.accepted_kind()),
        |payload| destination.accepts(&payload),
    )
}

fn drop_value_string(value: &glib::Value) -> Option<String> {
    if let Ok(text) = value.get::<String>() {
        return Some(text);
    }
    if let Ok(text) = value.get::<glib::GString>() {
        return Some(text.to_string());
    }
    None
}

fn drop_value_urls(value: &glib::Value) -> Option<Vec<Url>> {
    let files = value.get::<gdk4::FileList>().ok()?;
    Some(
        files
            .files()
            .iter()
            .filter_map(|file| file.uri().parse::<Url>().ok())
            .collect(),
    )
}

/// Builds the payload a foreign drop delivers to a destination accepting
/// `kind`, from the value GTK deserialized. Returns `None` when the value
/// cannot carry that kind — the destination is then not delivered to.
fn payload_from_drop_value(value: &glib::Value, kind: TransferKind) -> Option<DragPayload> {
    match kind {
        TransferKind::Text => {
            drop_value_string(value).map(|text| DragPayload::new(Str::from(text)))
        }
        TransferKind::Url => {
            let url = drop_value_urls(value)
                .and_then(|urls| urls.into_iter().next())
                .or_else(|| {
                    drop_value_string(value)
                        .and_then(|text| text.parse::<Url>().ok())
                        .filter(Url::is_absolute)
                })?;
            Some(DragPayload::new(url))
        }
        TransferKind::Files => {
            let urls = drop_value_urls(value).or_else(|| {
                drop_value_string(value).map(|text| {
                    text.lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty() && !line.starts_with('#'))
                        .filter_map(|line| line.parse::<Url>().ok())
                        .collect()
                })
            })?;
            // `Files` is a list of `file://` URLs; a foreign drop cannot be
            // inspected before it lands, so non-file entries are dropped here.
            let urls: Vec<Url> = urls
                .into_iter()
                .filter(|url| url.is_absolute() && url.scheme() == Some("file"))
                .collect();
            (!urls.is_empty()).then(|| DragPayload::new(Files::new(urls)))
        }
        // `InProcess` payloads are delivered from `DRAG_PAYLOADS`, never from
        // a deserialized value.
        TransferKind::InProcess(_) => None,
    }
}

fn call_boxed_action(action: &Rc<RefCell<BoxedAction<()>>>, env: &Environment) {
    if let Ok(mut handler) = action.try_borrow_mut() {
        (**handler)(env);
    }
}

/// The button a `GtkGestureSingle` sequence runs on, in `WaterUI` terms. GDK
/// numbers the primary button 1, middle 2, secondary 3 and the side buttons
/// 4 and 5 (back, forward), as `surface_pointer_button` in
/// `browser_input.rs` does. `GtkGestureSingle`'s `button` property selects a
/// single button rather than a mask, so the controllers below listen on 0
/// (any) and the `buttons` set filters each press; buttons outside the five
/// the framework names never satisfy it.
const fn gesture_pointer_button(gtk_button: u32) -> Option<PointerButton> {
    match gtk_button {
        1 => Some(PointerButton::Primary),
        2 => Some(PointerButton::Middle),
        3 => Some(PointerButton::Secondary),
        4 => Some(PointerButton::Back),
        5 => Some(PointerButton::Forward),
        _ => None,
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
)]
#[allow(
    clippy::too_many_lines,
    reason = "one cohesive widget-construction pass; splitting it would scatter GTK setup order"
)]
fn install_gesture_observer(
    widget: &Widget,
    gesture: waterui::gesture::Gesture,
    action: Rc<RefCell<BoxedAction<()>>>,
    env: Environment,
) {
    use waterui::gesture::{
        DragEvent, Gesture, GesturePhase, GesturePoint, LongPressEvent, MagnificationEvent,
        RotationEvent, TapEvent,
    };

    match gesture {
        Gesture::Tap(tap) => {
            let click = gtk4::GestureClick::new();
            // 0 listens on every button; the mask filters each press.
            click.set_button(0);
            click.set_propagation_phase(gtk4::PropagationPhase::Capture);
            let required_count = tap.count;
            let buttons = tap.buttons;
            let env_for_handler = env;
            let action_for_handler = action.clone();
            click.connect_pressed(move |gesture, n_press, x, y| {
                let Some(button) = gesture_pointer_button(gesture.current_button()) else {
                    gesture.set_state(gtk4::EventSequenceState::Denied);
                    return;
                };
                if !buttons.accepts(button) {
                    gesture.set_state(gtk4::EventSequenceState::Denied);
                    return;
                }
                if n_press as u32 >= required_count {
                    let tap_event = TapEvent {
                        location: GesturePoint::new(x as f32, y as f32),
                        count: n_press as u32,
                        button,
                    };
                    let mut local_env = env_for_handler.clone();
                    local_env.insert(tap_event);
                    call_boxed_action(&action_for_handler, &local_env);
                    gesture.set_state(gtk4::EventSequenceState::Claimed);
                }
            });
            widget.add_controller(click);
        }
        Gesture::LongPress(long_press) => {
            let press = gtk4::GestureLongPress::new();
            press.set_button(0);
            press.set_delay_factor(f64::from(long_press.duration) / 500.0);
            press.set_propagation_phase(gtk4::PropagationPhase::Capture);
            let env_for_handler = env;
            let action_for_handler = action.clone();
            let duration = long_press.duration;
            let buttons = long_press.buttons;
            press.connect_pressed(move |gesture, x, y| {
                let Some(button) = gesture_pointer_button(gesture.current_button()) else {
                    gesture.set_state(gtk4::EventSequenceState::Denied);
                    return;
                };
                if !buttons.accepts(button) {
                    gesture.set_state(gtk4::EventSequenceState::Denied);
                    return;
                }
                let event = LongPressEvent {
                    location: GesturePoint::new(x as f32, y as f32),
                    duration: duration as f32,
                    button,
                };
                let mut local_env = env_for_handler.clone();
                local_env.insert(event);
                call_boxed_action(&action_for_handler, &local_env);
                gesture.set_state(gtk4::EventSequenceState::Claimed);
            });
            widget.add_controller(press);
        }
        Gesture::Drag(drag) => {
            let drag_gesture = gtk4::GestureDrag::new();
            drag_gesture.set_button(0);
            drag_gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
            let min_distance = drag.min_distance;
            let buttons = drag.buttons;
            let drag_button = Rc::new(Cell::new(None));
            let drag_started = Rc::new(RefCell::new(false));
            {
                let env_for_handler = env.clone();
                let action_for_handler = action.clone();
                let drag_started = drag_started.clone();
                let drag_button = drag_button.clone();
                drag_gesture.connect_drag_begin(move |gesture, x, y| {
                    *drag_started.borrow_mut() = false;
                    let Some(button) = gesture_pointer_button(gesture.current_button()) else {
                        gesture.set_state(gtk4::EventSequenceState::Denied);
                        drag_button.set(None);
                        return;
                    };
                    if !buttons.accepts(button) {
                        gesture.set_state(gtk4::EventSequenceState::Denied);
                        drag_button.set(None);
                        return;
                    }
                    drag_button.set(Some(button));
                    let event = DragEvent {
                        phase: GesturePhase::Started,
                        location: GesturePoint::new(x as f32, y as f32),
                        translation: GesturePoint::new(0.0, 0.0),
                        velocity: GesturePoint::new(0.0, 0.0),
                        button,
                    };
                    let mut local_env = env_for_handler.clone();
                    local_env.insert(event);
                    call_boxed_action(&action_for_handler, &local_env);
                    gesture.set_state(gtk4::EventSequenceState::Claimed);
                });
            }
            {
                let env_for_handler = env.clone();
                let action_for_handler = action.clone();
                let drag_started = drag_started.clone();
                let drag_button = drag_button.clone();
                drag_gesture.connect_drag_update(move |gesture, offset_x, offset_y| {
                    let Some(button) = drag_button.get() else {
                        return;
                    };
                    let distance = offset_x.hypot(offset_y) as f32;
                    if distance < min_distance && !*drag_started.borrow() {
                        return;
                    }
                    *drag_started.borrow_mut() = true;
                    let start = gesture.start_point().unwrap_or((0.0, 0.0));
                    let event = DragEvent {
                        phase: GesturePhase::Updated,
                        location: GesturePoint::new(
                            (start.0 + offset_x) as f32,
                            (start.1 + offset_y) as f32,
                        ),
                        translation: GesturePoint::new(offset_x as f32, offset_y as f32),
                        velocity: GesturePoint::new(0.0, 0.0),
                        button,
                    };
                    let mut local_env = env_for_handler.clone();
                    local_env.insert(event);
                    call_boxed_action(&action_for_handler, &local_env);
                });
            }
            {
                let env_for_handler = env;
                let action_for_handler = action.clone();
                drag_gesture.connect_drag_end(move |gesture, offset_x, offset_y| {
                    let Some(button) = drag_button.get() else {
                        return;
                    };
                    if !*drag_started.borrow() {
                        return;
                    }
                    let start = gesture.start_point().unwrap_or((0.0, 0.0));
                    let event = DragEvent {
                        phase: GesturePhase::Ended,
                        location: GesturePoint::new(
                            (start.0 + offset_x) as f32,
                            (start.1 + offset_y) as f32,
                        ),
                        translation: GesturePoint::new(offset_x as f32, offset_y as f32),
                        velocity: GesturePoint::new(0.0, 0.0),
                        button,
                    };
                    let mut local_env = env_for_handler.clone();
                    local_env.insert(event);
                    call_boxed_action(&action_for_handler, &local_env);
                });
            }
            widget.add_controller(drag_gesture);
        }
        Gesture::Magnification(_) => {
            let zoom = gtk4::GestureZoom::new();
            let env_for_handler = env;
            let action_for_handler = action.clone();
            zoom.connect_scale_changed(move |gesture, scale| {
                let center = gesture.bounding_box().map_or_else(
                    || GesturePoint::new(0.0, 0.0),
                    |bbox| GesturePoint::new(bbox.x() as f32, bbox.y() as f32),
                );
                let event = MagnificationEvent {
                    phase: GesturePhase::Updated,
                    center,
                    scale: scale as f32,
                    velocity: 0.0,
                };
                let mut local_env = env_for_handler.clone();
                local_env.insert(event);
                call_boxed_action(&action_for_handler, &local_env);
                gesture.set_state(gtk4::EventSequenceState::Claimed);
            });
            widget.add_controller(zoom);
        }
        Gesture::Rotation(_) => {
            let rotate = gtk4::GestureRotate::new();
            let env_for_handler = env;
            let action_for_handler = action.clone();
            rotate.connect_angle_changed(move |gesture, angle, _delta| {
                let center = gesture.bounding_box().map_or_else(
                    || GesturePoint::new(0.0, 0.0),
                    |bbox| GesturePoint::new(bbox.x() as f32, bbox.y() as f32),
                );
                let event = RotationEvent {
                    phase: GesturePhase::Updated,
                    center,
                    angle: angle as f32,
                    // GTK reports no angular velocity, matching the zoom gesture.
                    velocity: 0.0,
                };
                let mut local_env = env_for_handler.clone();
                local_env.insert(event);
                call_boxed_action(&action_for_handler, &local_env);
                gesture.set_state(gtk4::EventSequenceState::Claimed);
            });
            widget.add_controller(rotate);
        }
        Gesture::Then(then) => {
            let armed = Rc::new(RefCell::new(false));
            let first_action: Rc<RefCell<BoxedAction<()>>> = Rc::new(RefCell::new(Box::new({
                let armed = armed.clone();
                move |_env: &Environment| {
                    *armed.borrow_mut() = true;
                }
            })));
            let second_action: Rc<RefCell<BoxedAction<()>>> = Rc::new(RefCell::new(Box::new({
                let chained_action = action.clone();
                move |env: &Environment| {
                    if !*armed.borrow() {
                        return;
                    }
                    *armed.borrow_mut() = false;
                    call_boxed_action(&chained_action, env);
                }
            })));
            install_gesture_observer(widget, then.first().clone(), first_action, env.clone());
            install_gesture_observer(widget, then.then().clone(), second_action, env);
        }
        Gesture::Simultaneous(simultaneous) => {
            install_gesture_observer(
                widget,
                simultaneous.first().clone(),
                action.clone(),
                env.clone(),
            );
            install_gesture_observer(widget, simultaneous.second().clone(), action, env);
        }
        Gesture::Exclusive(exclusive) => {
            let consumed = Rc::new(RefCell::new(false));
            let first_action: Rc<RefCell<BoxedAction<()>>> = Rc::new(RefCell::new(Box::new({
                let consumed = consumed.clone();
                let shared_action = action.clone();
                move |env: &Environment| {
                    *consumed.borrow_mut() = true;
                    call_boxed_action(&shared_action, env);
                    let consumed = consumed.clone();
                    glib::idle_add_local_once(move || {
                        *consumed.borrow_mut() = false;
                    });
                }
            })));
            let second_action: Rc<RefCell<BoxedAction<()>>> = Rc::new(RefCell::new(Box::new({
                let shared_action = action.clone();
                move |env: &Environment| {
                    if *consumed.borrow() {
                        return;
                    }
                    call_boxed_action(&shared_action, env);
                    let consumed = consumed.clone();
                    glib::idle_add_local_once(move || {
                        *consumed.borrow_mut() = false;
                    });
                }
            })));
            install_gesture_observer(widget, exclusive.first().clone(), first_action, env.clone());
            install_gesture_observer(widget, exclusive.second().clone(), second_action, env);
        }
        _ => panic!("unsupported Gesture variant on GTK backend"),
    }
}

/// GTK renderer that converts `WaterUI` views to GTK widgets.
#[derive(Debug)]
pub struct GtkRenderer {
    dispatcher: ViewDispatcher<(), RenderContext, Widget>,
    /// Stretch axis of the leaf view currently being resolved, as observed by
    /// the first non-transparent handler on the dispatch chain. Containers
    /// probe it through [`render_any_with_axis`](Self::render_any_with_axis):
    /// `View::stretch_axis` on a composite view reports the default `None`
    /// before `body()` expansion, so the axis must be read where dispatch
    /// actually lands — inside the leaf handler — and transparent wrappers
    /// (`Metadata<T>`, `IgnorableMetadata<T>`) must let their content's leaf
    /// answer instead.
    leaf_axis: Cell<Option<StretchAxis>>,
}

impl GtkRenderer {
    /// Creates a new GTK renderer with all component handlers registered.
    #[must_use]
    pub fn new() -> Self {
        let mut dispatcher = ViewDispatcher::new();

        // Register component handlers
        Self::register_components(&mut dispatcher);

        Self {
            dispatcher,
            leaf_axis: Cell::new(None),
        }
    }

    /// Renders a view to a GTK widget.
    pub fn render<V: View>(&mut self, view: V, env: &Environment) -> Widget {
        let ctx = RenderContext::with_renderer(self);
        self.dispatcher.dispatch(view, env, ctx)
    }

    /// Renders an `AnyView` to a GTK widget.
    pub fn render_any(&mut self, view: AnyView, env: &Environment) -> Widget {
        let ctx = RenderContext::with_renderer(self);
        self.dispatcher.dispatch(view, env, ctx)
    }

    /// Renders `view` and reports the resolved leaf's [`StretchAxis`].
    ///
    /// The probe is saved and restored around the render so nested container
    /// renders inside a handler do not disturb an in-flight outer probe.
    /// `None` left by an entirely transparent-but-handlerless chain means the
    /// default, content-sized [`StretchAxis::None`].
    pub fn render_any_with_axis(
        &mut self,
        view: AnyView,
        env: &Environment,
    ) -> (Widget, StretchAxis) {
        let prev = self.leaf_axis.replace(None);
        let widget = self.render_any(view, env);
        let axis = self.leaf_axis.replace(prev).unwrap_or(StretchAxis::None);
        // Record the resolved axis on the widget itself: `SubView` wrappers
        // built over it report through the marker, and a live provider can
        // still override it when the content's claim changes.
        note_reported_axis(&widget, axis);
        (widget, axis)
    }

    fn register_components(dispatcher: &mut ViewDispatcher<(), RenderContext, Widget>) {
        // Register Native<T> wrapped components
        Self::register_native::<TextConfig>(dispatcher);
        Self::register_native::<Spacer>(dispatcher);
        Self::register_native::<FixedContainer>(dispatcher);
        Self::register_native::<LazyContainer>(dispatcher);
        Self::register_native::<BadgeConfig>(dispatcher);
        Self::register_native::<ButtonConfig>(dispatcher);
        Self::register_native::<ToggleConfig>(dispatcher);
        Self::register_native::<SliderConfig>(dispatcher);
        Self::register_native::<ResolvedTextFieldConfig>(dispatcher);
        Self::register_native::<ProgressConfig>(dispatcher);
        Self::register_native::<StepperConfig>(dispatcher);
        Self::register_native::<ScrollView>(dispatcher);
        Self::register_native::<TabsLayout>(dispatcher);
        Self::register_native::<ListConfig>(dispatcher);
        Self::register_native::<SecureFieldConfig>(dispatcher);
        Self::register_native::<PickerConfig>(dispatcher);
        Self::register_native::<DatePickerConfig>(dispatcher);
        Self::register_native::<MultiDatePickerConfig>(dispatcher);
        Self::register_native::<ColorPickerConfig>(dispatcher);
        Self::register_native::<ResolvedMenu>(dispatcher);
        Self::register_native::<SystemIcon>(dispatcher);
        #[cfg(feature = "webview-system")]
        Self::register_native::<WebView>(dispatcher);
        Self::register_native::<Color>(dispatcher);
        Self::register_native::<Gradient>(dispatcher);
        Self::register_native::<ResolvedShape>(dispatcher);
        Self::register_native::<Picture>(dispatcher);

        // Register Dynamic for reactive content
        Self::register::<Native<Dynamic>>(dispatcher);

        // Register the GPU leaf views (rendered producers, retained
        // scenes, submitted external frames) and the filtered-subtree leaf.
        Self::register_native::<GpuContentView>(dispatcher);
        Self::register_native::<SceneView>(dispatcher);
        Self::register_native::<ExternalFrameView>(dispatcher);
        Self::register_native::<FilteredView>(dispatcher);

        // Register views that implement View directly
        Self::register::<Divider>(dispatcher);
        Self::register::<NavigationView>(dispatcher);
        Self::register::<NavigationStack<(), ()>>(dispatcher);
        Self::register::<NavigationSplitLayout>(dispatcher);

        // Register metadata handlers
        Self::register_metadata_handlers(dispatcher);

        // Register Str directly (before it wraps into Native<Str>)
        Self::register_str_handler(dispatcher);

        // Register unit type () as empty widget
        Self::register_unit_handler(dispatcher);
    }

    /// Registers a handler for `Str` that renders it as a GTK Label.
    fn register_str_handler(dispatcher: &mut ViewDispatcher<(), RenderContext, Widget>) {
        Self::register_with_renderer::<Str>(dispatcher, |_renderer, s, _env| {
            let label = gtk4::Label::new(Some(s.as_str()));
            // Let text maintain natural width - layout system handles sizing
            label.upcast()
        });
    }

    /// Registers a handler for unit type `()` as an empty widget.
    fn register_unit_handler(dispatcher: &mut ViewDispatcher<(), RenderContext, Widget>) {
        Self::register_with_renderer::<Native<()>>(dispatcher, |_renderer, _unit, _env| {
            // Return an empty widget (invisible box with no children)
            let empty = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            empty.set_visible(false);
            empty.upcast()
        });
    }

    /// Registers handlers for metadata wrapper views.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "one cohesive widget-construction pass; splitting it would scatter GTK setup order"
    )]
    fn register_metadata_handlers(dispatcher: &mut ViewDispatcher<(), RenderContext, Widget>) {
        use waterui::component::focus::Focused;
        use waterui::filter::Opacity;
        use waterui::gesture::GestureObserver;
        use waterui::style::Shadow;

        // Metadata<Environment> - use provided environment for subtree
        Self::register_transparent::<Metadata<Environment>>(
            dispatcher,
            |renderer, metadata, _env| renderer.render_any(metadata.content, &metadata.value),
        );

        // Metadata<Retain> - keep retained value alive for the widget lifetime
        Self::register_transparent::<Metadata<Retain>>(dispatcher, |renderer, metadata, env| {
            let widget = renderer.render_any(metadata.content, env);
            // SAFETY: `RETAIN_DATA_KEY` is written here only and never read
            // back; the value is stored purely so the widget's destruction
            // drops it, and widget data lives on the GTK main thread.
            unsafe { widget.set_data(RETAIN_DATA_KEY, metadata.value) };
            widget
        });

        // Metadata<LifeCycleHook> - invoke hook on appear/disappear
        Self::register_transparent::<Metadata<LifeCycleHook>>(
            dispatcher,
            |renderer, metadata, env| {
                let widget = renderer.render_any(metadata.content, env);
                match metadata.value.lifecycle() {
                    LifeCycle::Appear => {
                        let mut hook = Some(metadata.value);
                        let env = env.clone();
                        glib::idle_add_local_once(move || {
                            if let Some(hook) = hook.take() {
                                hook.handle(&env);
                            }
                        });
                    }
                    LifeCycle::Disappear => {
                        let hook = Rc::new(RefCell::new(Some(metadata.value)));
                        let env = env.clone();
                        widget.connect_unrealize(move |_| {
                            if let Some(hook) = hook.borrow_mut().take() {
                                hook.handle(&env);
                            }
                        });
                    }
                    _ => panic!("unsupported LifeCycle variant on GTK backend"),
                }
                widget
            },
        );

        // Metadata<Opacity> - apply opacity via GTK widget opacity
        Self::register_transparent::<Metadata<Opacity>>(dispatcher, |renderer, metadata, env| {
            let widget = renderer.render_any(metadata.content, env);
            let alpha = metadata.value.value;
            let (initial, guard) = subscribe_then_get(&alpha, {
                let widget = widget.clone();
                move |ctx| {
                    let alpha = ctx.into_value();
                    let widget = widget.clone();
                    glib::idle_add_local_once(move || {
                        widget.set_opacity(f64::from(alpha));
                    });
                }
            });
            widget.set_opacity(f64::from(initial));
            store_watcher_guard(&widget, Box::new(guard));
            widget
        });

        // Metadata<Shadow> - outset-shadow node of the silhouette
        Self::register_transparent::<Metadata<Shadow>>(dispatcher, |renderer, metadata, env| {
            let content = renderer.render_any(metadata.content, env);
            let shadow = metadata.value;
            let to_rgba = |resolved| {
                let (r, g, b, a) = crate::util::resolved_color_to_srgba_f64(resolved);
                gdk4::RGBA::new(r as f32, g as f32, b as f32, a as f32)
            };
            let color_signal = shadow.color.resolve(env);
            let weak = Rc::new(RefCell::new(
                None::<glib::WeakRef<crate::components::graphics::shadow_widget::WuiShadow>>,
            ));
            let (initial, guard) = subscribe_then_get(&color_signal, {
                let weak = weak.clone();
                move |ctx| {
                    let resolved = ctx.into_value();
                    let weak = weak.clone();
                    glib::idle_add_local_once(move || {
                        if let Some(shadow) =
                            weak.borrow().as_ref().and_then(glib::WeakRef::upgrade)
                        {
                            shadow.set_color(to_rgba(resolved));
                        }
                    });
                }
            });
            let wrapper = crate::components::graphics::shadow_widget::WuiShadow::new(
                shadow.silhouette.kind(),
                shadow.silhouette.commands(),
                shadow.offset.x,
                shadow.offset.y,
                shadow.radius,
                to_rgba(initial),
                &content,
            );
            *weak.borrow_mut() = Some(wrapper.downgrade());
            store_watcher_guard(&wrapper, Box::new(guard));
            wrapper.upcast()
        });

        // Metadata<Focused> - bridge focus state with GTK focus
        Self::register_transparent::<Metadata<Focused>>(dispatcher, |renderer, metadata, env| {
            let widget = renderer.render_any(metadata.content, env);
            attach_focus_metadata(widget, &metadata.value.0)
        });

        // Metadata<Cursor> - update pointer cursor while hovering
        Self::register_transparent::<Metadata<Cursor>>(dispatcher, |renderer, metadata, env| {
            let widget = renderer.render_any(metadata.content, env);
            widget.set_can_target(true);
            let style_signal = metadata.value.style;
            let hovered = Rc::new(Cell::new(false));
            let style_state = Rc::new(Cell::new(None));
            let (initial_style, guard) = subscribe_then_get(&style_signal, {
                let style_state = style_state.clone();
                let hovered = hovered.clone();
                let widget = widget.clone();
                move |ctx| {
                    let style = ctx.into_value();
                    style_state.set(Some(style));
                    if hovered.get() {
                        let widget = widget.clone();
                        glib::idle_add_local_once(move || {
                            widget.set_cursor_from_name(Some(cursor_style_to_gtk_name(style)));
                        });
                    }
                }
            });
            style_state.set(Some(initial_style));
            let motion = gtk4::EventControllerMotion::new();
            {
                let hovered = hovered.clone();
                let widget = widget.clone();
                motion.connect_enter(move |_, _, _| {
                    hovered.set(true);
                    let style = style_state
                        .get()
                        .expect("GTK cursor signal must be initialized before pointer entry");
                    widget.set_cursor_from_name(Some(cursor_style_to_gtk_name(style)));
                });
            }
            {
                let widget = widget.clone();
                motion.connect_leave(move |_| {
                    hovered.set(false);
                    widget.set_cursor_from_name(None);
                });
            }
            widget.add_controller(motion);
            store_watcher_guard(&widget, Box::new(guard));
            widget
        });

        // Metadata<Border> - apply CSS border to a wrapper
        Self::register_transparent::<Metadata<Border>>(dispatcher, |renderer, metadata, env| {
            let content = renderer.render_any(metadata.content, env);
            let wrapper = wrap_for_metadata(&content);
            let scoped_css = attach_css_provider(&wrapper, CSS_CLASS_BORDER);
            let border = metadata.value;
            let color_signal = border.color.resolve(env);
            let (initial, guard) = subscribe_then_get(&color_signal, {
                let scoped_css = scoped_css.clone();
                move |ctx| {
                    let resolved = ctx.into_value();
                    let scoped_css = scoped_css.clone();
                    glib::idle_add_local_once(move || {
                        apply_border_css(
                            &scoped_css,
                            resolved,
                            border.width,
                            border.corner_radius,
                            border.edges,
                        );
                    });
                }
            });
            apply_border_css(
                &scoped_css,
                initial,
                border.width,
                border.corner_radius,
                border.edges,
            );
            store_watcher_guard(&wrapper, Box::new(guard));
            wrapper.upcast()
        });

        // Metadata<Scale> - visual scale transform wrapper
        Self::register_transparent::<Metadata<Scale>>(dispatcher, |renderer, metadata, env| {
            let content = renderer.render_any(metadata.content, env);
            let wrapper = wrap_for_metadata(&content);
            let scoped_css = attach_css_provider(&wrapper, CSS_CLASS_SCALE);
            let scale = metadata.value;
            let values = Rc::new(ReactiveAxisPair::default());
            let (initial_x, x_guard) = subscribe_then_get(&scale.x, {
                let scoped_css = scoped_css.clone();
                let values = values.clone();
                move |ctx| {
                    values.update_x(ctx.into_value());
                    if let Some((x, y)) = values.values() {
                        let scoped_css = scoped_css.clone();
                        glib::idle_add_local_once(move || {
                            apply_scale_css(&scoped_css, x, y, scale.anchor);
                        });
                    }
                }
            });
            values.update_x(initial_x);
            let (initial_y, y_guard) = subscribe_then_get(&scale.y, {
                let scoped_css = scoped_css.clone();
                let values = values.clone();
                move |ctx| {
                    values.update_y(ctx.into_value());
                    if let Some((x, y)) = values.values() {
                        let scoped_css = scoped_css.clone();
                        glib::idle_add_local_once(move || {
                            apply_scale_css(&scoped_css, x, y, scale.anchor);
                        });
                    }
                }
            });
            values.update_y(initial_y);
            let (x, y) = values
                .values()
                .expect("GTK scale signals must be initialized after subscription");
            apply_scale_css(&scoped_css, x, y, scale.anchor);
            crate::util::store_watcher_guards(&wrapper, vec![Box::new(x_guard), Box::new(y_guard)]);
            wrapper.upcast()
        });

        // Metadata<Rotation> - visual rotation transform wrapper
        Self::register_transparent::<Metadata<Rotation>>(dispatcher, |renderer, metadata, env| {
            let content = renderer.render_any(metadata.content, env);
            let wrapper = wrap_for_metadata(&content);
            let scoped_css = attach_css_provider(&wrapper, CSS_CLASS_ROTATION);
            let rotation = metadata.value;
            let (initial, guard) = subscribe_then_get(&rotation.angle, {
                let scoped_css = scoped_css.clone();
                move |ctx| {
                    let angle = ctx.into_value();
                    let scoped_css = scoped_css.clone();
                    glib::idle_add_local_once(move || {
                        apply_rotation_css(&scoped_css, angle, rotation.anchor);
                    });
                }
            });
            apply_rotation_css(&scoped_css, initial, rotation.anchor);
            store_watcher_guard(&wrapper, Box::new(guard));
            wrapper.upcast()
        });

        // Metadata<Offset> - visual translate transform wrapper
        Self::register_transparent::<Metadata<Offset>>(dispatcher, |renderer, metadata, env| {
            let content = renderer.render_any(metadata.content, env);
            let wrapper = wrap_for_metadata(&content);
            let scoped_css = attach_css_provider(&wrapper, CSS_CLASS_OFFSET);
            let offset = metadata.value;
            let values = Rc::new(ReactiveAxisPair::default());
            let (initial_x, x_guard) = subscribe_then_get(&offset.x, {
                let scoped_css = scoped_css.clone();
                let values = values.clone();
                move |ctx| {
                    values.update_x(ctx.into_value());
                    if let Some((x, y)) = values.values() {
                        let scoped_css = scoped_css.clone();
                        glib::idle_add_local_once(move || {
                            apply_offset_css(&scoped_css, x, y);
                        });
                    }
                }
            });
            values.update_x(initial_x);
            let (initial_y, y_guard) = subscribe_then_get(&offset.y, {
                let scoped_css = scoped_css.clone();
                let values = values.clone();
                move |ctx| {
                    values.update_y(ctx.into_value());
                    if let Some((x, y)) = values.values() {
                        let scoped_css = scoped_css.clone();
                        glib::idle_add_local_once(move || {
                            apply_offset_css(&scoped_css, x, y);
                        });
                    }
                }
            });
            values.update_y(initial_y);
            let (x, y) = values
                .values()
                .expect("GTK offset signals must be initialized after subscription");
            apply_offset_css(&scoped_css, x, y);
            crate::util::store_watcher_guards(&wrapper, vec![Box::new(x_guard), Box::new(y_guard)]);
            wrapper.upcast()
        });

        // Metadata<ClipShape> - clip content to a shape
        Self::register_transparent::<Metadata<ClipShape>>(dispatcher, |renderer, metadata, env| {
            let content = renderer.render_any(metadata.content, env);
            // The clip resolves the shape's `ShapeKind` against the
            // allocated size at snapshot time. CSS cannot: a percentage
            // `border-radius` resolves per axis, so it turns every round
            // corner on a non-square surface into an elliptical one (#157).
            // The commands are only read for a custom path (#389).
            WuiClipShape::new(metadata.value.kind(), metadata.value.commands(), &content).upcast()
        });

        // Metadata<Secure> - passthrough (GTK cannot enforce screenshot protection)
        Self::register_passthrough_metadata::<Secure>(dispatcher);

        Self::register_transparent::<Metadata<StandardDynamicRange>>(
            dispatcher,
            |renderer, metadata, env| {
                let content = renderer.render_any(metadata.content, env);
                let wrapper = wrap_for_metadata(&content);
                wrapper.add_css_class(CSS_CLASS_DYNAMIC_RANGE_SDR);
                wrapper.upcast()
            },
        );
        Self::register_transparent::<Metadata<HighDynamicRange>>(
            dispatcher,
            |renderer, metadata, env| {
                let content = renderer.render_any(metadata.content, env);
                let wrapper = wrap_for_metadata(&content);
                wrapper.add_css_class(CSS_CLASS_DYNAMIC_RANGE_HDR);
                wrapper.upcast()
            },
        );

        // Metadata<OnEvent> - handle hover events
        Self::register_transparent::<Metadata<OnEvent>>(dispatcher, |renderer, metadata, env| {
            let widget = renderer.render_any(metadata.content, env);
            widget.set_can_target(true);
            let expected = metadata.value.event();
            let handler = Rc::new(RefCell::new(metadata.value));
            let env = env.clone();
            let motion = gtk4::EventControllerMotion::new();
            match expected {
                Event::HoverEnter => {
                    let env = env;
                    let handler = handler;
                    motion.connect_enter(move |_, _, _| {
                        if let Ok(mut on_event) = handler.try_borrow_mut() {
                            on_event.handle(&env);
                        }
                    });
                }
                Event::HoverMove => {
                    let env = env;
                    let handler = handler;
                    motion.connect_motion(move |_, x, y| {
                        if let Ok(mut on_event) = handler.try_borrow_mut() {
                            let hover_env = env.extending(HoverEvent::new(
                                waterui_core::layout::Point::new(x as f32, y as f32),
                            ));
                            on_event.handle(&hover_env);
                        }
                    });
                }
                Event::HoverExit => {
                    let env = env;
                    let handler = handler;
                    motion.connect_leave(move |_| {
                        if let Ok(mut on_event) = handler.try_borrow_mut() {
                            on_event.handle(&env);
                        }
                    });
                }
                _ => panic!("unsupported OnEvent variant on GTK backend"),
            }
            widget.add_controller(motion);
            widget
        });

        // Metadata<OnKeyPress> - claim unconsumed keys as they bubble up
        Self::register_transparent::<Metadata<OnKeyPress>>(
            dispatcher,
            |renderer, metadata, env| {
                let widget = renderer.render_any(metadata.content, env);
                let handler = Rc::new(RefCell::new(metadata.value));
                let env = env.clone();
                let keys = gtk4::EventControllerKey::new();
                // Bubble phase: the controller sees a key press only after the
                // focused descendant left it unconsumed, and each ancestor
                // handles it nearest-first as the event climbs toward the
                // toplevel. Stopping propagation ends the bubble.
                keys.set_propagation_phase(gtk4::PropagationPhase::Bubble);
                keys.connect_key_pressed(move |_, keyval, keycode, state| {
                    let press = KeyPress {
                        key: browser_input::surface_key(keyval),
                        code: browser_input::surface_code(keycode),
                        modifiers: browser_input::surface_modifiers(state),
                        // GDK's key controller does not report auto-repeat.
                        repeat: false,
                    };
                    if let Ok(mut handler) = handler.try_borrow_mut()
                        && handler.handle(&env.extending(press)) == KeyHandling::Handled
                    {
                        return Propagation::Stop;
                    }
                    Propagation::Proceed
                });
                widget.add_controller(keys);
                widget
            },
        );

        // Metadata<GestureObserver> - attach gesture recognizers
        Self::register_transparent::<Metadata<GestureObserver>>(
            dispatcher,
            |renderer, metadata, env| {
                let widget = renderer.render_any(metadata.content, env);
                widget.set_can_target(true);
                let action = Rc::new(RefCell::new(metadata.value.action));
                install_gesture_observer(&widget, metadata.value.gesture, action, env.clone());
                widget
            },
        );

        // Metadata<ResolvedContextMenu> - right-click popover menu
        Self::register_transparent::<Metadata<ResolvedContextMenu>>(
            dispatcher,
            |renderer, metadata, env| {
                let widget = renderer.render_any(metadata.content, env);
                widget.set_can_target(true);
                let items = metadata.value.items;
                let popover_state: Rc<RefCell<Option<gtk4::Popover>>> = Rc::new(RefCell::new(None));
                let click = gtk4::GestureClick::new();
                click.set_button(3);
                click.connect_pressed({
                    let widget = widget.clone();
                    let env = env.clone();
                    let popover_state = popover_state;
                    move |_, _, x, y| {
                        let entries = items.snapshot();
                        if entries.is_empty() {
                            return;
                        }
                        if let Some(existing) = popover_state.borrow_mut().take() {
                            existing.popdown();
                        }
                        let popover = gtk4::Popover::new();
                        popover.set_has_arrow(true);
                        popover.set_parent(&widget);
                        popover
                            .set_pointing_to(Some(&gdk4::Rectangle::new(x as i32, y as i32, 1, 1)));
                        rebuild_menu_popover(&popover, entries, &env);
                        popover.popup();
                        *popover_state.borrow_mut() = Some(popover);
                    }
                });
                widget.add_controller(click);
                widget
            },
        );

        // Metadata<AnchoredOverlay> - binding-driven anchored popover
        Self::register_transparent::<Metadata<AnchoredOverlay>>(
            dispatcher,
            crate::components::anchored_overlay::render_anchored_overlay,
        );

        // Metadata<Draggable> - native GTK drag source
        Self::register_transparent::<Metadata<Draggable>>(dispatcher, |renderer, metadata, env| {
            let widget = renderer.render_any(metadata.content, env);
            widget.set_can_target(true);
            let draggable = Rc::new(metadata.value);
            // `prepare` and `drag_begin` are separate signals; the payload is
            // snapshotted once in `prepare` so the pasteboard content and the
            // registered typed payload agree.
            let prepared = Rc::new(RefCell::new(None::<DragPayload>));
            let source = gtk4::DragSource::new();
            source.set_actions(gdk4::DragAction::COPY);
            source.connect_prepare({
                let draggable = draggable.clone();
                let prepared = prepared.clone();
                move |_, _, _| {
                    let payload = draggable.payload();
                    *prepared.borrow_mut() = Some(payload.clone());
                    Some(drag_content_provider(&payload))
                }
            });
            source.connect_drag_begin(move |_, drag| {
                let payload = prepared
                    .borrow_mut()
                    .take()
                    .unwrap_or_else(|| draggable.payload());
                register_drag_payload(drag, payload);
            });
            source.connect_drag_end(|_, drag, _| unregister_drag_payload(drag));
            widget.add_controller(source);
            widget
        });

        // Metadata<DropDestination> - native GTK drop target
        Self::register_transparent::<Metadata<DropDestination>>(
            dispatcher,
            |renderer, metadata, env| {
                let widget = renderer.render_any(metadata.content, env);
                widget.set_can_target(true);
                let destination = Rc::new(RefCell::new(metadata.value));
                // `enter`/`exit` fire only while an accepted drag is inside.
                let hovered = Rc::new(Cell::new(false));
                let kind = destination.borrow().accepted_kind();
                if kind.is_platform() {
                    // The GTypes gate the drop's formats: `String`/`GString`
                    // reach text mimes, `FileList` `text/uri-list`. `Url`
                    // destinations take `FileList` too so a file drop delivers
                    // its `file://` URL.
                    let gtypes: &[glib::Type] = match kind {
                        TransferKind::Files => &[
                            gdk4::FileList::static_type(),
                            String::static_type(),
                            glib::GString::static_type(),
                        ],
                        TransferKind::Url => &[
                            String::static_type(),
                            glib::GString::static_type(),
                            gdk4::FileList::static_type(),
                        ],
                        _ => &[String::static_type(), glib::GString::static_type()],
                    };
                    let target = gtk4::DropTarget::new(
                        gtypes[0],
                        gdk4::DragAction::COPY | gdk4::DragAction::MOVE,
                    );
                    target.set_types(gtypes);
                    target.connect_enter({
                        let destination = destination.clone();
                        let hovered = hovered.clone();
                        let env = env.clone();
                        move |target, _, _| {
                            let accepts = target.current_drop().is_some_and(|drop| {
                                drop_destination_accepts(&drop, &destination.borrow())
                            });
                            if accepts {
                                hovered.set(true);
                                if let Ok(mut destination) = destination.try_borrow_mut() {
                                    destination.enter(&env);
                                }
                                gdk4::DragAction::COPY
                            } else {
                                gdk4::DragAction::empty()
                            }
                        }
                    });
                    target.connect_leave({
                        let destination = destination.clone();
                        let hovered = hovered.clone();
                        let env = env.clone();
                        move |_| {
                            if hovered.replace(false)
                                && let Ok(mut destination) = destination.try_borrow_mut()
                            {
                                destination.exit(&env);
                            }
                        }
                    });
                    target.connect_drop({
                        let env = env.clone();
                        move |target, value, _, _| {
                            hovered.set(false);
                            let Ok(mut destination) = destination.try_borrow_mut() else {
                                return false;
                            };
                            if let Some(drop) = target.current_drop()
                                && let Some(payload) = local_drag_payload(&drop)
                            {
                                if destination.accepts(&payload) {
                                    destination.deliver(payload, &env);
                                    return true;
                                }
                                return false;
                            }
                            let kind = destination.accepted_kind();
                            let Some(payload) = payload_from_drop_value(value, kind) else {
                                return false;
                            };
                            destination.deliver(payload, &env);
                            true
                        }
                    });
                    widget.add_controller(target);
                } else {
                    // `InProcess` kinds have no pasteboard form, so the target
                    // watches the private MIME the source writes and the
                    // payload is looked up by `GdkDrag` — it can never arrive
                    // from another process.
                    let target = gtk4::DropTargetAsync::new(
                        Some(gdk4::ContentFormats::new(&[IN_PROCESS_DRAG_MIME])),
                        gdk4::DragAction::COPY | gdk4::DragAction::MOVE,
                    );
                    target.connect_accept({
                        let destination = destination.clone();
                        move |_, drop| {
                            local_drag_payload(drop)
                                .is_some_and(|payload| destination.borrow().accepts(&payload))
                        }
                    });
                    target.connect_drag_enter({
                        let destination = destination.clone();
                        let hovered = hovered.clone();
                        let env = env.clone();
                        move |_, drop, _, _| {
                            let accepts = local_drag_payload(drop)
                                .is_some_and(|payload| destination.borrow().accepts(&payload));
                            if accepts {
                                hovered.set(true);
                                if let Ok(mut destination) = destination.try_borrow_mut() {
                                    destination.enter(&env);
                                }
                                gdk4::DragAction::COPY
                            } else {
                                gdk4::DragAction::empty()
                            }
                        }
                    });
                    target.connect_drag_motion({
                        let destination = destination.clone();
                        move |_, drop, _, _| {
                            if local_drag_payload(drop)
                                .is_some_and(|payload| destination.borrow().accepts(&payload))
                            {
                                gdk4::DragAction::COPY
                            } else {
                                gdk4::DragAction::empty()
                            }
                        }
                    });
                    target.connect_drag_leave({
                        let destination = destination.clone();
                        let hovered = hovered.clone();
                        let env = env.clone();
                        move |_, _| {
                            if hovered.replace(false)
                                && let Ok(mut destination) = destination.try_borrow_mut()
                            {
                                destination.exit(&env);
                            }
                        }
                    });
                    target.connect_drop({
                        let env = env.clone();
                        move |_, drop, _, _| {
                            hovered.set(false);
                            let Ok(mut destination) = destination.try_borrow_mut() else {
                                return false;
                            };
                            let Some(payload) = local_drag_payload(drop) else {
                                return false;
                            };
                            if !destination.accepts(&payload) {
                                return false;
                            }
                            destination.deliver(payload, &env);
                            drop.finish(gdk4::DragAction::COPY);
                            true
                        }
                    });
                    widget.add_controller(target);
                }
                widget
            },
        );

        // Metadata<Hittable> - control hit testing and interaction
        Self::register_transparent::<Metadata<Hittable>>(dispatcher, |renderer, metadata, env| {
            let widget = renderer.render_any(metadata.content, env);
            let enabled = metadata.value.enabled;
            let (initial, guard) = subscribe_then_get(&enabled, {
                let widget = widget.clone();
                move |ctx| {
                    let enabled = ctx.into_value();
                    let widget = widget.clone();
                    glib::idle_add_local_once(move || {
                        widget.set_can_target(enabled);
                        widget.set_sensitive(enabled);
                    });
                }
            });
            widget.set_can_target(initial);
            widget.set_sensitive(initial);
            store_watcher_guard(&widget, Box::new(guard));
            widget
        });

        // Metadata<IgnoreSafeArea> - passthrough on GTK windowing model
        Self::register_passthrough_metadata::<IgnoreSafeArea>(dispatcher);

        // Metadata<Background> - passthrough for compatibility with metadata-based callers
        Self::register_passthrough_metadata::<Background>(dispatcher);

        Self::register_passthrough_metadata::<NavigationTransitionSource>(dispatcher);
        Self::register_passthrough_metadata::<NavigationTransitionDestination>(dispatcher);

        // Metadata<LayoutPriority> - override the content's layout priority
        Self::register_transparent::<Metadata<LayoutPriority>>(
            dispatcher,
            |renderer, metadata, env| {
                let widget = renderer.render_any(metadata.content, env);
                // Rendered after the content, so this overrides the default a
                // host like `Spacer` recorded on the same widget.
                set_layout_priority(&widget, metadata.value.get());
                widget
            },
        );

        // Native<FilteredView> - handled by applied_filter.rs, which
        // captures the child through the snapshot pipeline, runs the
        // effect on wgpu, and presents the filtered GL texture through
        // GTK's share-group compositor.

        // Ignorable metadata with no native semantic realization.
        Self::register_ignorable_metadata::<MaterialBackground>(dispatcher);

        Self::register_transparent::<IgnorableMetadata<AccessibilityLabel>>(
            dispatcher,
            |renderer, metadata, env| {
                let widget = renderer.render_any(metadata.content, env);
                let weak = widget.downgrade();
                let (initial, guard) = subscribe_then_get(metadata.value.signal(), move |ctx| {
                    if let Some(widget) = weak.upgrade() {
                        let label = ctx.into_value();
                        widget
                            .update_property(&[gtk4::accessible::Property::Label(label.as_str())]);
                    }
                });
                widget.update_property(&[gtk4::accessible::Property::Label(initial.as_str())]);
                store_watcher_guard(&widget, Box::new(guard));
                widget
            },
        );
        Self::register_transparent::<IgnorableMetadata<AccessibilityValue>>(
            dispatcher,
            |renderer, metadata, env| {
                let widget = renderer.render_any(metadata.content, env);
                let weak = widget.downgrade();
                let (initial, guard) = subscribe_then_get(metadata.value.signal(), move |ctx| {
                    if let Some(widget) = weak.upgrade() {
                        let value = ctx.into_value();
                        widget.update_property(&[gtk4::accessible::Property::Description(
                            value.as_str(),
                        )]);
                    }
                });
                widget
                    .update_property(&[gtk4::accessible::Property::Description(initial.as_str())]);
                store_watcher_guard(&widget, Box::new(guard));
                widget
            },
        );
        Self::register_transparent::<IgnorableMetadata<AccessibilityRole>>(
            dispatcher,
            |renderer, metadata, env| {
                let widget = renderer.render_any(metadata.content, env);
                widget.set_accessible_role(gtk_accessibility_role(&metadata.value));
                widget
            },
        );
        Self::register_transparent::<IgnorableMetadata<AccessibilityHidden>>(
            dispatcher,
            |renderer, metadata, env| {
                let widget = renderer.render_any(metadata.content, env);
                widget.update_state(&[gtk4::accessible::State::Hidden(metadata.value.is_hidden())]);
                widget
            },
        );
        Self::register_transparent::<IgnorableMetadata<AccessibilityChildren>>(
            dispatcher,
            |renderer, metadata, env| {
                let child = renderer.render_any(metadata.content, env);
                if !metadata.value.excludes_descendants() {
                    return child;
                }
                child.set_accessible_role(gtk4::AccessibleRole::Group);
                let mut descendant = child.first_child();
                while let Some(widget) = descendant {
                    widget.update_state(&[gtk4::accessible::State::Hidden(true)]);
                    descendant = widget.next_sibling();
                }
                child
            },
        );
        Self::register_transparent::<IgnorableMetadata<AccessibilityState>>(
            dispatcher,
            |renderer, metadata, env| {
                let widget = renderer.render_any(metadata.content, env);
                apply_gtk_accessibility_state(&widget, &metadata.value);
                widget
            },
        );
        Self::register_transparent::<IgnorableMetadata<AccessibilityStateSignal>>(
            dispatcher,
            |renderer, metadata, env| {
                let widget = renderer.render_any(metadata.content, env);
                let weak = widget.downgrade();
                let (initial, guard) = subscribe_then_get(metadata.value.state(), move |ctx| {
                    if let Some(widget) = weak.upgrade() {
                        apply_gtk_accessibility_state(&widget, &ctx.into_value());
                    }
                });
                apply_gtk_accessibility_state(&widget, &initial);
                store_watcher_guard(&widget, Box::new(guard));
                widget
            },
        );
    }

    /// Registers a handler for `Metadata<T>` that renders the content
    /// unchanged because the metadata has no GTK realization.
    fn register_passthrough_metadata<T: MetadataKey>(
        dispatcher: &mut ViewDispatcher<(), RenderContext, Widget>,
    ) where
        Metadata<T>: View,
    {
        Self::register_transparent::<Metadata<T>>(dispatcher, |renderer, metadata, env| {
            renderer.render_any(metadata.content, env)
        });
    }

    /// Registers a handler that receives the dispatching [`GtkRenderer`]
    /// directly, so handlers can recurse without touching the raw context
    /// pointer themselves.
    ///
    /// `V` counts as a leaf for stretch-axis probing: when
    /// [`render_any_with_axis`](Self::render_any_with_axis) is in flight, the
    /// first leaf handler on the resolution chain records `V::stretch_axis`.
    fn register_with_renderer<V: View>(
        dispatcher: &mut ViewDispatcher<(), RenderContext, Widget>,
        handler: impl 'static + Clone + Fn(&mut Self, V, &Environment) -> Widget,
    ) {
        Self::register_dispatch(dispatcher, false, handler);
    }

    /// Registers a handler for a view that is transparent to layout:
    /// `Metadata<T>` and `IgnorableMetadata<T>` wrappers decorate or observe
    /// their content but never change its size, so the stretch-axis probe
    /// keeps searching for the content's leaf instead of recording the
    /// wrapper's axis.
    fn register_transparent<V: View>(
        dispatcher: &mut ViewDispatcher<(), RenderContext, Widget>,
        handler: impl 'static + Clone + Fn(&mut Self, V, &Environment) -> Widget,
    ) {
        Self::register_dispatch(dispatcher, true, handler);
    }

    /// Shared registration for [`register_with_renderer`] (leaf) and
    /// [`register_transparent`] (layout-transparent wrapper).
    fn register_dispatch<V: View>(
        dispatcher: &mut ViewDispatcher<(), RenderContext, Widget>,
        transparent: bool,
        handler: impl 'static + Clone + Fn(&mut Self, V, &Environment) -> Widget,
    ) {
        dispatcher.register::<V>(move |_state, ctx, view, env| {
            // SAFETY: every `RenderContext` is created by
            // `GtkRenderer::render`/`render_any` from a live `&mut GtkRenderer`
            // immediately before the synchronous `dispatch` call that invokes
            // this handler, and rendering runs exclusively on the GTK main
            // thread, so the pointer is valid for the whole handler invocation
            // and this is the only renderer reference used during it.
            let renderer = unsafe { ctx.renderer() };
            if !transparent && renderer.leaf_axis.get().is_none() {
                renderer.leaf_axis.set(Some(view.stretch_axis()));
            }
            handler(renderer, view, env)
        });
    }

    /// Registers a handler for `IgnorableMetadata<T>` that just renders the content.
    fn register_ignorable_metadata<T: MetadataKey>(
        dispatcher: &mut ViewDispatcher<(), RenderContext, Widget>,
    ) {
        Self::register_transparent::<IgnorableMetadata<T>>(
            dispatcher,
            |renderer, metadata, env| renderer.render_any(metadata.content, env),
        );
    }

    /// Registers a `Native<T>` wrapped component with the dispatcher.
    fn register_native<T: waterui_core::NativeView + 'static>(
        dispatcher: &mut ViewDispatcher<(), RenderContext, Widget>,
    ) where
        Native<T>: GtkComponent,
    {
        Self::register::<Native<T>>(dispatcher);
    }

    /// Registers a `GtkComponent` view type with the dispatcher.
    fn register<V: GtkComponent>(dispatcher: &mut ViewDispatcher<(), RenderContext, Widget>) {
        Self::register_with_renderer::<V>(dispatcher, |renderer, view, env| {
            view.render(env, renderer)
        });
    }
}

impl Default for GtkRenderer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod focus_tests {
    use std::time::{Duration, Instant};

    use glib::MainContext;

    use super::*;

    fn init() {
        gtk4::init().expect("GTK tests need a display; run them under xvfb-run");
    }

    /// Pumps the default main context until `condition` holds, one iteration
    /// at a time so GTK's map/focus event delivery gets to run.
    fn wait_until(mut condition: impl FnMut() -> bool) {
        let context = MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for GTK state");
            context.iteration(true);
        }
    }

    /// Whether `anchor` still carries a deferred focus request.
    fn has_pending_focus_request(anchor: &impl ObjectExt) -> bool {
        // SAFETY: `FOCUS_REQUEST_PENDING_DATA_KEY` only ever stores
        // `PendingFocusRequest`, and the returned pointer is discarded after
        // the presence check; widget data is only touched on this thread.
        unsafe { anchor.data::<PendingFocusRequest>(FOCUS_REQUEST_PENDING_DATA_KEY) }.is_some()
    }

    /// A vertical box wrapping a marked `Entry`, mirroring the shape the real
    /// `TextField` component emits (`container > label + marked entry`).
    fn labelled_entry() -> (gtk4::Box, gtk4::Entry) {
        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        container.append(&gtk4::Label::new(Some("Field")));
        let entry = gtk4::Entry::new();
        mark_focus_anchor(&entry);
        container.append(&entry);
        (container, entry)
    }

    /// Whether the window's surface reports the toplevel `FOCUSED` state.
    /// `is-active` instead tracks the WM's `_NET_ACTIVE_WINDOW` hint and stays
    /// false without a window manager, so the surface state is the condition
    /// that matters under bare Xvfb.
    fn toplevel_focused(window: &gtk4::Window) -> bool {
        window
            .surface()
            .and_then(|surface| surface.downcast::<gdk4::Toplevel>().ok())
            .is_some_and(|toplevel| toplevel.state().contains(gdk4::ToplevelState::FOCUSED))
    }

    /// Presents `window` with `child` and waits until the toplevel is mapped
    /// and holds keyboard focus. Under bare Xvfb there is no window manager,
    /// so GDK's autofocus grants input focus to the mapped toplevel itself.
    fn present_active(window: &gtk4::Window, child: &Widget) {
        window.set_child(Some(child));
        window.present();
        wait_until(|| window.is_mapped() && toplevel_focused(window));
    }

    /// Whether `anchor` or one of its descendants holds keyboard focus in
    /// `window`, checked through the root's focus widget rather than
    /// `has_focus` on the anchor: `gtk4::Entry` delegates focus to its
    /// private `GtkText` and internal children are not reported by
    /// `is_ancestor`, so `FOCUS_WITHIN` on the anchor is the observable
    /// contract.
    fn focus_inside(_window: &gtk4::Window, anchor: &gtk4::Entry) -> bool {
        anchor_holds_focus(&anchor.clone().upcast::<Widget>())
    }

    #[test]
    fn resolves_the_single_marked_anchor_in_a_subtree() {
        init();
        let (container, entry) = labelled_entry();
        let resolved = resolve_single_focus_anchor(&container.upcast());
        assert_eq!(resolved, entry.upcast::<Widget>());
    }

    #[test]
    #[should_panic(expected = "requires exactly one TextField or SecureField")]
    fn attach_without_a_focus_anchor_panics() {
        init();
        let binding = Binding::container(false);
        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.append(&gtk4::Label::new(Some("no anchor")));
        let _ = attach_focus_metadata(container.upcast(), &binding);
    }

    #[test]
    #[should_panic(expected = "found 2")]
    fn attach_with_two_focus_anchors_panics() {
        init();
        let binding = Binding::container(false);
        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        let first = gtk4::Entry::new();
        let second = gtk4::Entry::new();
        mark_focus_anchor(&first);
        mark_focus_anchor(&second);
        container.append(&first);
        container.append(&second);
        let _ = attach_focus_metadata(container.upcast(), &binding);
    }

    #[test]
    fn binding_writes_move_platform_focus() {
        init();
        let binding_a = Binding::container(false);
        let binding_b = Binding::container(false);
        let (container_a, entry_a) = labelled_entry();
        let (container_b, entry_b) = labelled_entry();
        let widget_a = attach_focus_metadata(container_a.upcast(), &binding_a);
        let widget_b = attach_focus_metadata(container_b.upcast(), &binding_b);

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        vbox.append(&widget_a);
        vbox.append(&widget_b);
        let window = gtk4::Window::new();
        present_active(&window, &vbox.upcast());

        // GTK focuses the first focusable child on map; the write-back turns
        // binding A on without any explicit request.
        wait_until(|| focus_inside(&window, &entry_a));
        wait_until(|| binding_a.snapshot());
        assert!(!binding_b.snapshot());

        binding_b.set(true);
        wait_until(|| focus_inside(&window, &entry_b));
        wait_until(|| !focus_inside(&window, &entry_a));
        // A losing focus writes false back through its own binding.
        wait_until(|| !binding_a.snapshot());

        binding_b.set(false);
        wait_until(|| !focus_inside(&window, &entry_b));
    }

    #[test]
    fn platform_focus_updates_the_binding() {
        init();
        let binding = Binding::container(false);
        let (container, entry) = labelled_entry();
        let widget = attach_focus_metadata(container.upcast(), &binding);

        let window = gtk4::Window::new();
        present_active(&window, &widget);

        // GTK auto-focuses the first focusable child on map; the anchor's
        // state-flags observer must write that back to the binding.
        wait_until(|| binding.snapshot());
        assert!(focus_inside(&window, &entry));

        gtk4::prelude::RootExt::set_focus(&window, None::<&Widget>);
        wait_until(|| !binding.snapshot());

        entry.grab_focus();
        wait_until(|| binding.snapshot());
    }

    #[test]
    fn focus_request_on_an_unmapped_anchor_waits_for_map() {
        init();
        let binding = Binding::container(false);
        let (container, entry) = labelled_entry();
        let widget = attach_focus_metadata(container.upcast(), &binding);

        let window = gtk4::Window::new();
        window.set_child(Some(&widget));
        assert!(!entry.is_mapped());

        binding.set(true);
        assert!(
            has_pending_focus_request(&entry),
            "an unmapped anchor must record a pending focus request"
        );

        window.present();
        wait_until(|| focus_inside(&window, &entry));
        assert!(
            !has_pending_focus_request(&entry),
            "the pending request must be consumed once the anchor is mapped"
        );
    }

    #[test]
    fn initially_focused_binding_defers_until_map() {
        init();
        let binding = Binding::container(true);
        let (container, entry) = labelled_entry();
        let widget = attach_focus_metadata(container.upcast(), &binding);

        assert!(
            has_pending_focus_request(&entry),
            "an unmapped anchor must record the initial request as pending"
        );

        let window = gtk4::Window::new();
        present_active(&window, &widget);
        wait_until(|| focus_inside(&window, &entry));
    }

    #[test]
    fn clearing_a_pending_request_skips_late_map_focus() {
        init();
        let binding = Binding::container(false);
        let (container, entry) = labelled_entry();
        let widget = attach_focus_metadata(container.upcast(), &binding);

        let window = gtk4::Window::new();
        window.set_child(Some(&widget));

        binding.set(true);
        binding.set(false);
        assert!(
            !has_pending_focus_request(&entry),
            "clearing must drop the pending focus request"
        );

        window.present();
        wait_until(|| entry.is_mapped());
        // GTK still applies its own default focus to the first focusable
        // child on map; the write-back reports it through the binding. The
        // pending marker staying cleared is what proves our deferred path
        // did not fire.
        wait_until(|| binding.snapshot());
        assert!(focus_inside(&window, &entry));
    }

    #[test]
    fn repeated_focus_requests_are_idempotent() {
        init();
        let binding = Binding::container(false);
        let (container, entry) = labelled_entry();
        let widget = attach_focus_metadata(container.upcast(), &binding);

        let window = gtk4::Window::new();
        present_active(&window, &widget);

        // GTK auto-focuses the first focusable child on map.
        wait_until(|| focus_inside(&window, &entry));
        wait_until(|| binding.snapshot());

        // A redundant `set(true)` and a direct grab must not disturb the
        // already-satisfied focus state.
        binding.set(true);
        assert!(entry.grab_focus());
        wait_until(|| focus_inside(&window, &entry));
        assert!(binding.snapshot());
    }
}

#[cfg(test)]
mod gesture_button_tests {
    //! A tap observer installs a capture-phase `GtkGestureSingle` whose
    //! `buttons` mask must deny a press it does not accept, releasing the
    //! sequence to the next controller in the chain. GTK4 exposes no public
    //! button-event injection (`gdk4::ButtonEvent` is not constructible and
    //! the `gtk_test_*` entry points are gone), so the press is a real XTEST
    //! click delivered by `xdotool`; the windowed test group serializes tests
    //! that share the display's pointer.

    use std::cell::Cell;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use glib::MainContext;
    use waterui::gesture::{Gesture, PointerButtons, TapGesture};

    use super::*;

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

    fn flag_action(flag: &Rc<Cell<bool>>) -> Rc<RefCell<BoxedAction<()>>> {
        let flag = flag.clone();
        Rc::new(RefCell::new(
            Box::new(move |_: &Environment| flag.set(true)) as BoxedAction<()>,
        ))
    }

    /// Sends `button`'s click to the center of the active window through
    /// XTEST, so GTK dispatches a real button-press sequence.
    fn click_active_window_center(button: u32) {
        let output = Command::new("xdotool")
            .args(["getactivewindow", "getwindowgeometry", "--shell"])
            .output()
            .expect("xdotool must be on PATH to synthesize pointer input");
        assert!(
            output.status.success(),
            "xdotool could not resolve the active window"
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let field = |name: &str| -> i64 {
            stdout
                .lines()
                .find_map(|line| line.strip_prefix(name).and_then(|v| v.trim().parse().ok()))
                .unwrap_or_else(|| panic!("xdotool getwindowgeometry reported no {name}"))
        };
        let (x, y) = (
            field("X=") + field("WIDTH=") / 2,
            field("Y=") + field("HEIGHT=") / 2,
        );
        let activated = Command::new("xdotool")
            .args(["getactivewindow", "windowactivate", "--sync"])
            .status()
            .expect("xdotool must be on PATH to synthesize pointer input");
        assert!(activated.success(), "xdotool could not raise the window");
        let status = Command::new("xdotool")
            .args([
                "mousemove",
                &x.to_string(),
                &y.to_string(),
                "click",
                &button.to_string(),
            ])
            .status()
            .expect("xdotool must be on PATH to synthesize pointer input");
        assert!(status.success(), "xdotool click failed");
    }

    /// A MIDDLE-only tap on the parent and the default PRIMARY tap on its
    /// child are both capture-phase controllers. The parent's middle press
    /// claims the sequence; a primary press must be denied there and reach
    /// the child's controller — before the denial the parent's early return
    /// left the sequence claimed and starved the child.
    #[test]
    fn denied_button_sequence_reaches_child_controller() {
        init();
        let parent = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        let child = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        child.set_hexpand(true);
        child.set_vexpand(true);
        parent.append(&child);

        let parent_fired = Rc::new(Cell::new(false));
        let child_fired = Rc::new(Cell::new(false));
        install_gesture_observer(
            parent.upcast_ref(),
            Gesture::Tap(TapGesture::new().buttons(PointerButtons::MIDDLE)),
            flag_action(&parent_fired),
            Environment::new(),
        );
        install_gesture_observer(
            child.upcast_ref(),
            Gesture::Tap(TapGesture::new()),
            flag_action(&child_fired),
            Environment::new(),
        );

        let window = gtk4::Window::new();
        window.set_default_size(300, 300);
        window.set_child(Some(&parent));
        window.present();
        wait_until(|| window.is_mapped());
        // Give the WM a beat to focus and place the toplevel before XTEST input.
        std::thread::sleep(Duration::from_millis(500));
        wait_until(|| {
            parent
                .compute_bounds(&window)
                .is_some_and(|r| r.width() > 0.0)
        });

        click_active_window_center(2);
        wait_until(|| parent_fired.get());
        assert!(
            !child_fired.get(),
            "the claimed middle press must not reach the child"
        );

        parent_fired.set(false);
        click_active_window_center(1);
        wait_until(|| child_fired.get());
        assert!(
            !parent_fired.get(),
            "the denied primary press must not fire the MIDDLE-only tap"
        );
    }
}
