//! GTK4 `AnchoredOverlay` metadata implementation.
//!
//! `GtkPopover` cannot honour the contract's edge alignment, exact `gap` or
//! window-margin clamp on its own — GTK computes those itself — so the frame
//! comes from `waterui_backend_core::overlay::place_anchored_overlay` and the
//! popover is anchored to a zero-size `pointing_to` rectangle that makes GTK
//! land the popover content on the computed frame: the rect sits on the
//! frame's far-edge midpoint, and `set_position` names the resolved edge, so
//! the popover's near edge attaches exactly where the contract put it.
//!
//! Because `pointing_to` is expressed in anchor coordinates, GTK keeps the
//! overlay attached when the anchor moves or the window resizes; GTK's own
//! constraint solving then re-clamps the popover against the new window. The
//! anchor's `unmap` (it left the tree) closes the overlay and writes `false`
//! into the binding.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Orientation, Popover, PositionType, Widget};
use nami::Signal;
use waterui::metadata::anchored_overlay::{AnchorPlacement, AnchoredOverlay, Dismissal};
use waterui_backend_core::overlay::{PhysicalEdge, place_anchored_overlay};
use waterui_core::layout::{Point, Rect, Size, layout_direction};
use waterui_core::{Binding, Environment, Metadata};

use crate::renderer::GtkRenderer;
use crate::util::{store_watcher_guard, subscribe_then_get};

/// Renders `Metadata<AnchoredOverlay>`: the anchor view plus a `GtkPopover`
/// that presents the overlay content while `is_presented` holds.
pub(crate) fn render_anchored_overlay(
    renderer: &mut GtkRenderer,
    metadata: Metadata<AnchoredOverlay>,
    env: &Environment,
) -> Widget {
    let anchor = renderer.render_any(metadata.content, env);
    anchor.set_can_target(true);

    let AnchoredOverlay {
        content,
        is_presented,
        placement,
        dismissal,
    } = metadata.value;
    let overlay = renderer.render_any(content, env);

    let popover_state: Rc<RefCell<Option<Popover>>> = Rc::new(RefCell::new(None));

    let show = {
        let anchor = anchor.clone();
        let is_presented = is_presented.clone();
        let popover_state = popover_state.clone();
        let env = env.clone();
        move || {
            show_anchored_overlay(
                &anchor,
                &overlay,
                &is_presented,
                placement,
                dismissal,
                &env,
                &popover_state,
            );
        }
    };

    let dismiss = {
        let popover_state = popover_state.clone();
        move || {
            if let Some(popover) = popover_state.borrow_mut().take() {
                popover.popdown();
            }
        }
    };

    // The anchor left the tree: close the overlay and report it through the
    // binding.
    anchor.connect_unmap({
        let dismiss = dismiss.clone();
        let is_presented = is_presented.clone();
        move |_| {
            dismiss();
            if is_presented.snapshot() {
                is_presented.set(false);
            }
        }
    });

    let initial_show = show.clone();
    let (initial, guard) = subscribe_then_get(&is_presented, move |ctx| {
        if ctx.into_value() {
            show();
        } else {
            dismiss();
        }
    });
    store_watcher_guard(&anchor, guard);
    if initial {
        // The binding already holds: present once the anchor has an
        // allocation and a native to measure against.
        glib::idle_add_local_once(move || {
            initial_show();
        });
    }

    anchor
}

/// Builds (once) and pops up the overlay popover against `anchor`.
#[allow(clippy::too_many_arguments)]
fn show_anchored_overlay(
    anchor: &Widget,
    overlay: &Widget,
    is_presented: &Binding<bool>,
    placement: AnchorPlacement,
    dismissal: Dismissal,
    env: &Environment,
    popover_state: &Rc<RefCell<Option<Popover>>>,
) {
    if popover_state.borrow().is_some() {
        return;
    }
    let Some(native) = anchor.native() else {
        return;
    };
    let native = native.upcast::<Widget>();
    let window_width = native.width() as f32;
    let window_height = native.height() as f32;
    if window_width <= 0.0 || window_height <= 0.0 {
        return;
    }

    // Contract step 1: the content's ideal size capped by the window.
    let (_, natural_width, ..) = overlay.measure(Orientation::Horizontal, -1);
    let (_, natural_height, ..) = overlay.measure(
        Orientation::Vertical,
        natural_width.min(window_width as i32),
    );
    let overlay_size = Size::new(
        (natural_width as f32).min(window_width),
        (natural_height as f32).min(window_height),
    );

    let Some(anchor_origin) = anchor.compute_point(&native, &gtk4::graphene::Point::zero()) else {
        return;
    };
    let anchor_frame = Rect::new(
        Point::new(anchor_origin.x(), anchor_origin.y()),
        Size::new(anchor.width() as f32, anchor.height() as f32),
    );
    let window_frame = Rect::from_size(Size::new(window_width, window_height));

    let direction = layout_direction(env).snapshot();
    let placed = place_anchored_overlay(
        anchor_frame,
        window_frame,
        overlay_size,
        placement,
        direction,
    );
    let frame = placed.frame;

    // GTK attaches the popover's near edge to `pointing_to` and centres it on
    // the rect along that edge, so the rect is the frame's far-edge midpoint.
    // `placed.edge` is already physical — Leading/Trailing resolved — so it
    // maps straight onto the GTK position.
    let (position, attach_x, attach_y) = match placed.edge {
        PhysicalEdge::Top => (PositionType::Top, frame.mid_x(), frame.max_y()),
        PhysicalEdge::Bottom => (PositionType::Bottom, frame.mid_x(), frame.y()),
        PhysicalEdge::Left => (PositionType::Left, frame.max_x(), frame.mid_y()),
        PhysicalEdge::Right => (PositionType::Right, frame.x(), frame.mid_y()),
    };
    let Some(pointing) =
        native.compute_point(anchor, &gtk4::graphene::Point::new(attach_x, attach_y))
    else {
        return;
    };

    let popover = Popover::new();
    popover.set_has_arrow(false);
    popover.set_position(position);
    popover.set_autohide(matches!(dismissal, Dismissal::OutsideInteraction));
    popover.set_child(Some(overlay));
    popover.set_parent(anchor);
    popover.set_pointing_to(Some(&gdk4::Rectangle::new(
        pointing.x().round() as i32,
        pointing.y().round() as i32,
        0,
        0,
    )));

    // GTK's `autohide` dismisses on an outside press while the press still
    // reaches its target; the `closed` signal reports either path, so a
    // binding-driven popdown is idempotent with it.
    popover.connect_closed({
        let is_presented = is_presented.clone();
        move |_| {
            if is_presented.snapshot() {
                is_presented.set(false);
            }
        }
    });

    popover.popup();
    *popover_state.borrow_mut() = Some(popover);
}
