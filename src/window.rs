//! Window management utilities for GTK backend.

use std::cell::Cell;
use std::rc::Rc;

use gdk4::prelude::ToplevelExt as _;
use glib::object::{Cast as _, ObjectExt as _};
use gtk4::prelude::GtkWindowExt;
use gtk4::{Application, ApplicationWindow};
use nami::{Binding, Computed, Signal};
use num_traits::ToPrimitive as _;
use waterui::window::{WindowState, WindowStyle};
use waterui_core::Environment;
use waterui_graphics::color::ResolvedColor;

use crate::util::{ScopedCss, resolved_color_to_css_rgba, store_watcher_guard, subscribe_then_get};

/// Creates a new application window with the specified properties.
#[must_use]
pub fn create_window(app: &Application, title: &str, width: i32, height: i32) -> ApplicationWindow {
    ApplicationWindow::builder()
        .application(app)
        .title(title)
        .default_width(width)
        .default_height(height)
        .build()
}

/// Offers "Inspect element" on a secondary click anywhere in the window.
///
/// A debug build attaches this to the window itself rather than to any widget,
/// so inspection is reachable from everywhere without an application opting in.
/// A widget with a context menu of its own handles the click first and this
/// never runs, which is the same precedence a browser gives page menus.
///
/// GTK builds native widgets and publishes no accessibility tree to the
/// inspector, so there is no node to name here; the inspector opens on the
/// application. Naming the element as well needs GTK to feed the tree channel.
pub fn install_inspect_gesture(window: &ApplicationWindow, env: &Environment) {
    use gtk4::prelude::*;

    if !cfg!(debug_assertions) {
        return;
    }
    // Nothing is listening in a release build, and an entry that does nothing is
    // worse than no entry.
    if env.get::<waterui::inspector::InspectorRuntime>().is_none() {
        return;
    }

    let click = gtk4::GestureClick::new();
    click.set_button(3);
    // Run after the target widget has had its turn, so an application's own
    // context menu wins.
    click.set_propagation_phase(gtk4::PropagationPhase::Bubble);
    click.connect_pressed({
        let window = window.clone();
        let env = env.clone();
        move |_, _, x, y| {
            if env.get::<waterui::inspector::InspectorRuntime>().is_none() {
                return;
            }
            let popover = gtk4::Popover::new();
            popover.set_has_arrow(true);
            popover.set_parent(&window);
            popover.set_pointing_to(Some(&gdk4::Rectangle::new(
                pointer_coordinate(x),
                pointer_coordinate(y),
                1,
                1,
            )));

            let item = gtk4::Button::with_label("Inspect element");
            item.set_has_frame(false);
            item.connect_clicked({
                let popover = popover.clone();
                // `Environment::get` hands back a reference borrowed from the
                // environment, which cannot escape into this `'static` handler.
                // The environment itself is cheap to clone, so the handler
                // carries one and resolves the runtime when the item is
                // actually clicked.
                let env = env.clone();
                move |_| {
                    popover.popdown();
                    let Some(inspector) = env.get::<waterui::inspector::InspectorRuntime>() else {
                        return;
                    };
                    inspector.open();
                }
            });
            popover.set_child(Some(&item));
            popover.popup();
        }
    });
    window.add_controller(click);
}

/// Rounds a GTK gesture coordinate to the integer pixel `GdkRectangle` takes.
///
/// GTK reports gesture positions as finite widget-local logical pixels, so the
/// rounded value is always well inside `i32`; anything else means GTK handed us
/// a coordinate for a window that cannot exist, which is worth crashing on
/// rather than silently pointing the popover somewhere else.
fn pointer_coordinate(value: f64) -> i32 {
    value
        .round()
        .to_i32()
        .unwrap_or_else(|| panic!("GTK pointer coordinate {value} does not fit i32"))
}

/// Monitor geometry is in application pixels — small integers where the
/// `i32 -> f32` conversion cannot lose a real coordinate; anything else means
/// GTK reported a display that cannot exist, worth crashing on rather than
/// silently planting the window at 0.
fn monitor_coordinate(value: i32) -> f32 {
    value
        .to_f32()
        .unwrap_or_else(|| panic!("GTK monitor coordinate {value} does not fit f32"))
}

/// Whether `display` is the X11 backend — the only backend where the X
/// server's global answers (pointer position, work area, toplevel moves)
/// are reachable through the display's own connection.
fn is_x11_display(display: &gdk4::Display) -> bool {
    use glib::prelude::*;
    display.type_().name() == "GdkX11Display"
}

/// The `Display*` behind a `GdkX11Display`, borrowed from GTK — never a
/// second connection. The caller has already established the backend is
/// X11; a null handle is a bug and panics.
fn x11_xdisplay(display: &gdk4::Display) -> *mut std::ffi::c_void {
    use glib::translate::ToGlibPtr;
    unsafe extern "C" {
        fn gdk_x11_display_get_xdisplay(display: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }
    let display_ptr: *mut gdk4::ffi::GdkDisplay = display.to_glib_none().0;
    // Safety: `display` is a live `GdkX11Display`, so the X display it hands
    // back outlives this call.
    let xdisplay = unsafe { gdk_x11_display_get_xdisplay(display_ptr.cast()) };
    assert!(
        !xdisplay.is_null(),
        "window placement: GdkX11Display returned a null Display*"
    );
    xdisplay
}

/// The pointer's global position on the X11 backend, in root-window
/// (physical) coordinates.
///
/// `Device::surface_at_position` only reports surfaces owned by this process —
/// the pointer over another client's window or the bare root yields nothing —
/// so on X11 the position comes from the X server itself, through the
/// display's own connection. `XQueryPointer` returning false means the
/// pointer sits on another screen's root; the returned coordinates still
/// locate it for the nearest-monitor resolution the caller applies. A null
/// display or an unreadable answer is a bug and panics.
fn x11_root_pointer(display: &gdk4::Display) -> (i32, i32) {
    use std::ffi::c_void;
    use std::os::raw::{c_int, c_ulong};

    type XWindow = c_ulong;

    #[link(name = "X11")]
    unsafe extern "C" {
        fn XDefaultRootWindow(display: *mut c_void) -> XWindow;
        #[allow(clippy::too_many_arguments)]
        fn XQueryPointer(
            display: *mut c_void,
            window: XWindow,
            root_return: *mut XWindow,
            child_return: *mut XWindow,
            root_x_return: *mut c_int,
            root_y_return: *mut c_int,
            win_x_return: *mut c_int,
            win_y_return: *mut c_int,
            mask_return: *mut u32,
        ) -> c_int;
    }

    let xdisplay = x11_xdisplay(display);
    // Safety: `xdisplay` is a live X display borrowed from GTK, and every
    // out-pointer names a live stack slot.
    unsafe {
        let root = XDefaultRootWindow(xdisplay);
        let (mut root_x, mut root_y) = (0, 0);
        let (mut root_win, mut child_win, mut win_x, mut win_y, mut mask) = (0, 0, 0, 0, 0);
        let _on_same_screen = XQueryPointer(
            xdisplay,
            root,
            &raw mut root_win,
            &raw mut child_win,
            &raw mut root_x,
            &raw mut root_y,
            &raw mut win_x,
            &raw mut win_y,
            &raw mut mask,
        );
        (root_x, root_y)
    }
}

/// Squared distance from a point to an axis-aligned rect — zero inside,
/// the gap distance outside.
fn rect_distance_sq(left: f64, top: f64, right: f64, bottom: f64, px: f64, py: f64) -> f64 {
    let dx = if px < left {
        left - px
    } else if px > right {
        px - right
    } else {
        0.0
    };
    let dy = if py < top {
        top - py
    } else if py > bottom {
        py - bottom
    } else {
        0.0
    };
    dx.mul_add(dx, dy * dy)
}

/// Squared distance from a root (physical) point to the monitor's physical
/// bounds — zero inside, the gap distance outside.
fn monitor_root_distance_sq(monitor: &gdk4::Monitor, root_x: i32, root_y: i32) -> f64 {
    use gdk4::prelude::*;
    let geometry = monitor.geometry();
    let scale = f64::from(monitor.scale_factor());
    let left = f64::from(geometry.x()) * scale;
    let top = f64::from(geometry.y()) * scale;
    let right = f64::from(geometry.width()).mul_add(scale, left);
    let bottom = f64::from(geometry.height()).mul_add(scale, top);
    rect_distance_sq(
        left,
        top,
        right,
        bottom,
        f64::from(root_x),
        f64::from(root_y),
    )
}

/// The monitor physically nearest a root position that sits in no monitor's
/// bounds — a gap between monitors, or a pointer on another screen.
///
/// # Panics
///
/// Panics when the display reports no monitors — the caller asserts that
/// first, so reaching here with none is a bug.
fn monitor_nearest_root_position(
    display: &gdk4::Display,
    root_x: i32,
    root_y: i32,
) -> gdk4::Monitor {
    use gdk4::prelude::*;
    let monitors = display.monitors();
    (0..monitors.n_items())
        .filter_map(|index| monitors.item(index))
        .filter_map(|item| item.downcast::<gdk4::Monitor>().ok())
        .min_by(|a, b| {
            monitor_root_distance_sq(a, root_x, root_y)
                .total_cmp(&monitor_root_distance_sq(b, root_x, root_y))
        })
        .expect("window placement: GDK display reported no monitors to be nearest to")
}

/// The monitor whose physical bounds contain a global X position.
fn monitor_at_root_position(
    display: &gdk4::Display,
    root_x: i32,
    root_y: i32,
) -> Option<gdk4::Monitor> {
    use gdk4::prelude::*;
    let monitors = display.monitors();
    (0..monitors.n_items()).find_map(|index| {
        let monitor = monitors
            .item(index)
            .and_then(|item| item.downcast::<gdk4::Monitor>().ok())?;
        let geometry = monitor.geometry();
        // Root coordinates are physical; monitor geometry is in application
        // pixels, so the bounds are scaled up before the containment test.
        let scale = f64::from(monitor.scale_factor());
        let left = f64::from(geometry.x()) * scale;
        let top = f64::from(geometry.y()) * scale;
        let right = f64::from(geometry.width()).mul_add(scale, left);
        let bottom = f64::from(geometry.height()).mul_add(scale, top);
        (f64::from(root_x) >= left
            && f64::from(root_x) < right
            && f64::from(root_y) >= top
            && f64::from(root_y) < bottom)
            .then_some(monitor)
    })
}

/// The monitor under the pointer, resolved globally where GDK can.
///
/// On X11 this asks the X server for the root position, so the pointer is
/// tracked over any client's window — not only ours — and every failure of
/// the query itself panics. A pointer in a monitor gap (or on another
/// screen's root) resolves to the nearest monitor by distance. Everywhere
/// else GDK has no global pointer query (Wayland genuinely cannot provide
/// one): the selector then resolves to the first monitor and the limitation
/// is warned, never hidden.
fn pointer_monitor(display: &gdk4::Display) -> Option<gdk4::Monitor> {
    use glib::prelude::*;
    if !is_x11_display(display) {
        let backend = display.type_().name();
        tracing::warn!(
            "window placement: MonitorSelector::Pointer cannot see the global pointer on \
             {backend}; resolving to the first monitor"
        );
        return None;
    }
    let (root_x, root_y) = x11_root_pointer(display);
    Some(
        monitor_at_root_position(display, root_x, root_y)
            .unwrap_or_else(|| monitor_nearest_root_position(display, root_x, root_y)),
    )
}

/// `_NET_WORKAREA` of the current desktop (EWMH) on the X11 backend, in
/// root-window physical pixels.
///
/// `None` is legitimate only where the WM publishes no work-area properties
/// at all — a bare X server — and every failure of a live X11 query panics.
/// The caller checks `is_x11_display` first, so this never sees Wayland.
fn x11_work_area(display: &gdk4::Display) -> Option<(i32, i32, i32, i32)> {
    use std::ffi::{c_int, c_long, c_ulong, c_void};

    type XWindow = c_ulong;
    type XAtom = c_ulong;

    #[link(name = "X11")]
    unsafe extern "C" {
        fn XDefaultRootWindow(display: *mut c_void) -> XWindow;
        fn XInternAtom(
            display: *mut c_void,
            name: *const std::ffi::c_char,
            only_if_exists: c_int,
        ) -> XAtom;
    }

    // Non-X11 backends have no X connection to borrow — Wayland has no
    // work-area protocol, so `None` there is the documented limitation.
    if !is_x11_display(display) {
        return None;
    }
    let xdisplay = x11_xdisplay(display);
    // Safety: `xdisplay`/`root` name live objects owned by GTK.
    let root = unsafe { XDefaultRootWindow(xdisplay) };
    // Safety: `xdisplay` is a live X display; the atom names are static C
    // strings. `only_if_exists = 1` returns atom 0 when the atom is not
    // interned — a WM without EWMH atoms publishes no work areas at all.
    let workarea = unsafe { XInternAtom(xdisplay, c"_NET_WORKAREA".as_ptr(), 1) };
    // Safety: `xdisplay` is a live X display, atom name is a static C string.
    let current_desktop = unsafe { XInternAtom(xdisplay, c"_NET_CURRENT_DESKTOP".as_ptr(), 1) };
    if workarea == 0 || current_desktop == 0 {
        return None;
    }
    // `_NET_CURRENT_DESKTOP` selects which 4-tuple of the flat
    // `_NET_WORKAREA` list to read (four CARDINALs per desktop).
    let desktop = *x11_read_cardinals(xdisplay, root, current_desktop, 1)?.first()?;
    let cardinals = u32::try_from(desktop)
        .expect("desktop index non-negative")
        .checked_add(1)
        .and_then(|n| n.checked_mul(4))
        .expect("desktop index overflows the work-area read");
    let values = x11_read_cardinals(xdisplay, root, workarea, c_long::from(cardinals))?;
    let base = usize::try_from(desktop).expect("desktop index non-negative") * 4;
    let quad = values.get(base..base + 4)?;
    let cardinal_i32 = |value: c_ulong| -> i32 {
        i32::try_from(value).unwrap_or_else(|_| {
            panic!("window placement: _NET_WORKAREA value {value} does not fit i32")
        })
    };
    Some((
        cardinal_i32(quad[0]),
        cardinal_i32(quad[1]),
        cardinal_i32(quad[2]),
        cardinal_i32(quad[3]),
    ))
}

/// A `CARDINAL` (format-32) root-window property, as `long` elements the
/// way Xlib always hands them back. `None` when the property is absent or
/// empty; a failed request panics — the connection is live and the atom
/// was interned, so there is no legitimate failure to fold away.
fn x11_read_cardinals(
    xdisplay: *mut std::ffi::c_void,
    root: std::os::raw::c_ulong,
    atom: std::os::raw::c_ulong,
    long_length: std::os::raw::c_long,
) -> Option<Vec<std::os::raw::c_ulong>> {
    use std::ffi::{c_int, c_uchar, c_ulong, c_void};

    type XAtom = c_ulong;
    const XA_CARDINAL: XAtom = 6;
    /// Xlib's `Success` return for property requests.
    const X_SUCCESS: c_int = 0;

    #[link(name = "X11")]
    unsafe extern "C" {
        #[allow(clippy::too_many_arguments)]
        fn XGetWindowProperty(
            display: *mut c_void,
            window: c_ulong,
            property: XAtom,
            long_offset: std::os::raw::c_long,
            long_length: std::os::raw::c_long,
            delete: c_int,
            req_type: XAtom,
            actual_type_return: *mut XAtom,
            actual_format_return: *mut c_int,
            nitems_return: *mut c_ulong,
            bytes_after_return: *mut c_ulong,
            prop_return: *mut *mut c_uchar,
        ) -> c_int;
        fn XFree(data: *mut c_void) -> c_int;
    }

    let (mut actual_type, mut actual_format) = (0, 0);
    let (mut nitems, mut bytes_after) = (0, 0);
    let mut prop: *mut c_uchar = std::ptr::null_mut();
    // Safety: `xdisplay`/`root` name live X objects; every out-pointer
    // names a live stack slot, and `prop` is `XFree`d exactly once below
    // when it was handed out.
    let status = unsafe {
        XGetWindowProperty(
            xdisplay,
            root,
            atom,
            0,
            long_length,
            0,
            XA_CARDINAL,
            &raw mut actual_type,
            &raw mut actual_format,
            &raw mut nitems,
            &raw mut bytes_after,
            &raw mut prop,
        )
    };
    assert!(
        status == X_SUCCESS,
        "window placement: XGetWindowProperty(atom {atom}) failed with {status}"
    );
    if prop.is_null() || nitems == 0 {
        return None;
    }
    // 32-bit properties arrive as a `c_ulong` array from Xlib; the
    // `*mut c_uchar` out-pointer documents no alignment, so each element
    // is byte-copied out.
    let count = usize::try_from(nitems).expect("nitems is non-negative");
    let values: Vec<c_ulong> = (0..count)
        .map(|i| {
            let mut value = std::mem::MaybeUninit::<c_ulong>::uninit();
            // Safety: `prop` points at `nitems` live `c_ulong`s per
            // Xlib's property contract, checked just above; the copy is
            // u8-typed so alignment cannot matter.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    prop.add(i * std::mem::size_of::<c_ulong>()),
                    value.as_mut_ptr().cast::<c_uchar>(),
                    std::mem::size_of::<c_ulong>(),
                );
                value.assume_init()
            }
        })
        .collect();
    // Safety: `prop` was handed out by XGetWindowProperty and is freed
    // exactly once here.
    unsafe { XFree(prop.cast()) };
    Some(values)
}

/// The monitor's work area in application pixels: `_NET_WORKAREA`
/// intersected with the monitor's physical bounds, converted back by the
/// monitor's scale. `None` where no work area exists — non-X11 backends
/// (Wayland has no work-area protocol) or a WM that publishes none — and
/// `visible_frame` then equals `frame`.
fn x11_visible_frame(
    display: &gdk4::Display,
    monitor: &gdk4::Monitor,
) -> Option<waterui_core::layout::Rect> {
    use gdk4::prelude::*;
    use waterui_core::layout::{Point, Rect, Size};
    let (wx, wy, ww, wh) = x11_work_area(display)?;
    let geometry = monitor.geometry();
    let scale = f64::from(monitor.scale_factor());
    let left = f64::from(geometry.x()) * scale;
    let top = f64::from(geometry.y()) * scale;
    let right = f64::from(geometry.width()).mul_add(scale, left);
    let bottom = f64::from(geometry.height()).mul_add(scale, top);
    let x = f64::from(wx).max(left);
    let y = f64::from(wy).max(top);
    let far_x = (f64::from(wx) + f64::from(ww)).min(right);
    let far_y = (f64::from(wy) + f64::from(wh)).min(bottom);
    if far_x <= x || far_y <= y {
        // The work area reserves nothing on this monitor.
        return Some(Rect::new(
            Point::new(
                monitor_coordinate(geometry.x()),
                monitor_coordinate(geometry.y()),
            ),
            Size::new(
                monitor_coordinate(geometry.width()),
                monitor_coordinate(geometry.height()),
            ),
        ));
    }
    let logical = |physical: f64| -> f32 {
        (physical / scale).to_f32().unwrap_or_else(|| {
            panic!("window placement: work-area coordinate {physical} does not fit f32")
        })
    };
    Some(Rect::new(
        Point::new(logical(x), logical(y)),
        Size::new(logical(far_x - x), logical(far_y - y)),
    ))
}

/// Moves the toplevel to `place`'s origin once it is mapped — X11 only.
///
/// GTK4 gives toplevels no position API on purpose, but on the X11 backend
/// the surface's XID is enough to ask the X server to move the window
/// through the display's own connection — and only after it is mapped,
/// since a pre-map `XMoveWindow` is dropped by the window manager.
fn x11_move_on_map(window: &ApplicationWindow, root_x: i32, root_y: i32) {
    use gdk4::prelude::*;
    use gtk4::prelude::*;
    use std::ffi::c_void;
    use std::os::raw::{c_int, c_ulong};

    type XWindow = c_ulong;

    unsafe extern "C" {
        fn gdk_x11_surface_get_xid(surface: *mut c_void) -> XWindow;
        fn gdk_x11_surface_get_type() -> usize;
    }
    #[link(name = "gobject-2.0")]
    unsafe extern "C" {
        fn g_type_check_instance_is_a(instance: *mut c_void, iface_type: usize) -> c_int;
    }
    #[link(name = "X11")]
    unsafe extern "C" {
        fn XMoveWindow(display: *mut c_void, window: XWindow, x: c_int, y: c_int) -> c_int;
    }

    window.connect_map(move |window| {
        use glib::translate::ToGlibPtr;
        let surface = window
            .surface()
            .unwrap_or_else(|| panic!("window placement: a mapped window has no surface"));
        let surface_ptr: *mut gdk4::ffi::GdkSurface = surface.to_glib_none().0;
        // A mapped toplevel's surface is a `GdkX11Toplevel`, which subclasses
        // `GdkX11Surface` — an exact-name check would reject it.
        // Safety: `surface_ptr` is a live `GdkSurface`; the check reads its
        // class.
        let is_x11 = unsafe {
            g_type_check_instance_is_a(surface_ptr.cast::<c_void>(), gdk_x11_surface_get_type())
        };
        assert!(
            is_x11 != 0,
            "window placement: X11 display gave a non-X11 surface {}",
            surface.type_().name()
        );
        // Safety: `surface` is a live `GdkX11Surface`; the XID it hands back
        // is the toplevel's server-side window id.
        let xid = unsafe { gdk_x11_surface_get_xid(surface_ptr.cast::<c_void>()) };
        assert!(xid != 0, "window placement: GdkX11Surface has no XID");
        let display = gdk4::Display::default()
            .unwrap_or_else(|| panic!("window placement: no display at window map"));
        let xdisplay = x11_xdisplay(&display);
        // Safety: `xdisplay`/`xid` name live X objects for this display's
        // connection.
        unsafe { XMoveWindow(xdisplay, xid, root_x, root_y) };
    });
}

/// Resolves a [`WindowPlacement`]'s monitor selector and runs its `place`
/// closure against the resolved monitor, then applies what GTK can honour.
///
/// What GTK can honour differs per backend:
/// - Size: `set_default_size`, everywhere.
/// - Position: GTK4 gives toplevels no position API — `gtk_window_move` was
///   removed with GTK 3 — but on the X11 backend the surface's XID lets
///   `x11_move_on_map` move the mapped toplevel through the display's own X
///   connection. On Wayland the compositor owns toplevel position
///   absolutely: the position half of `place`'s rect is unhonoured there —
///   a platform limitation, never faked.
///
/// Selector mapping:
/// - `Primary`: the display's first monitor (GDK reports no explicit primary).
/// - `Pointer`: the monitor under the pointer — the X server's global root
///   position on the X11 backend, nearest-monitor on a gap or another
///   screen's root; on other backends (Wayland genuinely cannot expose a
///   global position) it resolves to the first monitor with a warning.
/// - `Focused`: the monitor of the application's active window surface
///   (`Display::monitor_at_surface`), falling back to the first monitor when
///   no window is active.
///
/// `visible_frame`: on X11, `_NET_WORKAREA` of the current desktop
/// intersected with the monitor's frame; on Wayland (or a WM that publishes
/// none — no work-area protocol exists there) it equals `frame`.
///
/// # Panics
///
/// Panics when there is no default display at all, when it reports no
/// monitors, and on any failed X11 query — a display with none cannot
/// carry a toplevel, and a placement that silently misplaces is a bug.
pub fn apply_window_placement(
    window: &ApplicationWindow,
    placement: &waterui::window::WindowPlacement,
    application: &Application,
) {
    use gdk4::prelude::*;
    use gtk4::prelude::*;
    use waterui::window::MonitorSelector;
    use waterui_core::layout::{Point, Rect, Size};

    // Placing a window where there is no display at all is a bug — the app
    // asked for placement it cannot honour — so crash, never silently skip.
    let display = gdk4::Display::default().unwrap_or_else(|| {
        panic!(
            "window placement requested {:?} but GDK has no default display",
            placement.monitor
        )
    });
    let monitors = display.monitors();
    let monitor_at = |index: u32| -> Option<gdk4::Monitor> {
        monitors
            .item(index)
            .and_then(|item| item.downcast::<gdk4::Monitor>().ok())
    };
    // A display with no monitors cannot carry a toplevel at all — placing one
    // on it is a bug, not a fallback case.
    assert!(
        monitors.n_items() > 0,
        "window placement requested {:?} but GDK reports no monitors",
        placement.monitor
    );

    let resolved = match placement.monitor {
        MonitorSelector::Primary => monitor_at(0),
        MonitorSelector::Pointer => pointer_monitor(&display).or_else(|| monitor_at(0)),
        MonitorSelector::Focused => application
            .active_window()
            .and_then(|active| active.surface())
            .and_then(|surface| display.monitor_at_surface(&surface))
            .or_else(|| monitor_at(0)),
    };
    let Some(gdk_monitor) = resolved else {
        panic!(
            "GDK reported monitors but none resolved for {:?}",
            placement.monitor
        );
    };

    let frame = gdk_monitor.geometry();
    // Monitor geometry is in application pixels, small integers where the
    // i32 -> f32 truncation warning cannot lose a real coordinate.
    let logical = Rect::new(
        Point::new(monitor_coordinate(frame.x()), monitor_coordinate(frame.y())),
        Size::new(
            monitor_coordinate(frame.width()),
            monitor_coordinate(frame.height()),
        ),
    );
    let monitor = waterui::window::Monitor {
        frame: logical,
        visible_frame: x11_visible_frame(&display, &gdk_monitor).unwrap_or(logical),
        scale_factor: f64::from(gdk_monitor.scale_factor()),
        name: gdk_monitor
            .connector()
            .map(|connector| waterui_core::Str::from(connector.to_string())),
    };

    let rect = (placement.place)(&monitor);
    window.set_default_size(
        pointer_coordinate(f64::from(rect.width())),
        pointer_coordinate(f64::from(rect.height())),
    );
    if is_x11_display(&display) {
        // The rect's origin is in the monitor's logical space; root
        // coordinates are physical, so the monitor's scale converts them.
        let scale = f64::from(gdk_monitor.scale_factor());
        x11_move_on_map(
            window,
            pointer_coordinate(f64::from(rect.x()) * scale),
            pointer_coordinate(f64::from(rect.y()) * scale),
        );
    }
}

/// Applies `WaterUI` window activation policy to a GTK window.
///
/// GTK4 honours what it can: `Never` maps to `can-focus(false)` +
/// `focus-on-click(false)`, which keeps keyboard focus off the window and
/// stops clicks from raising it. `OnShow` is GTK's default `present()`
/// behaviour. `OnClick` has no GTK equivalent — `present()` always activates
/// the application — so it maps to the same focusable defaults as `OnShow`
/// and the "do not activate on show" half is unhonoured.
pub fn apply_window_activation(
    window: &ApplicationWindow,
    activation: waterui::window::Activation,
) {
    use gtk4::prelude::*;
    use waterui::window::Activation;
    match activation {
        Activation::OnShow | Activation::OnClick => {
            window.set_can_focus(true);
            window.set_focus_on_click(true);
        }
        Activation::Never => {
            window.set_can_focus(false);
            window.set_focus_on_click(false);
        }
    }
}

/// Applies the window's reactive [`WindowStyle`] and keeps applying its
/// changes.
///
/// GTK4 toplevels have one chrome switch, `decorated`: `Borderless` removes
/// the title bar and frame, `Titled` and `FullSizeContentView` keep them —
/// GTK has no content-under-titlebar mode, so the latter renders titled.
pub fn apply_window_style(window: &ApplicationWindow, style: &Binding<WindowStyle>) {
    use gtk4::prelude::*;
    let (initial, guard) = subscribe_then_get(style, {
        let window = window.clone();
        move |ctx| {
            let decorated = style_is_decorated(ctx.into_value());
            let window = window.clone();
            glib::idle_add_local_once(move || window.set_decorated(decorated));
        }
    });
    window.set_decorated(style_is_decorated(initial));
    store_watcher_guard(window, guard);
}

const fn style_is_decorated(style: WindowStyle) -> bool {
    !matches!(style, WindowStyle::Borderless)
}

/// Applies the window's reactive background to a GTK window and keeps
/// applying its changes.
///
/// `resolved` is the window background resolved with
/// [`resolve_background`](waterui::window::resolve_background): the theme background for an opaque window, the declared colour otherwise,
/// following both a switch between the two and a change of the colour. It is
/// painted as the window's CSS background, so a translucent colour reaches
/// the compositor.
pub fn apply_window_background(window: &ApplicationWindow, resolved: &Computed<ResolvedColor>) {
    let css = ScopedCss::attach(
        window,
        "waterui-window-background",
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let (initial, guard) = subscribe_then_get(resolved, {
        let css = css.clone();
        move |ctx| {
            let resolved = ctx.into_value();
            let css = css.clone();
            glib::idle_add_local_once(move || apply_background_css(&css, resolved));
        }
    });
    apply_background_css(&css, initial);
    store_watcher_guard(window, guard);
}

fn apply_background_css(css: &ScopedCss, resolved: ResolvedColor) {
    css.set_declarations(&format!(
        "background-color: {};",
        resolved_color_to_css_rgba(resolved)
    ));
}

#[cfg(test)]
mod placement_x11_tests {
    use super::*;
    use gdk4::prelude::*;
    use glib::MainContext;
    use glib::translate::ToGlibPtr;
    use gtk4::prelude::*;
    use num_traits::ToPrimitive;
    use std::cell::RefCell;
    use std::ffi::{c_int, c_uchar, c_ulong, c_void};
    use std::rc::Rc;
    use std::time::{Duration, Instant};
    use waterui::window::{MonitorSelector, WindowPlacement};
    use waterui_core::layout::{Point, Rect, Size};

    type XWindow = c_ulong;
    type XAtom = c_ulong;

    #[link(name = "X11")]
    unsafe extern "C" {
        fn XOpenDisplay(name: *const std::ffi::c_char) -> *mut c_void;
        fn XDefaultRootWindow(display: *mut c_void) -> XWindow;
        fn XCloseDisplay(display: *mut c_void) -> c_int;
        #[allow(clippy::too_many_arguments)]
        fn XWarpPointer(
            display: *mut c_void,
            src_window: XWindow,
            dest_window: XWindow,
            src_x: c_int,
            src_y: c_int,
            src_width: u32,
            src_height: u32,
            dest_x: c_int,
            dest_y: c_int,
        ) -> c_int;
        fn XFlush(display: *mut c_void) -> c_int;
        fn XInternAtom(
            display: *mut c_void,
            name: *const std::ffi::c_char,
            only_if_exists: c_int,
        ) -> XAtom;
        #[allow(clippy::too_many_arguments)]
        fn XChangeProperty(
            display: *mut c_void,
            window: XWindow,
            property: XAtom,
            property_type: XAtom,
            format: c_int,
            mode: c_int,
            data: *const c_uchar,
            nelements: c_int,
        ) -> c_int;
        #[allow(clippy::too_many_arguments)]
        fn XGetGeometry(
            display: *mut c_void,
            drawable: XWindow,
            root_return: *mut XWindow,
            x_return: *mut c_int,
            y_return: *mut c_int,
            width_return: *mut u32,
            height_return: *mut u32,
            border_width_return: *mut u32,
            depth_return: *mut u32,
        ) -> c_int;
        #[allow(clippy::too_many_arguments)]
        fn XTranslateCoordinates(
            display: *mut c_void,
            src_window: XWindow,
            dest_window: XWindow,
            src_x: c_int,
            src_y: c_int,
            dest_x_return: *mut c_int,
            dest_y_return: *mut c_int,
            child_return: *mut XWindow,
        ) -> c_int;
    }
    unsafe extern "C" {
        fn gdk_x11_surface_get_xid(surface: *mut c_void) -> XWindow;
    }

    const XA_CARDINAL: XAtom = 6;
    const PROP_MODE_REPLACE: c_int = 0;

    /// The tests below are meaningful only on a live X11 display; off X11
    /// they print a SKIP line rather than fake the platform.
    fn x11_display_or_skip() -> Option<gdk4::Display> {
        if gtk4::init().is_err() {
            eprintln!("SKIP: no GTK display");
            return None;
        }
        let display = gdk4::Display::default()?;
        if !is_x11_display(&display) {
            eprintln!("SKIP: display is not X11");
            return None;
        }
        Some(display)
    }

    /// A second, test-owned connection for pointer warps and property
    /// writes — GTK owns the display's connection.
    fn test_xdisplay() -> *mut c_void {
        // Safety: a null name opens $DISPLAY; the result is checked.
        let display = unsafe { XOpenDisplay(std::ptr::null()) };
        assert!(!display.is_null(), "test could not open $DISPLAY");
        display
    }

    fn x_root(display: *mut c_void) -> XWindow {
        // Safety: `display` is a live X display.
        unsafe { XDefaultRootWindow(display) }
    }

    fn warp_pointer(display: *mut c_void, x: i32, y: i32) {
        // Safety: `display` is live; `0` as src means any window, the root
        // window is a valid destination.
        unsafe {
            XWarpPointer(display, 0, x_root(display), 0, 0, 0, 0, x, y);
            XFlush(display);
        }
    }

    /// Drain pending main-context work for `ms` wall-clock ms. Uses only
    /// non-blocking iterations — `iteration(true)` can sleep forever on an
    /// idle context, which is how a bounded wait turns into a hung test.
    fn pump_millis(ms: u64) {
        let context = MainContext::default();
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        while context.pending() {
            context.iteration(false);
        }
    }

    /// Pump until `check` holds or the deadline passes; panics on timeout so
    /// a missing map/configure fails the test instead of hanging it.
    fn pump_until(ms: u64, what: &str, check: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            pump_millis(50);
            if check() {
                return;
            }
        }
        panic!("timed out after {ms}ms waiting for {what}");
    }

    fn gdk_monitors(display: &gdk4::Display) -> Vec<gdk4::Monitor> {
        let monitors = display.monitors();
        (0..monitors.n_items())
            .filter_map(|index| monitors.item(index))
            .filter_map(|item| item.downcast::<gdk4::Monitor>().ok())
            .collect()
    }

    /// A monitor's physical root bounds.
    fn physical_rect(monitor: &gdk4::Monitor) -> (i32, i32, i32, i32) {
        let g = monitor.geometry();
        let s = monitor.scale_factor();
        (g.x() * s, g.y() * s, g.width() * s, g.height() * s)
    }

    /// GDK fills its monitor model asynchronously after the display opens.
    /// Poll (bounded, 5s) until the count holds steady across two polls and
    /// is non-zero, so tests never read a half-initialized model.
    fn settled_monitors(display: &gdk4::Display) -> Vec<gdk4::Monitor> {
        let mut last = usize::MAX;
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let monitors = gdk_monitors(display);
            if !monitors.is_empty() && monitors.len() == last {
                return monitors;
            }
            last = monitors.len();
            assert!(
                Instant::now() < end,
                "GDK monitor list never settled (last count {last})"
            );
            pump_millis(200);
        }
    }

    #[test]
    fn rect_distance_sq_is_zero_inside_and_grows_across_gaps() {
        let near = |got: f64, want: f64| (got - want).abs() < f64::EPSILON;
        assert!(near(rect_distance_sq(0.0, 0.0, 10.0, 10.0, 5.0, 5.0), 0.0));
        assert!(near(
            rect_distance_sq(0.0, 0.0, 10.0, 10.0, 14.0, 5.0),
            16.0
        ));
        assert!(near(rect_distance_sq(0.0, 0.0, 10.0, 10.0, 5.0, 13.0), 9.0));
        assert!(near(
            rect_distance_sq(0.0, 0.0, 10.0, 10.0, 14.0, 13.0),
            25.0
        ));
    }

    #[test]
    fn pointer_selector_tracks_the_global_pointer_not_just_our_surfaces() {
        let Some(display) = x11_display_or_skip() else {
            return;
        };
        let monitors = settled_monitors(&display);
        assert!(!monitors.is_empty());
        let test_dpy = test_xdisplay();
        for monitor in &monitors {
            let (left, top, width, height) = physical_rect(monitor);
            let (px, py) = (left + width / 2, top + height / 2);
            warp_pointer(test_dpy, px, py);
            // Give the X server a beat to publish the warp.
            pump_millis(50);
            let resolved = pointer_monitor(&display)
                .unwrap_or_else(|| panic!("X11 pointer_monitor returned None"));
            let rg = resolved.geometry();
            let g = monitor.geometry();
            println!(
                "EVIDENCE pointer=({px},{py}) resolved={}x{}+{}+{} target={}x{}+{}+{}",
                rg.width(),
                rg.height(),
                rg.x(),
                rg.y(),
                g.width(),
                g.height(),
                g.x(),
                g.y()
            );
            assert_eq!(
                rg, g,
                "pointer at ({px},{py}) should resolve to that monitor"
            );
        }
        // Safety: `test_dpy` is the live display this test opened.
        unsafe { XCloseDisplay(test_dpy) };
    }

    #[test]
    fn pointer_in_a_monitor_gap_resolves_the_nearest_monitor() {
        let Some(display) = x11_display_or_skip() else {
            return;
        };
        let monitors = settled_monitors(&display);
        if monitors.len() < 2 {
            eprintln!("SKIP: only one monitor, no gap possible");
            return;
        }
        let mut rects: Vec<(i32, i32, i32, i32)> = monitors.iter().map(physical_rect).collect();
        rects.sort_by_key(|r| r.0);
        // A horizontal gap between two adjacent monitors.
        let mut gap = None;
        for pair in rects.windows(2) {
            let (lx, ly, lw, lh) = pair[0];
            let (rx, _ry, _rw, _rh) = pair[1];
            if lx + lw < rx {
                // Nudge toward the right-hand monitor so the nearest answer
                // is unambiguous.
                gap = Some((rx - 40, ly + lh / 2, pair[0], pair[1]));
                break;
            }
        }
        let Some((gx, gy, _left, right)) = gap else {
            eprintln!("SKIP: monitors are contiguous, no gap");
            return;
        };
        let test_dpy = test_xdisplay();
        warp_pointer(test_dpy, gx, gy);
        pump_millis(50);
        let resolved = pointer_monitor(&display)
            .unwrap_or_else(|| panic!("X11 pointer_monitor returned None in a gap"));
        let rg = resolved.geometry();
        println!(
            "EVIDENCE gap-pointer=({gx},{gy}) resolved={}x{}+{}+{} right-monitor={}x{}+{}+{}",
            rg.width(),
            rg.height(),
            rg.x(),
            rg.y(),
            right.2,
            right.3,
            right.0,
            right.1
        );
        assert_eq!(rg.x(), right.0, "gap pointer should resolve nearest");
        // Safety: `test_dpy` is the live display this test opened.
        unsafe { XCloseDisplay(test_dpy) };
    }

    /// The toplevel's absolute root position, as `xwininfo` would report it:
    /// `XGetGeometry` plus `XTranslateCoordinates` to the root window, which
    /// also returns the frame's child id under a reparenting WM.
    fn x11_toplevel_root_position(display: *mut c_void, xid: XWindow) -> (i32, i32, XWindow) {
        let (mut root, mut rx, mut ry) = (0, 0, 0);
        let (mut w, mut h, mut bw, mut depth) = (0, 0, 0, 0);
        // Safety: `display`/`xid` name live X objects; every out-pointer
        // names a live stack slot.
        let got = unsafe {
            XGetGeometry(
                display,
                xid,
                &raw mut root,
                &raw mut rx,
                &raw mut ry,
                &raw mut w,
                &raw mut h,
                &raw mut bw,
                &raw mut depth,
            )
        };
        assert!(got != 0, "XGetGeometry failed on toplevel {xid}");
        let (mut abs_x, mut abs_y, mut child) = (0, 0, 0);
        // Safety: `display`/`xid`/`root` name live X objects; out-pointers
        // name live stack slots.
        let translated = unsafe {
            XTranslateCoordinates(
                display,
                xid,
                root,
                0,
                0,
                &raw mut abs_x,
                &raw mut abs_y,
                &raw mut child,
            )
        };
        assert!(translated != 0, "XTranslateCoordinates failed");
        (abs_x, abs_y, child)
    }

    #[test]
    fn toplevel_is_moved_to_the_placed_root_origin_on_x11() {
        let Some(display) = x11_display_or_skip() else {
            return;
        };
        let monitors = settled_monitors(&display);
        assert!(!monitors.is_empty());
        // Put the pointer on the last monitor so `Pointer` resolves there.
        let test_dpy = test_xdisplay();
        {
            let (x, y, w, h) = physical_rect(monitors.last().expect("monitor"));
            warp_pointer(test_dpy, x + w / 2, y + h / 2);
        }
        pump_millis(50);

        let app = gtk4::Application::new(None::<&str>, gtk4::gio::ApplicationFlags::default());
        app.register(None::<&gtk4::gio::Cancellable>)
            .expect("GTK application failed to register");
        let placed = Rc::new(RefCell::new((0.0f64, 0.0f64, 0.0f64)));
        let seen = Rc::new(RefCell::new((0.0f64, 0.0f64, 0.0f64, 0.0f64)));
        let placement = WindowPlacement {
            monitor: MonitorSelector::Pointer,
            place: Rc::new({
                let placed = Rc::clone(&placed);
                let seen = Rc::clone(&seen);
                move |m| {
                    let x = m.frame.x() + 40.0;
                    let y = m.frame.y() + 50.0;
                    *placed.borrow_mut() = (f64::from(x), f64::from(y), m.scale_factor);
                    *seen.borrow_mut() = (
                        f64::from(m.frame.x()),
                        f64::from(m.frame.y()),
                        f64::from(m.frame.width()),
                        f64::from(m.frame.height()),
                    );
                    Rect::new(Point::new(x, y), Size::new(300.0, 200.0))
                }
            }),
        };
        let window = ApplicationWindow::new(&app);
        window.set_child(Some(&gtk4::Label::new(Some("placement probe"))));
        apply_window_placement(&window, &placement, &app);
        window.present();
        // Wait on the real map: the toplevel has a surface once the server
        // (and WM, if any) has mapped it. Fails, never hangs, after 10s.
        pump_until(10_000, "toplevel surface to map", || {
            window.surface().is_some()
        });

        let surface = window
            .surface()
            .unwrap_or_else(|| panic!("presented window has no surface"));
        let surface_ptr: *mut gdk4::ffi::GdkSurface = surface.to_glib_none().0;
        // Safety: the surface is a live GdkX11Surface; the XID names the
        // toplevel on the server.
        let xid = unsafe { gdk_x11_surface_get_xid(surface_ptr.cast::<c_void>()) };
        assert!(xid != 0);
        let (abs_x, abs_y, child) = x11_toplevel_root_position(test_dpy, xid);
        let (px, py, scale) = *placed.borrow();
        let (mx, my, mw, mh) = *seen.borrow();
        let expected_x = (px * scale)
            .round()
            .to_i32()
            .unwrap_or_else(|| panic!("placed x does not fit i32"));
        let expected_y = (py * scale)
            .round()
            .to_i32()
            .unwrap_or_else(|| panic!("placed y does not fit i32"));
        println!(
            "EVIDENCE monitors={} resolved-monitor-frame={mx},{my} {mw}x{mh} \
             placed-origin=({px},{py}) scale={scale} toplevel-xid={xid:#x} \
             root-position=({abs_x},{abs_y}) frame-child={child:#x}",
            monitors.len()
        );
        assert!(
            (abs_x - expected_x).abs() <= 8,
            "toplevel root x {abs_x} should equal placed {expected_x} (+WM border)"
        );
        assert!(
            (abs_y - expected_y).abs() <= 48,
            "toplevel root y {abs_y} should equal placed {expected_y} (+WM frame)"
        );
        // The whole point of the two-monitor run: the position must land
        // inside the monitor the selector resolved.
        let frame_i32 = |v: f64| {
            v.to_i32()
                .unwrap_or_else(|| panic!("monitor frame {v} does not fit i32"))
        };
        let (fx, fy, fw, fh) = (
            frame_i32(mx * scale),
            frame_i32(my * scale),
            frame_i32(mw * scale),
            frame_i32(mh * scale),
        );
        assert!(
            abs_x >= fx && abs_x < fx + fw && abs_y >= fy - 48 && abs_y < fy + fh,
            "toplevel ({abs_x},{abs_y}) outside resolved monitor {fx},{fy} {fw}x{fh}"
        );
        window.close();
        // Safety: `test_dpy` is the live display this test opened.
        unsafe { XCloseDisplay(test_dpy) };
    }

    /// Write a `CARDINAL/32` property — the shape `_NET_WORKAREA` and
    /// `_NET_CURRENT_DESKTOP` take.
    fn x11_write_cardinals(
        display: *mut c_void,
        window: XWindow,
        property: XAtom,
        values: &[c_ulong],
    ) {
        // Safety: `display`/`window` are live X objects; the data pointer
        // outlives the call, which copies it into the property.
        unsafe {
            XChangeProperty(
                display,
                window,
                property,
                XA_CARDINAL,
                32,
                PROP_MODE_REPLACE,
                values.as_ptr().cast::<c_uchar>(),
                i32::try_from(values.len()).expect("cardinal count fits"),
            );
        }
    }

    #[test]
    fn visible_frame_intersects_net_workarea_on_x11() {
        let Some(display) = x11_display_or_skip() else {
            return;
        };
        let test_dpy = test_xdisplay();
        let root = x_root(test_dpy);
        // Safety: `test_dpy`/`root` are live; atom names are static C
        // strings; `only_if_exists = 0` creates atoms that do not exist yet
        // (a bare Xvfb has no WM to publish them).
        let workarea_atom = unsafe { XInternAtom(test_dpy, c"_NET_WORKAREA".as_ptr(), 0) };
        // Safety: same contract as above.
        let desktop_atom = unsafe { XInternAtom(test_dpy, c"_NET_CURRENT_DESKTOP".as_ptr(), 0) };
        assert!(workarea_atom != 0 && desktop_atom != 0);
        // Save whatever is there so it can be restored afterwards.
        let saved_desktop = x11_read_cardinals(test_dpy.cast(), root, desktop_atom, 1);
        let saved_workarea = x11_read_cardinals(test_dpy.cast(), root, workarea_atom, 4096);

        let monitors = settled_monitors(&display);
        let monitor = monitors.first().expect("no monitor");
        let (fx, fy, fw, fh) = physical_rect(monitor);
        let scale = f64::from(monitor.scale_factor());
        // A work area strictly smaller than the monitor's frame.
        let wa = [fx + 40, fy + 30, fw - 80, fh - 60];
        // Four desktops' worth of quads so any _NET_CURRENT_DESKTOP is
        // covered; a real WM writes the same quad per desktop here anyway.
        let mut quads: Vec<c_ulong> = Vec::new();
        for _ in 0..4 {
            quads.extend(
                wa.iter()
                    .map(|&v| c_ulong::try_from(v).expect("workarea value is non-negative")),
            );
        }
        x11_write_cardinals(test_dpy, root, desktop_atom, &[0]);
        x11_write_cardinals(test_dpy, root, workarea_atom, &quads);
        // Safety: `test_dpy` is live.
        unsafe { XFlush(test_dpy) };
        pump_millis(50);

        let visible = x11_visible_frame(&display, monitor)
            .expect("_NET_WORKAREA is set and must produce a frame");
        let to_logical = |physical: f64| -> f32 {
            (physical / scale)
                .to_f32()
                .unwrap_or_else(|| panic!("coordinate {physical} does not fit f32"))
        };
        let expected = Rect::new(
            Point::new(
                to_logical(f64::from(fx) + 40.0),
                to_logical(f64::from(fy) + 30.0),
            ),
            Size::new(
                to_logical(f64::from(fw) - 80.0),
                to_logical(f64::from(fh) - 60.0),
            ),
        );
        println!(
            "EVIDENCE workarea={:?} monitor-frame={}x{}+{}+{} visible_frame={}x{}+{}+{}",
            wa,
            fw,
            fh,
            fx,
            fy,
            visible.width(),
            visible.height(),
            visible.x(),
            visible.y()
        );
        assert!((visible.x() - expected.x()).abs() < 1.0);
        assert!((visible.y() - expected.y()).abs() < 1.0);
        assert!((visible.width() - expected.width()).abs() < 1.0);
        assert!((visible.height() - expected.height()).abs() < 1.0);

        // Restore whatever the WM published before this test.
        if let Some(desktop) = saved_desktop {
            x11_write_cardinals(test_dpy, root, desktop_atom, &desktop);
        }
        if let Some(workarea) = saved_workarea {
            x11_write_cardinals(test_dpy, root, workarea_atom, &workarea);
        }
        // Safety: `test_dpy` is the live display this test opened.
        unsafe {
            XFlush(test_dpy);
            XCloseDisplay(test_dpy);
        }
    }
}

/// Binds `Window::state` onto a GTK window.
///
/// Writes to the binding map onto the corresponding `GtkWindow` calls — each
/// entered state unwinds the ones it is leaving — and compositor-driven
/// transitions write the binding back: `maximized` and `fullscreened`
/// property notifications, plus `GdkToplevelState::MINIMIZED` through the
/// toplevel's `state` notify.
///
/// `Window::level`, `Window::attention` and `Window::resize_increments` have
/// no GTK4 path to bind: GTK4 removed GTK3's `gtk_window_set_keep_above`,
/// `gtk_window_set_urgency_hint` and `gtk_window_set_geometry_hints` —
/// keep-above, demand-attention and resize increments are window-manager
/// hints GTK4 deliberately no longer exposes, so all three are ignored here.
/// (`install_attention_settle` still honors the binding's write-back half.)
pub fn install_window_state(window: &gtk4::Window, state: &Binding<WindowState>) {
    // The state's own transitions notify the write-back handlers below; the
    // flag keeps a programmatic apply from being read as a compositor move.
    let applying = Rc::new(Cell::new(false));
    let apply = {
        let window = window.clone();
        let applying = Rc::clone(&applying);
        move |next: WindowState| {
            applying.set(true);
            apply_window_state(&window, next);
            applying.set(false);
        }
    };

    let (initial, guard) = subscribe_then_get(state, {
        let apply = apply.clone();
        move |ctx| {
            let next = ctx.into_value();
            let apply = apply.clone();
            glib::idle_add_local_once(move || apply(next));
        }
    });
    apply(initial);

    window.connect_maximized_notify({
        let state = state.clone();
        let applying = Rc::clone(&applying);
        move |window| {
            // Only Normal and Maximized are written back: an unmaximize also
            // precedes a fullscreen or minimize transition, and that window
            // keeps its own state rather than being clobbered to Normal.
            if applying.get() {
                return;
            }
            let current = state.snapshot();
            let next = if window.is_maximized() {
                WindowState::Maximized
            } else {
                WindowState::Normal
            };
            if matches!(current, WindowState::Normal | WindowState::Maximized) && current != next {
                state.set(next);
            }
        }
    });

    window.connect_fullscreened_notify({
        let state = state.clone();
        let applying = Rc::clone(&applying);
        move |window| {
            if applying.get() {
                return;
            }
            let current = state.snapshot();
            let next = if window.is_fullscreen() {
                WindowState::Fullscreen
            } else if window.is_maximized() {
                WindowState::Maximized
            } else {
                WindowState::Normal
            };
            if matches!(
                current,
                WindowState::Normal | WindowState::Maximized | WindowState::Fullscreen
            ) && current != next
            {
                state.set(next);
            }
        }
    });

    // The minimized flag lives on the toplevel's `state`, which only exists
    // once the window has a surface, so the notify is wired when it lands.
    window.connect_notify_local(Some("surface"), {
        let state = state.clone();
        let applying = Rc::clone(&applying);
        move |window, _| {
            use gtk4::prelude::NativeExt as _;
            let Some(toplevel) = window
                .surface()
                .and_then(|surface| surface.downcast::<gdk4::Toplevel>().ok())
            else {
                return;
            };
            let state = state.clone();
            let applying = Rc::clone(&applying);
            let window = window.clone();
            toplevel.connect_state_notify(move |toplevel| {
                if applying.get() {
                    return;
                }
                let minimized = toplevel.state().contains(gdk4::ToplevelState::MINIMIZED);
                let current = state.snapshot();
                if minimized {
                    if current != WindowState::Minimized {
                        state.set(WindowState::Minimized);
                    }
                } else if current == WindowState::Minimized {
                    let next = if window.is_fullscreen() {
                        WindowState::Fullscreen
                    } else if window.is_maximized() {
                        WindowState::Maximized
                    } else {
                        WindowState::Normal
                    };
                    state.set(next);
                }
            });
        }
    });

    store_watcher_guard(window, guard);
}

fn apply_window_state(window: &gtk4::Window, state: WindowState) {
    match state {
        WindowState::Normal => {
            window.unmaximize();
            window.unfullscreen();
            window.unminimize();
            window.present();
        }
        WindowState::Closed => window.close(),
        WindowState::Minimized => {
            window.unfullscreen();
            window.minimize();
        }
        WindowState::Maximized => {
            window.unfullscreen();
            window.unminimize();
            window.present();
            window.maximize();
        }
        WindowState::Fullscreen => {
            window.unminimize();
            window.present();
            window.fullscreen();
        }
    }
}

/// Settles `Window::attention` back to `None` once the window gains focus.
///
/// GTK4 exposes no way to *raise* the request — the urgency hint was removed
/// with the other X11 window-manager hints — so this is only the contract's
/// write-back half: a pending request resolves the moment the window is
/// active.
pub fn install_attention_settle(
    window: &gtk4::Window,
    attention: &Binding<Option<waterui::window::UserAttention>>,
) {
    window.connect_is_active_notify({
        let attention = attention.clone();
        move |window| {
            if window.is_active() && attention.snapshot().is_some() {
                attention.set(None);
            }
        }
    });
}

#[cfg(test)]
mod window_state_tests {
    use std::time::{Duration, Instant};

    use glib::MainContext;
    use gtk4::prelude::{NativeExt as _, WidgetExt as _};
    use nami::binding;

    use super::*;

    fn init() {
        gtk4::init().expect("GTK tests need a display; run them under xvfb-run");
    }

    /// Pumps the default main context until `condition` holds; window-state
    /// changes round-trip through the compositor, so assertions poll.
    fn wait_until(mut condition: impl FnMut() -> bool) {
        let context = MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for GTK state");
            context.iteration(true);
        }
    }

    fn minimized(window: &gtk4::Window) -> bool {
        window
            .surface()
            .and_then(|surface| surface.downcast::<gdk4::Toplevel>().ok())
            .is_some_and(|toplevel| toplevel.state().contains(gdk4::ToplevelState::MINIMIZED))
    }

    /// Presents a window wired to `state` and waits until it is mapped, which
    /// is when the toplevel (and its `state` notify) exists.
    fn present(state: &Binding<WindowState>) -> gtk4::Window {
        let window = gtk4::Window::new();
        install_window_state(&window, state);
        window.present();
        wait_until(|| window.is_mapped());
        window
    }

    #[test]
    fn restores_normal_from_fullscreen() {
        init();
        let state = binding(WindowState::Normal);
        let window = present(&state);
        state.set(WindowState::Fullscreen);
        wait_until(|| window.is_fullscreen());
        state.set(WindowState::Normal);
        wait_until(|| !window.is_fullscreen() && !window.is_maximized() && !minimized(&window));
        assert_eq!(state.snapshot(), WindowState::Normal);
    }

    #[test]
    fn restores_normal_from_minimized() {
        init();
        let state = binding(WindowState::Normal);
        let window = present(&state);
        state.set(WindowState::Minimized);
        wait_until(|| minimized(&window));
        state.set(WindowState::Normal);
        wait_until(|| window.is_mapped() && !minimized(&window));
        assert_eq!(state.snapshot(), WindowState::Normal);
    }
}
